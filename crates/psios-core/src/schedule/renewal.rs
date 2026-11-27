//! Admit one separately authorized successor to a complete interval schedule.

use super::{
    Create, create_schedule, instant, repeat, schedule_id, timestamp, trigger_id, workflow_overlap,
};
use crate::{
    Result, agent, broker,
    db::{self, Db, Doc, row},
    decode, err, label, now, policy, require, storage, workflow,
};
use psios_api::{ScheduleRenewalRequest, canonical, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = storage::estimate::FORMAT + 1;
const KIND: &str = "ScheduleRenewal";

fn receipt_id(schedule: &str) -> String {
    format!("schedule-renewal-{schedule}")
}

fn template(conn: &Connection, schedule: &Doc) -> Result<(String, Value)> {
    let request = &schedule.spec["request"];
    let tool = request["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "schedule lacks its prepared tool"))?;
    let input = db::resolve(conn, &request["input_ref"])?;
    require(
        input.owner == schedule.owner,
        "RecoveryRequired",
        "schedule input belongs to a different principal",
    )?;
    Ok((
        tool.to_owned(),
        broker::dependencies::tool_input(&input)?.clone(),
    ))
}

fn session_unsettled(db: &Db<'_>, launch: &Doc) -> Result<bool> {
    let session_id = agent::launched_session_id(launch);
    let opened =
        db.c.query_row(
            "SELECT * FROM versions WHERE id=?1 AND kind='Session' AND rev=1",
            [&session_id],
            row,
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "scheduled Task lost its original Session",
            )
        })?;
    let origin = db.resolve(&opened.spec["origin_task_ref"])?;
    require(
        opened.owner == launch.owner
            && origin.kind == "Task"
            && origin.id == launch.id
            && origin.spec == launch.spec
            && origin.body["outcome_knowledge"] == "InFlight",
        "RecoveryRequired",
        "scheduled Session does not match its original launch",
    )?;
    workflow::session_join::launch_receipt(db.c, launch, &opened)?;
    let original = db.of(&session_id, "Session")?;
    require(
        original.spec == opened.spec,
        "RecoveryRequired",
        "scheduled Session admission changed",
    )?;
    Ok(db.c.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM current s WHERE s.kind='Session' AND s.owner=?1
             AND s.spec->'origin_task_ref'->>'resource_id'=?2
             AND (coalesce(s.body->>'lifecycle','') NOT IN ('Succeeded','Failed','Cancelled')
              OR s.body->>'outcome_knowledge'='Unknown'
              OR EXISTS(SELECT 1 FROM session_turns t JOIN model_inference_leases i USING(lease)
                    WHERE t.session=s.id AND i.status IN ('Pending','Unknown'))
              OR EXISTS(SELECT 1 FROM session_tasks j LEFT JOIN current t ON t.id=j.task
                    WHERE j.session=s.id AND (t.id IS NULL OR t.owner<>s.owner OR t.kind<>'Task'
                       OR coalesce(t.body->>'lifecycle','') NOT IN ('Succeeded','Failed','Cancelled')
                       OR coalesce(t.body->>'outcome_knowledge','') NOT IN ('Known','NotStarted')))))",
        params![launch.owner, launch.id],
        |row| row.get(0),
    )?)
}

fn realized_triggers(conn: &Connection, schedule: &Doc) -> Result<Vec<Doc>> {
    let rules: repeat::Repeat = repeat::persisted(&schedule.spec["repeat"])?;
    let references = schedule.body["trigger_refs"]
        .as_array()
        .filter(|refs| refs.len() == rules.count as usize && (2..=32).contains(&refs.len()))
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "completed interval schedule lost its trigger history",
            )
        })?;
    let mut realized = Vec::new();
    for (index, reference) in references.iter().enumerate() {
        let trigger = workflow_overlap::trigger(conn, schedule, reference, index)?;
        if trigger.body["lifecycle"] == "Triggered" {
            realized.push(trigger);
        } else {
            require(
                matches!(
                    trigger.body["lifecycle"].as_str(),
                    Some("Expired" | "Skipped")
                ) && trigger.body["task_receipt"].is_null(),
                "RecoveryRequired",
                "completed schedule contains an invalid trigger receipt",
            )?;
        }
    }
    require(
        realized.last().map_or(Value::Null, |trigger| {
            trigger.body["task_receipt"]["task_id"].clone()
        }) == schedule.body["last_task_id"],
        "RecoveryRequired",
        "completed schedule lost its final admitted Task",
    )?;
    Ok(realized)
}

