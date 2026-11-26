pub(crate) mod batch;
pub(crate) mod selection;

use super::{CHUNK_BYTES, Manifest, is_task_output, repair, stored_digest};
use crate::{
    Error, Result, agent,
    db::{Db, Doc, find},
    decode, err, label, now, policy, require, workflow,
};
use psios_api::contract;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) const FORMAT: u32 = 31;
const KIND: &str = "TaskResultEviction";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evict {
    id: String,
    expected_resource_version: String,
    reason: String,
}

pub(super) enum CandidateMetadata {
    Ready(u128),
    Pinned,
    Ineligible,
}

// Tag ordinary exclusions where they are decided. A nested reference or custody
// check may return the same public error code for damaged authoritative metadata.
enum CandidateError {
    Pinned(Error),
    Ineligible(Error),
    Fault(Error),
}

type CandidateResult<T> = std::result::Result<T, CandidateError>;

impl From<Error> for CandidateError {
    fn from(error: Error) -> Self {
        Self::Fault(error)
    }
}

impl From<rusqlite::Error> for CandidateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Fault(error.into())
    }
}

impl From<CandidateError> for Error {
    fn from(error: CandidateError) -> Self {
        match error {
            CandidateError::Pinned(error)
            | CandidateError::Ineligible(error)
            | CandidateError::Fault(error) => error,
        }
    }
}

fn eligible(condition: bool, message: &str) -> CandidateResult<()> {
    require(condition, "PreconditionFailed", message).map_err(CandidateError::Ineligible)
}

fn unpinned(condition: bool, message: &str) -> CandidateResult<()> {
    require(condition, "Conflict", message).map_err(CandidateError::Pinned)
}

pub(super) struct Candidate {
    task: Doc,
    artifacts: Vec<CandidateArtifact>,
    bytes: u128,
}

struct CandidateArtifact {
    id: String,
    rev: i64,
    chunks: usize,
}

impl Candidate {
    pub(super) fn bytes(&self) -> u128 {
        self.bytes
    }
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
    if super::is_raw(artifact) {
        return super::ingress::retention(conn, artifact);
    }
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
        "artifact content was evicted",
    )
}

pub fn describe(db: &Db<'_>, artifact: &Doc, shared: bool) -> Result<Value> {
    let record = receipt(db.c, artifact)?;
    let resource = artifact.envelope(&db.meta("authority")?);
    contract("resource-envelope", &resource)?;
    Ok(json!({
        "reference":artifact.reference(), "resource":resource,
        "availability": if record.is_some() { "Evicted" } else { "Available" },
        "repair_chunk_bytes": (record.is_none() && repair::supported(artifact)).then_some(CHUNK_BYTES),
        "retention": record.as_ref().filter(|_| !shared).map(view)
    }))
}

fn read_only_tool<'a>(conn: &Connection, task: &'a Doc) -> CandidateResult<&'a str> {
    let tool = task.spec["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "producing Task has no tool identity"))?;
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
        let op = crate::db::resolve(conn, reference)?;
        require(
            op.kind == "Operation" && op.owner == task.owner && op.spec["task_id"] == task.id,
            "RecoveryRequired",
            "operation does not belong to the producing Task",
        )?;
        let plan = crate::db::resolve(conn, &op.body["plan_ref"])?;
        let capability = crate::db::resolve(conn, &plan.body["runtime_refs"][0])?;
        require(
            plan.kind == "OperationPlan"
                && plan.body["allowed_effects"][0]["action"] == tool
                && capability.kind == "Capability"
                && capability.body["actions"][0] == tool,
            "RecoveryRequired",
            "operation does not pin the producing capability",
        )?;
        eligible(
            capability.spec["effect_class"] == "ReadOnly",
            "the dispatched capability retained effect evidence",
        )?;
    }
    Ok(tool)
}

pub fn evict(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Evict = decode(params)?;
    validate_reason(&p.reason)?;
    let task = db.of(&p.id, "Task")?;
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
    let candidate = prepare_candidate(db, task)?;
    apply_candidate(db, actor, &p.reason, candidate)
}

pub(super) fn validate_reason(reason: &str) -> Result<()> {
    label(reason)?;
    require(
        !reason.trim().is_empty(),
        "InvalidArgument",
        "a retention reason is required",
    )
}

fn require_current(db: &Db<'_>, task: &Doc) -> Result<()> {
    let current = db.of(&task.id, "Task")?;
    require(
        task.rev == current.rev
            && task.kind == current.kind
            && task.owner == current.owner
            && task.spec == current.spec
            && task.body == current.body,
        "Conflict",
        "resource version changed",
    )
}

