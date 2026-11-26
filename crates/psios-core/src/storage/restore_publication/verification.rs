//! Check restore custody without opening directories or reading volume data.
use super::{disposal::finished, *};
use crate::db::{find, resolve, row};
use rusqlite::Connection;

pub(super) fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "restore custody resource is missing"))
}

fn epoch(conn: &Connection) -> Result<i64> {
    let sequence: String = conn.query_row(
        "SELECT value FROM meta WHERE key='restore_custody_epoch'",
        [],
        |r| r.get(0),
    )?;
    sequence
        .parse::<i64>()
        .ok()
        .filter(|sequence| *sequence >= 0)
        .ok_or_else(|| err("RecoveryRequired", "restore custody epoch is invalid"))
}

fn active(conn: &Connection, operation: &str, epoch: i64) -> Result<Option<String>> {
    let op = needed(conn, operation)?;
    let task_id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "restore operation has no Task"))?;
    let task = needed(conn, task_id)?;
    let queued: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (binding, source) = Source::load(conn, &op)?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![source.id, operation],
        |r| r.get(0),
    )?;
    require(
        held && op.kind == "Operation"
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == binding.tool()
            && task.body["operation_refs"][0] == op.reference()
            && task.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && needed(conn, &source.id)?.reference() == source.reference()
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && effect.body["operation_id"] == operation
            && effect.body["action"] == binding.tool()
            && effect.body["resource_ref"] == source.reference()
            && queued.is_some_and(|(id, phase)| {
                id == operation
                    && (phase == "Dispatched" && op.body["outcome_knowledge"] == "InFlight"
                        || phase == "Unknown" && op.body["outcome_knowledge"] == "Unknown")
            }),
        "RecoveryRequired",
        "active restore queue, effect and backup hold disagree",
    )?;
    let dispatch_sequence: i64 = conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=1",
        [&effect.id],
        |r| r.get(0),
    )?;
    let intent = find(conn, &intent_id(operation))?;
    require(
        match &intent {
            Some(intent) => belongs(intent, &op, INTENT) && binding.matches(intent, &source, false),
            None => source.kind == "Backup" && dispatch_sequence <= epoch,
        },
        "RecoveryRequired",
        "active restore intent is missing or disagrees with its effect",
    )?;
    let checkpoint = find(conn, &checkpoint_id(operation))?;
    if let Some(checkpoint) = checkpoint {
        let placement = verify_custody(&op, &checkpoint, intent.as_ref())?;
        binding.check_receipt(&op, &checkpoint.body["receipt"])?;
        let staged = placement.stage_name()?;
        require(
            binding.matches(&checkpoint, &source, true)
                && (staged.is_some() || source.kind == "Backup" && dispatch_sequence <= epoch),
            "RecoveryRequired",
            "active restore checkpoint disagrees with its backup or placement",
        )?;
        Ok(staged.map(str::to_owned))
    } else {
        require(
            intent.is_none_or(|intent| intent.body.get("checkpoint_ref").is_none()),
            "RecoveryRequired",
            "witnessed restore checkpoint is missing",
        )?;
        Ok(None)
    }
}

pub(crate) fn protected_stages(db: &Db<'_>) -> Result<HashSet<String>> {
    selected(db.c).map_err(custody_error)
}

fn selected(conn: &Connection) -> Result<HashSet<String>> {
    let epoch = epoch(conn)?;
    // A missing index row must not make its still-owned stage look abandoned.
    // The authorization and physical-completion facts remain durable after
    // the bounded pending-work index is trimmed.
    let balanced: bool = conn.query_row(
        "SELECT (SELECT count(*) FROM restore_not_applied)
           = (SELECT count(*) FROM restore_stage_disposals)
             + (SELECT count(*) FROM audit WHERE method='Storage.RestoreStageReclaim')",
        [],
        |r| r.get(0),
    )?;
    require(
        balanced,
        "RecoveryRequired",
        "restore disposal index has missing or orphaned work",
    )?;
    let mut names = HashSet::new();
    let mut work = conn.prepare(
        "SELECT q.operation FROM queue q JOIN current t ON q.task=t.id
           WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type' IN ('backup.restore','volume.clone')
         UNION
         SELECT h.holder FROM holds h CROSS JOIN current o CROSS JOIN current t
           WHERE h.holder=o.id AND o.kind='Operation'
             AND t.id=o.spec->>'task_id' AND t.spec->>'task_type' IN ('backup.restore','volume.clone')",
    )?;
    for operation in work.query_map([], |r| r.get::<_, String>(0))? {
        if let Some(name) = active(conn, &operation?, epoch)? {
            names.insert(name);
        }
    }
    let mut pending = conn.prepare("SELECT operation FROM restore_stage_disposals")?;
    for operation in pending.query_map([], |r| r.get::<_, String>(0))? {
        let job = Disposal::indexed(conn, &operation?)?;
        names.insert(job.stage_name()?.into());
    }
    Ok(names)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_all(conn).map_err(custody_error)
}

