//! Bind a WorkItem's workflow criterion to a broker-admitted successful step.
use super::{criterion_definition, stable_id};
use crate::{
    Result, artifact,
    db::{Db, Doc, find, resolve, row},
    decode, err, policy, require,
};
use psios_api::{WorkItemCriterion, canonical};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::milestone::FORMAT + 1;
const DEFINITION: &str = "WorkflowDefinition";
const RUN: &str = "WorkflowRun";

fn mismatch() -> crate::Error {
    err(
        "PreconditionFailed",
        "task does not prove the configured successful workflow step",
    )
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "invalid workflow work item evidence custody",
    )
}

fn position(conn: &Connection, d: &Doc) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![d.id, d.rev],
        |row| row.get(0),
    )?)
}

fn source_run(conn: &Connection, task: &Doc, evidence: Option<&Doc>) -> Result<Doc> {
    let origin = &task.body["origin_ref"];
    if origin["origin_type"] != RUN
        || origin["attempt_generation"] != "1"
        || task.spec["origin_ref"] != *origin
    {
        return Err(mismatch());
    }
    let id = origin["origin_id"].as_str().ok_or_else(mismatch)?;
    let run = if let Some(evidence) = evidence {
        let seq = position(conn, evidence)?;
        require(
            position(conn, task)? < seq,
            "RecoveryRequired",
            "workflow evidence precedes its submitted task revision",
        )?;
        conn.prepare_cached(
            "SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![id, seq], row)
        .optional()?
    } else {
        find(conn, id)?
    };
    run.filter(|run| run.kind == RUN && run.owner == task.owner)
        .ok_or_else(mismatch)
}

fn step<'a>(steps: &'a Value, key: &str, id: &str) -> Result<&'a Value> {
    let mut matches = steps
        .as_array()
        .ok_or_else(invalid)?
        .iter()
        .filter(|entry| entry[key] == id);
    let found = matches.next().ok_or_else(mismatch)?;
    require(
        matches.next().is_none(),
        "RecoveryRequired",
        "workflow repeats a step identity",
    )?;
    Ok(found)
}

fn admitted(
    conn: &Connection,
    run: &Doc,
    prepared: &Value,
    observed: &Value,
    task: &Doc,
    step_key: &str,
) -> Result<()> {
    let origin = &task.body["origin_ref"];
    let key = stable_id("workflow-dispatch", &json!([step_key, "1"]));
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    require(
        origin["authority_id"] == authority
            && origin["step_key"] == step_key
            && prepared["step_key"] == step_key
            && prepared["request"]["task_type"] == task.spec["task_type"]
            && prepared["request"]["origin_ref"] == *origin
            && prepared["request"]["idempotency_key"] == key
            && task.spec["idempotency_key"] == key
            && observed.is_object()
            && observed["deduplicated"].is_boolean(),
        "PreconditionFailed",
        "workflow step does not match the immutable task admission",
    )?;
    let origin_key = String::from_utf8(canonical(origin)).map_err(|_| invalid())?;
    let receipt: Value = conn
        .query_row(
            "SELECT receipt FROM submissions WHERE task=?1 AND owner=?2 AND origin=?3
               AND key=?4 AND scope=?5 AND digest=?6 AND cancelled=0 AND receipt IS NOT NULL",
            params![
                task.id,
                task.owner,
                origin_key,
                key,
                run.id,
                task.spec["request_digest"].as_str()
            ],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(invalid)?;
    let admission = conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=1",
            [&task.id],
            row,
        )
        .optional()?
        .ok_or_else(invalid)?;
    let mut normalized = observed.clone();
    // Broker idempotent lookup can replay the same accepted receipt with this bit set.
    normalized["deduplicated"] = json!(false);
    require(
        normalized == receipt
            && receipt["origin_ref"] == *origin
            && receipt["task_id"] == task.id
            && receipt["task_revision"] == admission.version()
            && receipt["request_digest"] == task.spec["request_digest"]
            && receipt["idempotency_key"] == key
            && admission.kind == "Task"
            && admission.owner == task.owner
            && admission.spec == task.spec
            && admission.body["origin_ref"] == *origin
            && admission.body["outcome_knowledge"] == "NotStarted",
        "RecoveryRequired",
        "workflow step does not match its original broker receipt",
    )
}

