//! A Workflow step can await exactly the native Session created by its launch Task.
use super::{ACTION, PreparedStep, RUN};
use crate::{
    Result, agent, artifact,
    db::{self, Db, Doc},
    decode, err,
    policy::{self, deadline::Clock},
    require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::storage::cloning::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("session_join.sql");
const BINDING: &str = "workflow_join";
const MAX_LAUNCH_RECEIPT: usize = 4096;

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Completion {
    SessionTerminal,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Binding {
    run: String,
    step: String,
    step_key: String,
    deadline_at: String,
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "workflow session join lost its original launch",
    )
}

pub(super) fn validate(db: &Db<'_>, tool: &str, input: &Value) -> Result<()> {
    require(
        tool == agent::START_SESSION,
        "InvalidArgument",
        "session completion is supported only on an Agent session launch step",
    )?;
    let blueprint: agent::SessionBlueprint = decode(input)?;
    require(
        blueprint.fallback_models(db)?.is_empty()
            && agent::inference::strict(&blueprint.model(db)?),
        "UnsupportedCapability",
        "joined sessions require one strict model without fallback",
    )?;
    let skill = blueprint.skill(db)?;
    require(
        [agent::START_SESSION, super::START_TOOL]
            .into_iter()
            .all(|tool| skill.spec["tool_refs"].get(tool).is_none()),
        "UnsupportedCapability",
        "joined sessions cannot launch nested Agent sessions or workflows",
    )
}

fn step(run: &Doc, key: &str) -> Result<PreparedStep> {
    let steps: Vec<PreparedStep> = decode(&run.spec["steps"])?;
    steps
        .into_iter()
        .find(|step| step.step_key == key)
        .ok_or_else(invalid)
}

fn from_task(run: &Doc, task: &Doc) -> Result<Option<Binding>> {
    let origin = &task.body["origin_ref"];
    require(
        run.kind == RUN
            && task.kind == "Task"
            && run.id == origin["origin_id"]
            && origin["origin_type"] == RUN
            && run.owner == task.owner
            && task.spec["task_type"] == agent::START_SESSION
            && task.spec["delegation_ref"]["resource_id"] == run.spec["delegation_id"],
        "RecoveryRequired",
        "workflow session launch has no matching origin or authority",
    )?;
    let key = origin["step_key"].as_str().ok_or_else(invalid)?;
    let step = step(run, key)?;
    if step.completion != Some(Completion::SessionTerminal) {
        return Ok(None);
    }
    require(
        step.request["task_type"] == agent::START_SESSION
            && task.spec["deadline_at"] == run.spec["deadline_at"]
            && task.spec["request_digest"] == step.request["request_digest"]
            && *origin == step.request["origin_ref"]
            && run.spec["dsl_version"] == 4,
        "RecoveryRequired",
        "workflow session join does not match the frozen launch step",
    )?;
    Ok(Some(Binding {
        run: run.id.clone(),
        step: step.id,
        step_key: key.into(),
        deadline_at: run.spec["deadline_at"].as_str().ok_or_else(invalid)?.into(),
    }))
}

fn authorized(db: &Db<'_>, run: &Doc) -> Result<()> {
    policy::active(db, &run.owner)?;
    let delegation = run.spec["delegation_id"].as_str().ok_or_else(invalid)?;
    policy::delegation(db, &run.owner, delegation, ACTION)?;
    policy::delegation(db, &run.owner, delegation, "task.read")?;
    Ok(())
}

fn require_active(db: &Db<'_>, run: &Doc) -> Result<()> {
    let active: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_workflows WHERE run=?1 AND stopping=0)",
        [&run.id],
        |row| row.get(0),
    )?;
    require(active, "Fenced", "owning workflow is stopping or closed")
}