fn verify_all(conn: &Connection) -> Result<()> {
    selected(conn)?;
    let mut history = conn.prepare("SELECT * FROM restore_not_applied")?;
    for record in history.query_map([], row)? {
        let evidence = record?;
        let operation = evidence.body["operation_ref"]["resource_id"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "restore evidence has no operation"))?;
        let pending: Option<Value> = conn
            .query_row(
                "SELECT evidence_ref FROM restore_stage_disposals WHERE operation=?1",
                [operation],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(reference) = pending {
            require(
                reference == evidence.reference() && !finished(conn, operation)?,
                "RecoveryRequired",
                "restore evidence disagrees with pending disposal",
            )?;
        } else {
            require(
                finished(conn, operation)?,
                "RecoveryRequired",
                "restore evidence has no pending or completed disposal",
            )?;
            Disposal::authenticate(conn, operation, evidence.clone())?;
        }
    }
    Ok(())
}

fn custody_error(error: crate::Error) -> crate::Error {
    if error.0.code == "RecoveryRequired" {
        error
    } else {
        err(
            "RecoveryRequired",
            format!("restore custody verification: {}", error.0.message),
        )
    }
}

pub(crate) fn verify_clones(conn: &Connection) -> Result<()> {
    verify_clones_inner(conn).map_err(custody_error)
}

fn verify_clones_inner(conn: &Connection) -> Result<()> {
    let mut tasks = conn
        .prepare("SELECT * FROM current WHERE kind='Task' AND spec->>'task_type'='volume.clone'")?;
    for task in tasks.query_map([], row)? {
        let task = task?;
        let op = resolve(conn, &task.body["operation_refs"][0])?;
        if op.body["effect_refs"][0].is_null() {
            continue;
        }
        let (binding, source) = Source::load(conn, &op)?;
        let intent = needed(conn, &intent_id(&op.id))?;
        require(
            binding.tool() == super::super::cloning::TOOL
                && belongs(&intent, &op, INTENT)
                && binding.matches(&intent, &source, false),
            "RecoveryRequired",
            "dispatched clone has no exact witnessed intent",
        )?;
        let volume_id = format!("volume-{}", op.id);
        let current = find(conn, &volume_id)?;
        let checkpoint = find(conn, &checkpoint_id(&op.id))?;
        if let Some(checkpoint) = &checkpoint {
            verify_custody(&op, checkpoint, Some(&intent))?;
            require(
                binding.matches(checkpoint, &source, true),
                "RecoveryRequired",
                "clone checkpoint has another source",
            )?;
            binding.check_receipt(&op, &checkpoint.body["receipt"])?;
        } else {
            require(
                intent.body.get("checkpoint_ref").is_none(),
                "RecoveryRequired",
                "clone publication checkpoint disappeared",
            )?;
        }
        if let Some(current) = current {
            let checkpoint = checkpoint
                .ok_or_else(|| err("RecoveryRequired", "cataloged clone lacks a checkpoint"))?;
            let receipt = &checkpoint.body["receipt"];
            let original: Doc = conn.query_row(
                "SELECT * FROM versions WHERE id=?1 AND rev=1",
                [&volume_id],
                row,
            )?;
            let expected = binding.catalog(&source, &receipt["restore_proof"]);
            require(
                op.body["outcome_knowledge"] == "Known"
                    && matches!(
                        op.body["lifecycle"].as_str(),
                        Some("Succeeded" | "Cancelled")
                    )
                    && original.kind == "Volume"
                    && original.owner == op.owner
                    && current.kind == "Volume"
                    && original.id != source.id
                    && original.body["data_incarnation"] == receipt["incarnation"]
                    && original.body["data_incarnation"] != source.body["data_incarnation"]
                    && current.body["data_incarnation"] == original.body["data_incarnation"]
                    && original.spec["device"] == receipt["device"]
                    && original.spec["inode"] == receipt["inode"]
                    && original.spec["storage_restore_phase"] == "AwaitingActivation"
                    && original.spec["name"] == expected["name"]
                    && original.spec["cloned_from"] == expected["cloned_from"]
                    && original.spec["restore_proof"] == expected["restore_proof"]
                    && current.spec["cloned_from"] == original.spec["cloned_from"]
                    && current.spec["restore_proof"] == original.spec["restore_proof"],
                "RecoveryRequired",
                "cataloged clone contradicts its exact publication and source lineage",
            )?;
        } else {
            require(
                op.body["lifecycle"] != "Succeeded",
                "RecoveryRequired",
                "succeeded clone has no cataloged volume",
            )?;
        }
    }
    Ok(())
}
