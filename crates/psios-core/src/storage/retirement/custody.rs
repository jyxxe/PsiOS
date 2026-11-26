//! Validate the paired retirement journal and reclamation index without opening storage paths.
use super::{KIND, Location, Reclamation, id, record};
use crate::{Result, broker, db, decode, err, require};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

fn current(conn: &Connection, id: &str, kind: &str) -> Result<db::Doc> {
    db::find(conn, id)?
        .filter(|doc| doc.kind == kind)
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "storage reclamation reference is unavailable",
            )
        })
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "storage reclamation reference is malformed",
        )
    })
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current AS r
         WHERE r.kind=?1 AND r.body->>'lifecycle'='PendingCleanup'
         AND NOT EXISTS(SELECT 1 FROM storage_reclamations q WHERE q.retirement=r.id))",
        [KIND],
        |row| row.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "pending storage retirement is missing its reclamation row",
    )?;
    let mut statement = conn
        .prepare("SELECT retirement,ready_after FROM storage_reclamations ORDER BY retirement")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let due: String = row.get(1)?;
        selected(conn, &id, &due)?;
    }
    Ok(())
}

pub(super) fn selected(conn: &Connection, retirement_id: &str, due: &str) -> Result<Reclamation> {
    let job = current(conn, retirement_id, KIND)?;
    require(
        job.body["lifecycle"] == "PendingCleanup",
        "RecoveryRequired",
        "storage reclamation row does not name a pending retirement",
    )?;
    // Both the original eligibility time and later backoff use this wire
    // format. The actual timestamp stays in the index, including across restart.
    require(
        DateTime::parse_from_rfc3339(due).is_ok_and(|parsed| {
            parsed
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Millis, true)
                == due
        }),
        "RecoveryRequired",
        "storage reclamation row has an invalid due time",
    )?;
    exact_deletion(conn, &job).map_err(|error| {
        if error.0.code == "StorageUnavailable" {
            error
        } else {
            err(
                "RecoveryRequired",
                "storage reclamation is not bound to its committed deletion",
            )
        }
    })
}