pub(crate) fn launch(db: &Db<'_>, task: &Doc) -> Result<Option<Binding>> {
    let origin = &task.body["origin_ref"];
    if origin["origin_type"] != RUN {
        return Ok(None);
    }
    let run = db.of(origin["origin_id"].as_str().ok_or_else(invalid)?, RUN)?;
    if run.spec["dsl_version"] != 4 {
        return Ok(None);
    }
    let Some(binding) = from_task(&run, task)? else {
        return Ok(None);
    };
    require_active(db, &run)?;
    authorized(db, &run)?;
    let expected_task: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_workflow_tasks WHERE run=?1 AND step=?2 AND task=?3)",
        params![run.id, binding.step, task.id],
        |row| row.get(0),
    )?;
    require(
        expected_task && task.body["outcome_knowledge"] == "InFlight",
        "Fenced",
        "workflow launch is not owned by the active step",
    )?;
    require(
        !Clock::sample()?.expired(db, &binding.run, &binding.deadline_at)?,
        "DeadlineExceeded",
        "owning workflow deadline passed",
    )?;
    Ok(Some(binding))
}

pub(crate) fn seal_launch(db: &Db<'_>, binding: &Binding, task: &Doc, session: &Doc) -> Result<()> {
    Clock::sample()?.register_until(db, &session.id, &binding.deadline_at, Some(&binding.run))?;
    db.c.execute(
        "INSERT INTO workflow_session_joins VALUES(?1,?2,?3,?4,?5)",
        params![
            task.id,
            binding.run,
            binding.step,
            session.id,
            session.reference()
        ],
    )?;
    Ok(())
}

pub(crate) fn stamp(spec: &mut Value, binding: &Binding) -> Result<()> {
    spec[BINDING] = serde_json::to_value(binding)?;
    Ok(())
}

pub(crate) fn is_bound(session: &Doc) -> bool {
    session.spec.get(BINDING).is_some()
}

pub(crate) fn execution(db: &Db<'_>, session: &Doc, clock: &Clock) -> Result<Option<String>> {
    let Some(value) = session.spec.get(BINDING) else {
        return Ok(None);
    };
    let binding: Binding = decode(value)?;
    let run = db.of(&binding.run, RUN)?;
    let owned: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM workflow_session_joins j
          JOIN active_workflow_tasks a ON a.task=j.launch_task AND a.run=j.run AND a.step=j.step
          WHERE j.session=?1 AND j.run=?2 AND j.step=?3 AND j.launch_task=?4)",
        params![
            session.id,
            run.id,
            binding.step,
            session.spec["origin_task_ref"]["resource_id"].as_str()
        ],
        |row| row.get(0),
    )?;
    require(
        session.owner == run.owner
            && run.spec["deadline_at"] == binding.deadline_at
            && run.spec["delegation_id"] == session.spec["delegation_id"],
        "RecoveryRequired",
        "joined session disagrees with its workflow authority",
    )?;
    require(
        !clock.expired(db, &session.id, &binding.deadline_at)?
            && !clock.expired(db, &run.id, &binding.deadline_at)?,
        "DeadlineExceeded",
        "owning workflow deadline passed",
    )?;
    require_active(db, &run)?;
    require(
        owned,
        "RecoveryRequired",
        "active workflow lost its session join",
    )?;
    authorized(db, &run)?;
    Ok(Some(binding.deadline_at))
}

struct Joined {
    opened: Doc,
    current: Doc,
}