fn settled_tasks(db: &Db<'_>, previous: &Doc) -> Result<Vec<Value>> {
    let mut evidence = Vec::new();
    for trigger in realized_triggers(db.c, previous)? {
        let task = workflow_overlap::prior_task(db.c, previous, &trigger, None)?;
        require(
            workflow_overlap::settled(&task),
            "Conflict",
            "an earlier scheduled Task is unsettled",
        )?;
        if task.body["lifecycle"] == "Succeeded" {
            match task.spec["task_type"].as_str() {
                Some(workflow::START_TOOL) => {
                    let (_, run) = workflow_overlap::completed_launch(
                        db.c,
                        &trigger.body["deadline_at"],
                        &task,
                        None,
                    )?;
                    require(
                        !workflow_overlap::child_incomplete(db.c, &run, None)?,
                        "Conflict",
                        "an earlier scheduled Workflow is unsettled",
                    )?;
                }
                Some(agent::START_SESSION) => {
                    require(
                        !session_unsettled(db, &task)?,
                        "Conflict",
                        "an earlier scheduled Session is unsettled",
                    )?;
                }
                _ => {}
            }
        }
        evidence.push(task.reference());
    }
    Ok(evidence)
}

fn response(receipt: &Doc) -> Value {
    json!({"renewal_ref":receipt.reference(), "schedule_ref":receipt.body["successor_ref"]})
}

pub(crate) fn renew(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let mut request: ScheduleRenewalRequest = decode(params)?;
    label(&request.id)?;
    label(&request.key)?;
    label(&request.delegation_id)?;
    request.run_at = timestamp(instant(&request.run_at)?);
    request.deadline_at = timestamp(instant(&request.deadline_at)?);
    policy::active(db, actor)?;
    policy::can(db, actor, "task.read")?;
    let previous = db.of(&request.id, "Schedule")?;
    require(
        previous.owner == actor,
        "PermissionDenied",
        "renewal requires the original schedule owner",
    )?;
    let receipt_id = receipt_id(&previous.id);
    if let Some(receipt) = db.find(&receipt_id)? {
        validate_receipt(db.c, &receipt)?;
        require(
            receipt.body["request"] == json!(request),
            "IdempotencyConflict",
            "schedule already renewed with another request",
        )?;
        return Ok(response(&receipt));
    }
    previous.cas(&request.expected_resource_version)?;
    require(
        previous.body["lifecycle"] == "Completed",
        "Conflict",
        "schedule is not complete",
    )?;
    require(
        previous.spec["repeat"].is_object(),
        "UnsupportedCapability",
        "renewal requires an interval schedule",
    )?;
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_schedules WHERE schedule=?1)",
        [&previous.id],
        |row| row.get(0),
    )?;
    require(
        !pending,
        "RecoveryRequired",
        "completed schedule still has a pending occurrence",
    )?;
    let evidence = settled_tasks(db, &previous)?;
    let (tool, input) = template(db.c, &previous)?;
    let declaration = Create {
        key: request.key.clone(),
        tool,
        input,
        delegation_id: request.delegation_id.clone(),
        run_at: Some(request.run_at.clone()),
        deadline_at: Some(request.deadline_at.clone()),
        repeat: Some(repeat::persisted(&previous.spec["repeat"])?),
        calendar: None,
        event: None,
    };
    let successor = create_schedule(db, actor, declaration, Some(&previous))?;
    let receipt = db.create(
        KIND,
        &receipt_id,
        actor,
        json!({"schedule_id":previous.id}),
        json!({
            "predecessor_ref":previous.reference(), "successor_ref":successor["reference"],
            "request":request, "settled_task_refs":evidence, "renewed_at":now(),
        }),
    )?;
    validate_receipt(db.c, &receipt)?;
    db.audit(actor, "Schedule.Renew", Some(&previous.id), "Accepted")?;
    Ok(response(&receipt))
}