fn exact_deletion(conn: &Connection, job: &db::Doc) -> Result<Reclamation> {
    let operation_id = text(&job.spec, "operation_id")?;
    let op = current(conn, operation_id, "Operation")?;
    let retirement = record(conn, &op)?;
    let original = db::resolve(conn, &retirement.spec["target_ref"])?;
    let deleted = db::resolve(conn, &retirement.body["deleted_ref"])?;
    let plan = db::resolve(conn, &op.body["plan_ref"])?;
    let task = current(conn, text(&op.spec, "task_id")?, "Task")?;
    let run = current(conn, text(&task.body, "current_run_id")?, "Run")?;
    let effect = db::resolve(conn, &op.body["effect_refs"][0])?;
    let tool = match original.kind.as_str() {
        "Volume" => "volume.delete",
        "Backup" => "backup.delete",
        _ => return Err(err("RecoveryRequired", "unsupported retired storage kind")),
    };
    let prepared = conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=2",
            [&job.id],
            db::row,
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "storage reclamation has no prepared checkpoint",
            )
        })?;
    let queued_hold: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1)",
        [&deleted.id],
        |row| row.get(0),
    )?;
    let location: Location = decode(&retirement.body["location"])?;
    let valid_number = |value: &str| value.parse::<u64>().is_ok_and(|v| v.to_string() == value);
    require(
        job.id == id(operation_id)
            && job.reference() == retirement.reference()
            && retirement.rev > 2
            && retirement.body["lifecycle"] == "PendingCleanup"
            && prepared.kind == KIND
            && prepared.owner == retirement.owner
            && prepared.spec == retirement.spec
            && prepared.body["lifecycle"] == "Prepared"
            && prepared.body["location"] == retirement.body["location"]
            && valid_number(&location.parent.device)
            && valid_number(&location.parent.inode)
            && valid_number(&location.directory.inode)
            && location.parent.device == location.directory.device
            && location.parent != location.directory
            && deleted.id == original.id
            && deleted.kind == original.kind
            && deleted.owner == original.owner
            && deleted.rev.checked_sub(1) == Some(original.rev)
            && deleted.spec == original.spec
            && deleted.body["lifecycle"] == "Deleted"
            && original.body["lifecycle"] != "Deleted"
            && current(conn, &deleted.id, &deleted.kind)?.reference() == deleted.reference()
            && !queued_hold,
        "RecoveryRequired",
        "storage retirement does not match the prepared and deleted resource revisions",
    )?;
    require(
        op.body["outcome_knowledge"] == "Known"
            && matches!(
                op.body["lifecycle"].as_str(),
                Some("Succeeded" | "Cancelled")
            )
            && task.owner == op.owner
            && task.spec["task_type"] == tool
            && task.body["lifecycle"] == op.body["lifecycle"]
            && task.body["outcome_knowledge"] == "Known"
            && task.body["operation_refs"] == json!([op.reference()])
            && run.owner == task.owner
            && run.body["task_id"] == task.id
            && run.body["operation_refs"] == task.body["operation_refs"]
            && run.body["outcome_knowledge"] == "Known"
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && op.body["effect_refs"] == json!([effect.reference()])
            && effect.body["operation_id"] == op.id
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["action"] == tool
            && effect.body["resource_ref"] == original.reference(),
        "RecoveryRequired",
        "storage retirement has no known original deletion settlement",
    )?;
    broker::verify_bound_approval(conn, &op, &task, &run, &plan, &effect, tool)?;
    verify_receipt(conn, &op, &task, &run, &effect, &prepared, &original)?;
    Ok(Reclamation {
        record: retirement,
        target: deleted,
        operation: op.id,
        location,
    })
}

fn verify_receipt(
    conn: &Connection,
    op: &db::Doc,
    task: &db::Doc,
    run: &db::Doc,
    effect: &db::Doc,
    prepared: &db::Doc,
    target: &db::Doc,
) -> Result<()> {
    let receipt = db::resolve(conn, &effect.body["evidence_refs"][0])?;
    require(
        crate::artifact::is_task_output(&receipt, task)
            && receipt.spec["operation_id"] == op.id
            && task.body["result"]["artifact_refs"] == json!([receipt.reference()])
            && run.body["output_refs"] == task.body["result"]["artifact_refs"],
        "RecoveryRequired",
        "storage deletion receipt is not attributed to the original Task and Run",
    )?;
    if effect.body["phase"] == "EffectConfirmed" {
        return require(
            receipt.spec["effect_id"] == effect.id
                && effect.body["evidence_refs"] == json!([receipt.reference()]),
            "RecoveryRequired",
            "storage deletion receipt disagrees with the confirmed effect",
        );
    }
    let evidence = db::resolve(conn, &receipt.spec["reconciliation_evidence_ref"])?;
    let unknown = db::resolve(conn, &evidence.body["operation_ref"])?;
    require(
        effect.body["phase"] == "EffectReconciled"
            && evidence.kind == "RecoveryEvidence"
            && evidence.owner == task.owner
            && effect.body["evidence_refs"] == json!([receipt.reference(), evidence.reference()])
            && unknown.kind == "Operation"
            && unknown.id == op.id
            && unknown.body["outcome_knowledge"] == "Unknown"
            && unknown.body["effect_refs"] == json!([receipt.spec["effect_ref"]])
            && receipt.spec["effect_ref"]["resource_id"] == effect.id
            && evidence.body["target_ref"] == target.reference()
            && evidence.body["evidence"]["checkpoint_ref"] == prepared.reference()
            && evidence.body["evidence"]["conclusion"] == "RetirementConfirmed",
        "RecoveryRequired",
        "storage deletion reconciliation has no exact original checkpoint",
    )
}