fn joined(conn: &Connection, run: &Doc, task: &Doc) -> Result<Option<Joined>> {
    let row: Option<(String, String, String, Value)> = conn
        .query_row(
            "SELECT run,step,session,opened_ref FROM workflow_session_joins WHERE launch_task=?1",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let binding = from_task(run, task)?;
    let Some((run_id, step, session, reference)) = row else {
        require(
            binding.is_none() || task.body["lifecycle"] != "Succeeded",
            "RecoveryRequired",
            "successful workflow launch lost its original session join",
        )?;
        return Ok(None);
    };
    let binding = binding.ok_or_else(invalid)?;
    let opened = db::resolve(conn, &reference)?;
    let current = db::find(conn, &session)?.ok_or_else(invalid)?;
    let origin = db::resolve(conn, &opened.spec["origin_task_ref"])?;
    require(
        run_id == binding.run
            && step == binding.step
            && opened.kind == "Session"
            && opened.id == session
            && opened.rev == 1
            && opened.owner == task.owner
            && opened.body["lifecycle"] == "AwaitingModel"
            && opened.spec[BINDING] == serde_json::to_value(&binding)?
            && opened.spec["delegation_id"] == run.spec["delegation_id"]
            && opened.body["delegation_ref"] == task.spec["delegation_ref"]
            && current.spec == opened.spec
            && current.kind == opened.kind
            && current.owner == opened.owner
            && origin.id == task.id
            && origin.kind == "Task"
            && origin.spec == task.spec
            && origin.body["outcome_knowledge"] == "InFlight",
        "RecoveryRequired",
        "joined session does not match its original workflow launch",
    )?;
    Ok(Some(Joined { opened, current }))
}

pub(crate) fn verify_launch_spec(
    conn: &Connection,
    task: &Doc,
    opened: &Doc,
    expected: &mut Value,
) -> Result<()> {
    if !is_bound(opened) {
        return Ok(());
    }
    let run_id = task.body["origin_ref"]["origin_id"]
        .as_str()
        .ok_or_else(invalid)?;
    let run = db::find(conn, run_id)?.ok_or_else(invalid)?;
    let binding = from_task(&run, task)?.ok_or_else(invalid)?;
    let join = joined(conn, &run, task)?.ok_or_else(invalid)?;
    require(
        join.opened.reference() == opened.reference(),
        "RecoveryRequired",
        "private inputs do not belong to the workflow's launched session",
    )?;
    stamp(expected, &binding)
}

pub(super) fn stopped_launch(db: &Db<'_>, run: &Doc, task: &Doc) -> Result<()> {
    if run.spec["dsl_version"] == 4
        && task.spec["task_type"] == agent::START_SESSION
        && let Some(join) = joined(db.c, run, task)?
    {
        agent::cancel(
            db,
            &run.owner,
            &json!({
                "id":join.current.id,"expected_resource_version":join.current.version()
            }),
        )?;
    }
    Ok(())
}

fn launch_receipt(conn: &Connection, task: &Doc, opened: &Doc) -> Result<Doc> {
    let references = task.body["result"]["artifact_refs"]
        .as_array()
        .ok_or_else(invalid)?;
    let [reference] = references.as_slice() else {
        return Err(invalid());
    };
    let receipt = db::resolve(conn, reference)?;
    require(
        task.body["lifecycle"] == "Succeeded"
            && task.body["outcome_knowledge"] == "Known"
            && artifact::is_task_output(&receipt, task)
            && task.body["operation_refs"][0]["resource_id"] == receipt.spec["operation_id"],
        "RecoveryRequired",
        "workflow session receipt is not from its settled launch Task",
    )?;
    let payload: Value = serde_json::from_slice(&artifact::checkpoint_bytes(
        conn,
        &receipt,
        MAX_LAUNCH_RECEIPT,
    )?)?;
    require(
        payload == json!({"session_ref":opened.reference()}),
        "RecoveryRequired",
        "workflow launch receipt points to another session",
    )?;
    Ok(receipt)
}

fn unresolved(conn: &Connection, session: &Doc) -> Result<(bool, bool)> {
    let missing_tool: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_turns t WHERE t.session=?1 AND t.task IS NOT NULL
           AND (t.receipt->>'task_id' IS NOT t.task OR NOT EXISTS(
              SELECT 1 FROM session_tasks j WHERE j.session=t.session AND j.turn=t.turn
                AND j.attempt=1 AND j.task=t.task)))",
        [&session.id],
        |row| row.get(0),
    )?;
    require(
        !missing_tool,
        "RecoveryRequired",
        "workflow session lost its original tool Task",
    )?;
    conn.query_row(
        "SELECT
           EXISTS(SELECT 1 FROM session_turns t JOIN model_inference_leases i USING(lease)
             WHERE t.session=?1 AND i.status='Unknown')
           OR EXISTS(SELECT 1 FROM session_tasks j JOIN current t ON t.id=j.task
             WHERE j.session=?1 AND t.body->>'outcome_knowledge'='Unknown'),
           EXISTS(SELECT 1 FROM session_turns t LEFT JOIN model_inference_leases i USING(lease)
             WHERE t.session=?1 AND (i.status='Pending' OR i.lease IS NULL))
           OR EXISTS(SELECT 1 FROM session_tasks j LEFT JOIN current t ON t.id=j.task
             WHERE j.session=?1 AND (t.id IS NULL OR t.kind<>'Task' OR t.owner<>?2
              OR coalesce(t.body->>'lifecycle','') NOT IN ('Succeeded','Failed','Cancelled')
              OR coalesce(t.body->>'outcome_knowledge','') NOT IN ('Known','NotStarted')))",
        params![session.id, session.owner],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(Into::into)
}

