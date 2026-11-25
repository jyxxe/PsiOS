use super::{CHUNK_BYTES, Manifest, is_task_output, stored_digest};
use crate::{
    Result, agent,
    db::{Db, Doc, find},
    decode, err, label, now, policy, require, workflow,
};
use psios_api::contract;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = 31;
const KIND: &str = "TaskResultEviction";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evict {
    id: String,
    expected_resource_version: String,
    reason: String,
}

fn receipt_id(task: &str) -> String {
    format!("result-eviction-{task}")
}

fn view(d: &Doc) -> Value {
    json!({"reference":d.reference(),"eviction":d.body})
}

fn validate_record(record: &Doc, task: &Doc) -> Result<()> {
    require(
        record.kind == KIND
            && record.rev == 1
            && record.owner == task.owner
            && task.kind == "Task"
            && task.body["result"]["availability"] == "Evicted"
            && record.body["artifact_refs"] == task.body["result"]["artifact_refs"]
            && record.body["task_ref"] == task.reference()
            && record.body["previous_task_ref"]["resource_id"] == task.id,
        "RecoveryRequired",
        "result eviction disagrees with the producing Task",
    )
}

pub(super) fn task_receipt(conn: &Connection, task: &Doc) -> Result<Option<Doc>> {
    let record = find(conn, &receipt_id(&task.id))?;
    if let Some(record) = &record {
        validate_record(record, task)?;
    }
    Ok(record)
}

pub(super) fn receipt(conn: &Connection, artifact: &Doc) -> Result<Option<Doc>> {
    if stored_digest(artifact).is_none() {
        return Ok(None);
    }
    let id = artifact.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "artifact producer identity missing"))?;
    let Some(record) = find(conn, &receipt_id(id))? else {
        return Ok(None);
    };
    let task = find(conn, id)?
        .ok_or_else(|| err("RecoveryRequired", "evicted artifact has no producing Task"))?;
    validate_record(&record, &task)?;
    require(
        record.owner == artifact.owner
            && record.body["artifact_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&artifact.reference())),
        "RecoveryRequired",
        "result eviction does not cover this artifact revision",
    )?;
    Ok(Some(record))
}

pub(crate) fn require_content(conn: &Connection, artifact: &Doc) -> Result<()> {
    require(
        receipt(conn, artifact)?.is_none(),
        "ResultEvicted",
        "task result content was evicted",
    )
}

pub fn describe(db: &Db<'_>, artifact: &Doc, shared: bool) -> Result<Value> {
    let record = receipt(db.c, artifact)?;
    let resource = artifact.envelope(&db.meta("authority")?);
    contract("resource-envelope", &resource)?;
    Ok(json!({
        "reference":artifact.reference(), "resource":resource,
        "availability": if record.is_some() { "Evicted" } else { "Available" },
        "repair_chunk_bytes": (record.is_none() && stored_digest(artifact).is_some()).then_some(CHUNK_BYTES),
        "retention": record.as_ref().filter(|_| !shared).map(view)
    }))
}

fn require_read_only(db: &Db<'_>, task: &Doc) -> Result<()> {
    let tool = task.spec["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "producing Task has no tool identity"))?;
    require(
        policy::readonly(tool),
        "PreconditionFailed",
        "write results are retained as effect evidence",
    )?;
    let operations = task.body["operation_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "producing Task has no Operation evidence",
            )
        })?;
    for reference in operations {
        let op = db.resolve(reference)?;
        require(
            op.kind == "Operation" && op.owner == task.owner && op.spec["task_id"] == task.id,
            "RecoveryRequired",
            "operation does not belong to the producing Task",
        )?;
        let plan = db.resolve(&op.body["plan_ref"])?;
        let capability = db.resolve(&plan.body["runtime_refs"][0])?;
        require(
            plan.kind == "OperationPlan"
                && plan.body["allowed_effects"][0]["action"] == tool
                && capability.kind == "Capability"
                && capability.body["actions"][0] == tool,
            "RecoveryRequired",
            "operation does not pin the producing capability",
        )?;
        require(
            capability.spec["effect_class"] == "ReadOnly",
            "PreconditionFailed",
            "the dispatched capability retained effect evidence",
        )?;
    }
    Ok(())
}

pub fn evict(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Evict = decode(params)?;
    label(&p.reason)?;
    require(
        !p.reason.trim().is_empty(),
        "InvalidArgument",
        "a retention reason is required",
    )?;
    let task = db.of(&p.id, "Task")?;
    let record_id = receipt_id(&task.id);
    if let Some(record) = task_receipt(db.c, &task)? {
        require(
            record.body["previous_task_ref"]["version"] == p.expected_resource_version
                && record.body["reason"] == p.reason,
            "Conflict",
            "task results were already evicted with a different request",
        )?;
        return Ok(view(&record));
    }
    task.cas(&p.expected_resource_version)?;
    require(
        matches!(
            task.body["lifecycle"].as_str(),
            Some("Succeeded" | "Cancelled")
        ) && task.body["outcome_knowledge"] == "Known"
            && task.body["result"]["availability"] == "Available",
        "PreconditionFailed",
        "eviction requires a settled Task with available results",
    )?;
    require_read_only(db, &task)?;
    require(
        !workflow::retains_result(db, &task)?,
        "Conflict",
        "an active WorkflowRun retains the task result",
    )?;
    require(
        !agent::retains_result(db, &task)?,
        "Conflict",
        "an active Agent session retains the task result",
    )?;
    require(
        !crate::broker::dependencies::retains_result(db, &task.id)?,
        "Conflict",
        "an unsettled Task requires this producer's result",
    )?;
    let refs = task.body["result"]["artifact_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "available task result has no artifact references",
            )
        })?;
    let mut bytes_removed = 0u128;
    for reference in refs {
        let artifact = db.resolve(reference)?;
        require(
            is_task_output(&artifact, &task),
            "RecoveryRequired",
            "task result refers to a foreign artifact",
        )?;
        let retained: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE kind='RecoveryEvidence'
               AND json_extract(body,'$.evidence.observation_ref.resource_id')=?1)",
            [&artifact.id],
            |row| row.get(0),
        )?;
        require(
            !retained,
            "Conflict",
            "an Operation retains this artifact as reconciliation evidence",
        )?;
        let manifest = Manifest::read(&artifact)?.ok_or_else(|| {
            err(
                "UnsupportedCapability",
                "legacy inline artifact content cannot be evicted",
            )
        })?;
        manifest.verify(db.c)?;
        require(
            db.c.execute(
                "DELETE FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2",
                params![artifact.id, artifact.rev],
            )? == manifest.chunks.len(),
            "RecoveryRequired",
            "artifact payload count disagrees with the retained manifest",
        )?;
        super::capacity::release(db, &artifact.id)?;
        bytes_removed += manifest.size as u128;
    }
    let mut body = task.body.clone();
    body["result"]["availability"] = json!("Evicted");
    let updated = db.status(&task, body)?;
    let record = db.create(
        KIND,
        &record_id,
        &task.owner,
        json!({"task_id":task.id}),
        json!({
            "previous_task_ref": task.reference(), "task_ref": updated.reference(),
            "artifact_refs": refs, "reason": p.reason, "evicted_at": now(),
            "evicted_by": actor, "content_bytes_removed": bytes_removed.to_string()
        }),
    )?;
    db.audit(actor, "Task.EvictResults", Some(&task.id), "Evicted")?;
    Ok(view(&record))
}