pub(super) fn witness(
    conn: &Connection,
    item: &Doc,
    task: &Doc,
    output: &Doc,
    evidence: Option<&Doc>,
) -> Result<Option<Value>> {
    let criterion: WorkItemCriterion = decode(&item.spec["criterion"])?;
    let recorded = evidence.and_then(|doc| doc.body.get("workflow_witness"));
    let WorkItemCriterion::WorkflowTaskResult { step_id, .. } = &criterion else {
        require(
            recorded.is_none(),
            "RecoveryRequired",
            "task-only work item evidence carries a workflow witness",
        )?;
        return Ok(None);
    };

    let run = source_run(conn, task, evidence)?;
    let definition = criterion_definition(conn, &criterion, &task.owner)?.ok_or_else(invalid)?;
    let run_position = position(conn, &run)?;
    require(
        run.spec["definition_ref"] == definition.reference()
            && position(conn, &definition)? < run_position,
        "PreconditionFailed",
        "task did not run the fixed workflow definition",
    )?;
    let prepared = step(&run.spec["steps"], "id", step_id)?;
    let observed = step(&run.body["steps"], "step_id", step_id)?;
    let key = stable_id("workflow-step", &json!([run.id, step_id]));
    require(
        observed["step_key"] == key
            && observed["lifecycle"] == "Succeeded"
            && observed["outcome_knowledge"] == "Known"
            && observed["task_ref"]["resource_id"] == task.id,
        "PreconditionFailed",
        "task is not the known successful receipt of the configured workflow step",
    )?;
    admitted(conn, &run, prepared, &observed["task_receipt"], task, &key)?;

    let settled = resolve(conn, &observed["task_ref"])?;
    let settled_position = position(conn, &settled)?;
    require(
        settled.kind == "Task"
            && settled.owner == task.owner
            && settled.spec == task.spec
            && settled.body["origin_ref"] == task.body["origin_ref"]
            && settled.body["lifecycle"] == "Succeeded"
            && settled.body["outcome_knowledge"] == "Known"
            && settled.body["current_run_id"] == task.body["current_run_id"]
            && settled.body["operation_refs"] == task.body["operation_refs"]
            && settled.body["result"]["artifact_refs"] == task.body["result"]["artifact_refs"]
            && settled.body["result"]["artifact_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&output.reference()))
            && artifact::is_task_output(output, &settled)
            && settled_position < run_position
            && position(conn, output)? < settled_position,
        "PreconditionFailed",
        "workflow step does not seal the original successful task output",
    )?;

    let witness = json!({
        "workflow_run_ref":run.reference(),
        "step_id":step_id,
        "step_key":key,
        "task_ref":settled.reference(),
    });
    require(
        evidence.is_none() || recorded == Some(&witness),
        "RecoveryRequired",
        "work item evidence lost or changed its exact workflow step witness",
    )?;
    Ok(Some(witness))
}

fn can_view(db: &Db<'_>, actor: &str, reference: &Value, kind: &str) -> Result<bool> {
    let result = (|| {
        policy::can(db, actor, "task.read")?;
        let resource = db.resolve(reference)?;
        require(
            resource.kind == kind,
            "RecoveryRequired",
            "expected workflow resource",
        )?;
        policy::owned(actor, &resource)
    })();
    match result {
        Ok(()) => Ok(true),
        Err(error) if error.0.code == "PermissionDenied" => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn may_view(db: &Db<'_>, actor: &str, definition: &Value) -> Result<bool> {
    can_view(db, actor, definition, DEFINITION)
}

pub(super) fn redact(db: &Db<'_>, actor: &str, evidence: &mut Value) -> Result<()> {
    if evidence["criterion"]["kind"] != "WorkflowTaskResult" {
        return Ok(());
    }
    let definition = may_view(db, actor, &evidence["criterion"]["definition_ref"])?;
    if !definition {
        let criterion = evidence["criterion"].as_object_mut().ok_or_else(invalid)?;
        criterion.remove("definition_ref");
        criterion.remove("step_id");
    }
    if !definition
        || !can_view(
            db,
            actor,
            &evidence["workflow_witness"]["workflow_run_ref"],
            RUN,
        )?
    {
        evidence
            .as_object_mut()
            .ok_or_else(invalid)?
            .remove("workflow_witness");
    }
    Ok(())
}