fn inspect_candidate(
    db: &Db<'_>,
    task: &Doc,
    mut inspect_payload: impl FnMut(&Doc, &Manifest<'_>) -> Result<()>,
) -> CandidateResult<u128> {
    require_current(db, task)?;
    let prior = task_receipt(db.c, task)?;
    eligible(
        prior.is_none()
            && matches!(
                task.body["lifecycle"].as_str(),
                Some("Succeeded" | "Cancelled")
            )
            && task.body["outcome_knowledge"] == "Known"
            && task.body["result"]["availability"] == "Available",
        "eviction requires a settled Task with available results",
    )?;
    eligible(
        policy::readonly(read_only_tool(db.c, task)?),
        "write results are retained as effect evidence",
    )?;
    unpinned(
        !workflow::retains_result(db, task)?,
        "an active WorkflowRun retains the task result",
    )?;
    unpinned(
        !agent::retains_result(db, task)?,
        "an active Agent session retains the task result",
    )?;
    unpinned(
        !crate::broker::dependencies::retains_result(db, &task.id)?,
        "an unsettled Task requires this producer's result",
    )?;
    unpinned(
        !crate::apps::stop::retains_result(db, &task.id)?,
        "an admitted OCI stop continuation retains the exit evidence",
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
    let mut seen = BTreeSet::new();
    let mut bytes = 0u128;
    for reference in refs {
        let artifact = db.resolve(reference)?;
        require(
            is_task_output(&artifact, task),
            "RecoveryRequired",
            "task result refers to a foreign artifact",
        )?;
        require(
            seen.insert(artifact.id.clone()),
            "RecoveryRequired",
            "task result contains duplicate artifact identities",
        )?;
        unpinned(
            !crate::node::network::probe::state::retains_artifact(db.c, &artifact)?,
            "a network probe retains its latest measurement for verification",
        )?;
        unpinned(
            !crate::project::evidence::retains_artifact(db.c, &artifact)?,
            "a work item retains this artifact during review and acceptance",
        )?;
        let mut evidence = db.c.prepare_cached(
            "SELECT * FROM versions WHERE kind='RecoveryEvidence'
               AND json_extract(body,'$.evidence.observation_ref.resource_id')=?1",
        )?;
        for record in evidence.query_map([&artifact.id], crate::db::row)? {
            unpinned(
                task.spec["task_type"] == "service.inspect"
                    && crate::node::service_control::releases_inspection(db, &artifact, &record?)?,
                "an Operation retains this artifact as reconciliation evidence",
            )?;
        }
        let manifest = Manifest::read(&artifact)?.ok_or_else(|| {
            CandidateError::Ineligible(err(
                "UnsupportedCapability",
                "legacy inline artifact content cannot be evicted",
            ))
        })?;
        // Removal operates on SQLite chunks. File-backed artifacts have separate custody.
        require(
            !super::files::managed(&artifact),
            "RecoveryRequired",
            "artifact chunk is missing or has changed size",
        )?;
        inspect_payload(&artifact, &manifest)?;
        bytes = bytes
            .checked_add(manifest.size as u128)
            .ok_or_else(|| err("RecoveryRequired", "task result byte count overflow"))?;
    }
    Ok(bytes)
}

pub(super) fn candidate_metadata(db: &Db<'_>, task: &Doc) -> Result<CandidateMetadata> {
    match inspect_candidate(db, task, |_, _| Ok(())) {
        Ok(bytes) => Ok(CandidateMetadata::Ready(bytes)),
        Err(CandidateError::Pinned(_)) => Ok(CandidateMetadata::Pinned),
        Err(CandidateError::Ineligible(_)) => Ok(CandidateMetadata::Ineligible),
        Err(CandidateError::Fault(error)) => {
            if matches!(
                error.0.code.as_str(),
                "Conflict"
                    | "InvalidArgument"
                    | "NotFound"
                    | "PermissionDenied"
                    | "PreconditionFailed"
            ) {
                Err(err(
                    "RecoveryRequired",
                    format!("result eviction candidate metadata: {}", error.0.message),
                ))
            } else {
                Err(error)
            }
        }
    }
}

/// Checks live retention and content, keeping only metadata needed by this transaction.
pub(super) fn prepare_candidate(db: &Db<'_>, task: Doc) -> Result<Candidate> {
    let mut artifacts = Vec::new();
    let bytes = inspect_candidate(db, &task, |artifact, manifest| {
        manifest.verify(db.c)?;
        let stored: i64 = db.c.query_row(
            "SELECT count(*) FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2",
            params![artifact.id, artifact.rev],
            |row| row.get(0),
        )?;
        require(
            usize::try_from(stored).ok() == Some(manifest.chunks.len()),
            "RecoveryRequired",
            "artifact payload count disagrees with the retained manifest",
        )?;
        artifacts.push(CandidateArtifact {
            chunks: manifest.chunks.len(),
            id: artifact.id.clone(),
            rev: artifact.rev,
        });
        Ok(())
    })?;
    Ok(Candidate {
        task,
        artifacts,
        bytes,
    })
}

/// Applies a candidate prepared in the same authority transaction.
pub(super) fn apply_candidate(
    db: &Db<'_>,
    actor: &str,
    reason: &str,
    candidate: Candidate,
) -> Result<Value> {
    require(
        !db.c.is_autocommit(),
        "RecoveryRequired",
        "result eviction requires an authority transaction",
    )?;
    let Candidate {
        task,
        artifacts,
        bytes,
    } = candidate;
    require_current(db, &task)?;
    for artifact in artifacts {
        require(
            db.c.execute(
                "DELETE FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2",
                params![artifact.id, artifact.rev],
            )? == artifact.chunks,
            "RecoveryRequired",
            "artifact payload count disagrees with the retained manifest",
        )?;
        super::capacity::release(db, &artifact.id)?;
    }
    let mut body = task.body.clone();
    body["result"]["availability"] = json!("Evicted");
    let updated = db.status(&task, body)?;
    let record = db.create(
        KIND,
        &receipt_id(&task.id),
        &task.owner,
        json!({"task_id":task.id}),
        json!({
            "previous_task_ref": task.reference(), "task_ref": updated.reference(),
            "artifact_refs": task.body["result"]["artifact_refs"], "reason": reason, "evicted_at": now(),
            "evicted_by": actor, "content_bytes_removed": bytes.to_string()
        }),
    )?;
    db.audit(actor, "Task.EvictResults", Some(&task.id), "Evicted")?;
    Ok(view(&record))
}