fn validate_receipt(conn: &Connection, receipt: &Doc) -> Result<()> {
    let previous = db::resolve(conn, &receipt.body["predecessor_ref"])?;
    let successor = db::resolve(conn, &receipt.body["successor_ref"])?;
    let request: ScheduleRenewalRequest = repeat::persisted(&receipt.body["request"])?;
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    let admission: Option<i64> = conn
        .query_row(
            "SELECT s.seq FROM versions p, versions s, versions r
         WHERE p.id=?1 AND p.rev=?2 AND s.id=?3 AND s.rev=?4 AND r.id=?5 AND r.rev=?6
            AND p.seq<s.seq AND s.seq<r.seq",
            params![
                previous.id,
                previous.rev,
                successor.id,
                successor.rev,
                receipt.id,
                receipt.rev
            ],
            |row| row.get(0),
        )
        .optional()?;
    let admission = admission.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "schedule renewal has no ordered admission witness",
        )
    })?;
    let original_template = template(conn, &previous)?;
    let creation = sha256(&canonical(&json!([
        original_template.0, original_template.1, request.delegation_id, request.run_at, request.deadline_at,
        previous.spec["repeat"], {"predecessor_ref":previous.reference()},
    ])));
    require(
        receipt.kind == KIND
            && receipt.rev == 1
            && receipt.id == receipt_id(&previous.id)
            && receipt.spec["schedule_id"] == previous.id
            && receipt.owner == previous.owner
            && previous.kind == "Schedule"
            && previous.id == request.id
            && previous.version() == request.expected_resource_version
            && previous.body["lifecycle"] == "Completed"
            && successor.kind == "Schedule"
            && successor.rev == 1
            && successor.id != previous.id
            && successor.id == schedule_id(&authority, &previous.owner, &request.key)
            && successor.owner == previous.owner
            && successor.body["lifecycle"] == "Pending"
            && successor.spec["predecessor_ref"] == previous.reference()
            && successor.body["predecessor_ref"] == previous.reference()
            && successor.spec["repeat"] == previous.spec["repeat"]
            && successor.spec["creation_digest"] == creation
            && successor.spec["request"]["origin_ref"]["origin_type"] == "Schedule"
            && successor.spec["request"]["origin_ref"]["origin_id"] == successor.id
            && successor.spec["request"]["delegation_ref"]["resource_id"] == request.delegation_id
            && successor.body["next"]["ordinal"] == 1
            && successor.body["next"]["run_at"] == request.run_at
            && successor.body["next"]["deadline_at"] == request.deadline_at
            && successor.body["next"]["trigger_id"] == trigger_id(&successor.id, &request.run_at)
            && original_template == template(conn, &successor)?,
        "RecoveryRequired",
        "schedule renewal receipt differs from its linked schedules",
    )?;
    let triggers = realized_triggers(conn, &previous)?;
    let evidence = receipt.body["settled_task_refs"]
        .as_array()
        .filter(|refs| refs.len() == triggers.len())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "schedule renewal lost its settled Task references",
            )
        })?;
    for (trigger, reference) in triggers.iter().zip(evidence) {
        let task = workflow_overlap::prior_task(conn, &previous, trigger, Some(admission))?;
        require(
            task.reference() == *reference && workflow_overlap::settled(&task),
            "RecoveryRequired",
            "schedule renewal has unrelated or unsettled Task evidence",
        )?;
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut query = conn.prepare("SELECT * FROM current WHERE kind=?1")?;
    let mut count = 0i64;
    for receipt in query.query_map([KIND], row)? {
        validate_receipt(conn, &receipt?)?;
        count += 1;
    }
    let successors: i64 = conn.query_row(
        "SELECT count(*) FROM versions WHERE kind='Schedule' AND rev=1 AND spec->'predecessor_ref' IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    require(
        count == successors,
        "RecoveryRequired",
        "schedule renewal journal has an orphaned successor",
    )
}
