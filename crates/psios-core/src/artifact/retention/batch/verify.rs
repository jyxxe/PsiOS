//! Check a batch against the exact immutable Task transitions and retained manifests.
use super::{KIND, Plan, PlanItem, digest, receipt_id};
use crate::{
    Result,
    artifact::{Manifest, is_task_output, retention},
    db::{Doc, resolve, row},
    decode, err, policy, require,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

struct Entry {
    seq: i64,
    doc: Doc,
    correlation: String,
    incarnation: String,
}

fn entry(r: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        seq: r.get("seq")?,
        doc: row(r)?,
        correlation: r.get("correlation")?,
        incarnation: r.get("incarnation")?,
    })
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "invalid task result eviction batch history",
    )
}

fn stamped(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).is_ok())
}

fn manifest_bytes(conn: &Connection, previous: &Entry) -> Result<u64> {
    let task = &previous.doc;
    let refs = task.body["result"]["artifact_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or_else(invalid)?;
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    let mut custody = conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM versions WHERE id=?1 AND rev=?2 AND seq<?3)
          AND NOT EXISTS(SELECT 1 FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2)",
    )?;
    for reference in refs {
        let artifact = resolve(conn, reference)?;
        let manifest = Manifest::read(&artifact)?.ok_or_else(invalid)?;
        let evicted: bool = custody
            .query_row(params![artifact.id, artifact.rev, previous.seq], |r| {
                r.get(0)
            })?;
        require(
            is_task_output(&artifact, task)
                && !crate::artifact::files::managed(&artifact)
                && seen.insert(artifact.id.clone())
                && evicted,
            "RecoveryRequired",
            "batch result must refer to a unique evicted output of the producing Task",
        )?;
        let bytes = u64::try_from(manifest.size).map_err(|_| invalid())?;
        total = total.checked_add(bytes).ok_or_else(invalid)?;
    }
    Ok(total)
}

fn member(
    conn: &Connection,
    plan: &Plan,
    item: &PlanItem,
    start: i64,
    updated: &Doc,
    eviction: &Doc,
) -> Result<()> {
    let prior_entry = conn
        .prepare_cached("SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1")?
        .query_row(params![item.task_ref.resource_id, start], entry)
        .optional()?
        .ok_or_else(invalid)?;
    let previous = &prior_entry.doc;
    require(
        previous.reference() == serde_json::to_value(&item.task_ref)?
            && previous.kind == "Task"
            && matches!(
                previous.body["lifecycle"].as_str(),
                Some("Succeeded" | "Cancelled")
            )
            && previous.body["outcome_knowledge"] == "Known"
            && previous.body["result"]["availability"] == "Available"
            && updated.id == previous.id
            && updated.owner == previous.owner
            && updated.spec == previous.spec
            && updated.generation == previous.generation
            && updated.created == previous.created
            && previous.rev.checked_add(1) == Some(updated.rev),
        "RecoveryRequired",
        "batch eviction lost its exact previously available Task",
    )?;
    retention::require_read_only(conn, previous)?;
    retention::validate_record(eviction, updated)?;
    let mut expected = previous.body.clone();
    expected["result"]["availability"] = json!("Evicted");
    expected["task_revision"] = json!(updated.version());
    require(
        updated.body == expected,
        "RecoveryRequired",
        "batch eviction modified Task fields beyond result availability",
    )?;
    require(
        eviction.id == retention::receipt_id(&previous.id)
            && eviction.generation == 1
            && eviction.spec == json!({"task_id":previous.id})
            && stamped(&eviction.body["evicted_at"])
            && eviction.body
                == json!({
                    "previous_task_ref":previous.reference(),
                    "task_ref":updated.reference(),
                    "artifact_refs":previous.body["result"]["artifact_refs"],
                    "reason":plan.reason,
                    "evicted_at":eviction.body["evicted_at"],
                    "evicted_by":policy::OWNER,
                    "content_bytes_removed":item.content_bytes.to_string(),
                })
            && manifest_bytes(conn, &prior_entry)? == item.content_bytes,
        "RecoveryRequired",
        "batch member receipt disagrees with the exact original output metadata",
    )
}

fn check_batch(conn: &Connection, authority: &str, batch: &Entry) -> Result<()> {
    let record = &batch.doc;
    let plan: Plan = decode(&record.spec["plan"])?;
    plan.validate()?;
    let digest = digest(&plan)?;
    let references = record.body["eviction_refs"]
        .as_array()
        .ok_or_else(invalid)?;
    let revised: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions WHERE id=?1 AND rev>1)",
        [&record.id],
        |r| r.get(0),
    )?;
    require(
        record.id == receipt_id(&digest)
            && record.rev == 1
            && record.generation == 1
            && record.owner == policy::OWNER
            && plan.owner == policy::OWNER
            && plan.authority_id == authority
            && plan.recovery_incarnation == batch.incarnation
            && !revised
            && record.spec == json!({"plan":plan})
            && stamped(&record.body["evicted_at"])
            && record.body
                == json!({
                    "plan_digest":digest,
                    "reason":plan.reason,
                    "eviction_refs":references,
                    "content_bytes_removed":plan.content_bytes.to_string(),
                    "evicted_at":record.body["evicted_at"],
                    "evicted_by":policy::OWNER,
                })
            && references.len() == plan.items.len(),
        "RecoveryRequired",
        "batch aggregate does not bind its canonical plan and receipt set",
    )?;

    let count = i64::try_from(plan.items.len()).map_err(|_| invalid())?;
    let start = batch
        .seq
        .checked_sub(2 * count)
        .filter(|seq| *seq > 0)
        .ok_or_else(invalid)?;
    let mut statement =
        conn.prepare_cached("SELECT * FROM versions WHERE seq>=?1 AND seq<?2 ORDER BY seq")?;
    let mut history = statement.query_map(params![start, batch.seq], entry)?;
    for (index, (item, reference)) in plan.items.iter().zip(references).enumerate() {
        let updated = history.next().transpose()?.ok_or_else(invalid)?;
        let eviction = history.next().transpose()?.ok_or_else(invalid)?;
        let expected = start + 2 * index as i64;
        require(
            updated.seq == expected
                && eviction.seq == expected + 1
                && updated.correlation == batch.correlation
                && eviction.correlation == batch.correlation
                && updated.incarnation == batch.incarnation
                && eviction.incarnation == batch.incarnation
                && eviction.doc.reference() == *reference,
            "RecoveryRequired",
            "batch does not bind consecutive task transitions and exact eviction receipts",
        )?;
        member(conn, &plan, item, start, &updated.doc, &eviction.doc)?;
    }
    Ok(())
}

pub(crate) fn check(conn: &Connection) -> Result<()> {
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    let mut batches = conn.prepare("SELECT * FROM versions WHERE kind=?1")?;
    for batch in batches.query_map([KIND], entry)? {
        check_batch(conn, &authority, &batch?).map_err(|error| {
            if matches!(
                error.0.code.as_str(),
                "InvalidArgument" | "PermissionDenied" | "PreconditionFailed" | "QuotaExceeded"
            ) {
                err(
                    "RecoveryRequired",
                    format!(
                        "task result eviction batch verification: {}",
                        error.0.message
                    ),
                )
            } else {
                error
            }
        })?;
    }
    Ok(())
}