fn successful(conn: &Connection, session: &Doc) -> Result<()> {
    let receipt: Option<Value> = conn
        .query_row(
            "SELECT t.receipt FROM session_turns t JOIN model_inference_leases i USING(lease)
         WHERE t.session=?1 AND t.turn=?2 AND i.status='Responded' AND t.task IS NULL",
            params![session.id, session.body["turn"].as_i64()],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    require(
        receipt.is_some_and(|value| {
            value["session_ref"] == session.reference() && value["lifecycle"] == "Succeeded"
        }),
        "RecoveryRequired",
        "workflow session success lacks its exact strict final decision",
    )
}

pub(super) struct Observation {
    pub(super) terminal: bool,
    pub(super) failure: Option<&'static str>,
    pub(super) session_revision: i64,
}

pub(super) fn observe(
    db: &Db<'_>,
    run: &Doc,
    task: &Doc,
    step: &mut Value,
) -> Result<Option<Observation>> {
    if run.spec["dsl_version"] != 4
        || task.spec["task_type"] != agent::START_SESSION
        || task.body["lifecycle"] != "Succeeded"
    {
        return Ok(None);
    }
    let Some(join) = joined(db.c, run, task)? else {
        return Ok(None);
    };
    let receipt = launch_receipt(db.c, task, &join.opened)?;
    let session = &join.current;
    let (unknown, unsettled) = unresolved(db.c, session)?;
    let lifecycle = session.body["lifecycle"].as_str().ok_or_else(invalid)?;
    step["session_ref"] = session.reference();
    step["session_lifecycle"] = json!(lifecycle);
    step["launch_artifact_ref"] = receipt.reference();
    let terminal =
        !unknown && !unsettled && matches!(lifecycle, "Failed" | "Cancelled" | "Succeeded");
    let mut failure = None;
    let (phase, knowledge, reason) = if unknown {
        ("Blocked", "Unknown", Some("OutcomeUnknown"))
    } else if terminal {
        match lifecycle {
            "Succeeded" => {
                successful(db.c, session)?;
                if let Err(error) = authorized(db, run) {
                    if error.0.code != "PermissionDenied" {
                        return Err(error);
                    }
                    failure = Some("PermissionDenied");
                }
            }
            "Failed" => failure = Some("SessionFailed"),
            "Cancelled" => failure = Some("SessionCancelled"),
            _ => unreachable!(),
        }
        (
            if failure == Some("PermissionDenied") {
                "Failed"
            } else {
                lifecycle
            },
            "Known",
            failure,
        )
    } else {
        (
            "Running",
            "InFlight",
            (lifecycle == "Paused").then_some("SessionPaused"),
        )
    };
    step["lifecycle"] = json!(phase);
    step["outcome_knowledge"] = json!(knowledge);
    if let Some(reason) = reason {
        step["reason"] = json!(reason);
    } else {
        step.as_object_mut().ok_or_else(invalid)?.remove("reason");
    }
    Ok(Some(Observation {
        terminal,
        failure,
        session_revision: session.rev,
    }))
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current s WHERE s.kind='Session'
         AND s.spec->'workflow_join' IS NOT NULL
         AND NOT EXISTS(SELECT 1 FROM workflow_session_joins j WHERE j.session=s.id))
         OR EXISTS(SELECT 1 FROM active_workflow_tasks a WHERE a.session_rev IS NOT NULL
             AND NOT EXISTS(SELECT 1 FROM workflow_session_joins j
               WHERE j.launch_task=a.task AND j.run=a.run AND j.step=a.step))
         OR EXISTS(SELECT 1 FROM current w, json_each(w.body->'steps') step
             WHERE w.kind='WorkflowRun' AND w.spec->>'dsl_version'=4
             AND step.value->'session_ref' IS NOT NULL
             AND NOT EXISTS(SELECT 1 FROM workflow_session_joins j
               WHERE j.run=w.id AND j.step=step.value->>'step_id'
               AND j.session=step.value->'session_ref'->>'resource_id'))",
        [],
        |row| row.get(0),
    )?;
    require(
        !orphan,
        "RecoveryRequired",
        "joined session lost its workflow index entry",
    )?;
    let mut launches = conn.prepare("SELECT run,launch_task FROM workflow_session_joins")?;
    let rows = launches.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for entry in rows {
        let (run, task) = entry?;
        let run = db::find(conn, &run)?.ok_or_else(invalid)?;
        let task = db::find(conn, &task)?.ok_or_else(invalid)?;
        let join = joined(conn, &run, &task)?.ok_or_else(invalid)?;
        let receipt = launch_receipt(conn, &task, &join.opened)?;
        let deadline: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM authorization_deadlines child WHERE child.resource=?1)
               AND NOT EXISTS(SELECT 1 FROM authorization_deadlines child
                 JOIN authorization_deadlines parent ON parent.resource=?2 AND parent.boot_id=child.boot_id
                 WHERE child.resource=?1 AND (child.time_ns<>parent.time_ns
                   OR child.deadline_boot_ms>parent.deadline_boot_ms))",
            params![join.opened.id,run.id],
            |row| row.get(0),
        )?;
        require(
            deadline,
            "RecoveryRequired",
            "joined session lost its inherited elapsed deadline",
        )?;
        let active: Option<(i64, Option<i64>)> = conn
            .query_row(
                "SELECT task_rev,session_rev FROM active_workflow_tasks WHERE run=?1 AND task=?2",
                params![run.id, task.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let projection = run.body["steps"]
            .as_array()
            .and_then(|steps| {
                steps
                    .iter()
                    .find(|step| step["task_receipt"]["task_id"] == task.id)
            })
            .ok_or_else(invalid)?;
        if let Some(reference) = projection.get("session_ref") {
            let observed = db::resolve(conn, reference)?;
            require(
                observed.id == join.opened.id
                    && observed.kind == "Session"
                    && projection["session_lifecycle"] == observed.body["lifecycle"]
                    && projection["launch_artifact_ref"] == receipt.reference()
                    && active.is_none_or(|(_, rev)| rev == Some(observed.rev)),
                "RecoveryRequired",
                "workflow session observation disagrees with its active join",
            )?;
            if projection["lifecycle"] == "Succeeded" {
                require(
                    active.is_none()
                        && observed.body["lifecycle"] == "Succeeded"
                        && observed.reference() == join.current.reference()
                        && unresolved(conn, &observed)? == (false, false),
                    "RecoveryRequired",
                    "successful workflow session join still has unsettled work",
                )?;
                successful(conn, &observed)?;
            } else {
                require(
                    matches!(
                        projection["lifecycle"].as_str(),
                        Some("Failed" | "Cancelled")
                    ) || active.is_some(),
                    "RecoveryRequired",
                    "unsettled joined session lost its active workflow step",
                )?;
            }
        } else {
            require(
                active.is_some_and(|(rev, session_rev)| rev < task.rev && session_rev.is_none()),
                "RecoveryRequired",
                "workflow lost its unobserved original session launch",
            )?;
        }
    }
    Ok(())
}
