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

fn active(
    conn: &Connection,
    operation: &str,
    epoch: i64,
    create_epoch: Option<i64>,
) -> Result<Option<String>> {
    let op = needed(conn, operation)?;
    let task_id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "restore operation has no Task"))?;
    let task = needed(conn, task_id)?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let dispatch_sequence = effect_dispatch_seq(conn, &effect.id)?;
    let create = task.spec["task_type"] == CREATE;
    if create && create_epoch.is_none_or(|epoch| dispatch_sequence <= epoch) {
        return Ok(None);
    }
    let queued: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [task_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (binding, source) = Source::load(conn, &op)?;
    let held = create
        || conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
            params![source.id, operation],
            |r| r.get::<_, bool>(0),
        )?;
    require(
        held && op.kind == "Operation"
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == binding.tool()
            && task.body["operation_refs"][0] == op.reference()
            && task.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && (create || needed(conn, &source.id)?.reference() == source.reference())
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
        "active volume publication queue, effect and source custody disagree",
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
        let placement = verify_custody(conn, &op, &checkpoint, intent.as_ref())?;
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
            intent.is_none_or(|intent| intent.body.get("checkpoint_ref").is_none())
                && (!create || effect.body["phase"] == "PermitBound"),
            "RecoveryRequired",
            "witnessed restore checkpoint is missing",
        )?;
        Ok(None)
    }
}

fn effect_dispatch_seq(conn: &Connection, effect: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=1",
        [effect],
        |row| row.get(0),
    )?)
}

pub(crate) fn protected_stages(db: &Db<'_>) -> Result<HashSet<String>> {
    selected(db.c).map_err(custody_error)
}

fn selected(conn: &Connection) -> Result<HashSet<String>> {
    let epoch = epoch(conn)?;
    let create_epoch = creation::cutover(conn)?;
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
           WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type' IN ('backup.restore','volume.clone','volume.create')
         UNION
         SELECT h.holder FROM holds h CROSS JOIN current o CROSS JOIN current t
           WHERE h.holder=o.id AND o.kind='Operation'
             AND t.id=o.spec->>'task_id' AND t.spec->>'task_type' IN ('backup.restore','volume.clone')
         UNION
         SELECT t.body->'operation_refs'->0->>'resource_id' FROM current t
           WHERE ?1 AND t.kind='Task' AND t.spec->>'task_type'='volume.create'
             AND t.body->>'outcome_knowledge' IN ('InFlight','Unknown')",
    )?;
    for operation in work.query_map([create_epoch.is_some()], |r| r.get::<_, String>(0))? {
        if let Some(name) = active(conn, &operation?, epoch, create_epoch)? {
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

pub(crate) fn verify_creates(conn: &Connection) -> Result<()> {
    verify_creates_inner(conn).map_err(custody_error)
}

fn verify_creates_inner(conn: &Connection) -> Result<()> {
    let epoch = creation::cutover(conn)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "new volume custody format is unavailable",
        )
    })?;
    let mut operations = conn.prepare(
        "SELECT * FROM versions WHERE kind='Effect' AND rev=1 AND seq>?1 AND body->>'action'='volume.create'",
    )?;
    for effect in operations.query_map([epoch], row)? {
        let effect = effect?;
        let operation = needed(conn, field_of(&effect.body, "operation_id")?)?;
        let task = needed(conn, field_of(&operation.spec, "task_id")?)?;
        let current_effect = resolve(conn, &operation.body["effect_refs"][0])?;
        let (binding, source) = Source::load(conn, &operation)?;
        let intent = needed(conn, &intent_id(&operation.id))?;
        require(
            effect.body["phase"] == "PermitBound"
                && effect.owner == operation.owner
                && effect.body["resource_ref"] == source.reference()
                && current_effect.id == effect.id
                && current_effect.kind == "Effect"
                && task.body["operation_refs"] == json!([operation.reference()])
                && task.body["outcome_knowledge"] == operation.body["outcome_knowledge"]
                && binding.tool() == CREATE
                && belongs(&intent, &operation, INTENT)
                && binding.matches(&intent, &source, false),
            "RecoveryRequired",
            "volume creation has no exact original Effect, Task and intent",
        )?;
        let volume_id = format!("volume-{}", operation.id);
        let volume = find(conn, &volume_id)?;
        let Some(checkpoint) = find(conn, &checkpoint_id(&operation.id))? else {
            require(
                intent.body.get("checkpoint_ref").is_none()
                    && current_effect.body["phase"] == "PermitBound"
                    && matches!(
                        operation.body["outcome_knowledge"].as_str(),
                        Some("InFlight" | "NotStarted")
                    )
                    && volume.is_none(),
                "RecoveryRequired",
                "volume creation lost its witnessed publication checkpoint",
            )?;
            continue;
        };
        verify_custody(conn, &operation, &checkpoint, Some(&intent))?;
        let receipt = &checkpoint.body["receipt"];
        binding.check_receipt(&operation, receipt)?;
        require(
            binding.matches(&checkpoint, &source, true),
            "RecoveryRequired",
            "volume creation checkpoint does not match its original pool and request",
        )?;
        if let Some(volume) = volume {
            let initial = conn.query_row(
                "SELECT * FROM versions WHERE id=?1 AND rev=1",
                [&volume.id],
                row,
            )?;
            require(
                operation.body["outcome_knowledge"] == "Known"
                    && matches!(
                        operation.body["lifecycle"].as_str(),
                        Some("Succeeded" | "Cancelled")
                    )
                    && initial.kind == "Volume"
                    && initial.owner == operation.owner
                    && initial.body["lifecycle"] == "Available"
                    && initial.body["data_incarnation"] == receipt["incarnation"]
                    && initial.body["capacity_bytes"] == receipt["capacity"]
                    && initial.body["pool_id"] == source.id
                    && initial.body["attachment_refs"] == json!([])
                    && initial.spec
                        == json!({"name":intent.body["create_source"]["name"],
                        "device":receipt["device"],"inode":receipt["inode"],"pool_id":source.id})
                    && volume.body["data_incarnation"] == initial.body["data_incarnation"]
                    && volume.spec["device"] == initial.spec["device"]
                    && volume.spec["inode"] == initial.spec["inode"],
                "RecoveryRequired",
                "cataloged volume disagrees with its original empty creation",
            )?;
            verify_create_receipt(
                conn,
                &task,
                &operation,
                &current_effect,
                &checkpoint,
                &initial,
            )?;
        } else if operation.body["outcome_knowledge"] == "Known" {
            let references = current_effect.body["evidence_refs"].as_array();
            let mut nonpublication = false;
            for reference in references.into_iter().flatten() {
                let evidence = resolve(conn, reference)?;
                if evidence.kind == "RecoveryEvidence"
                    && evidence.body["evidence"]["conclusion"] == NOT_APPLIED
                {
                    Disposal::authenticate(conn, &operation.id, evidence)?;
                    nonpublication = true;
                }
            }
            require(
                nonpublication,
                "RecoveryRequired",
                "settled volume creation has neither a Volume nor verified non-publication",
            )?;
        } else {
            require(
                matches!(
                    operation.body["outcome_knowledge"].as_str(),
                    Some("InFlight" | "Unknown")
                ),
                "RecoveryRequired",
                "volume creation checkpoint lacks a truthful outcome",
            )?;
        }
    }
    Ok(())
}

fn verify_create_receipt(
    conn: &Connection,
    task: &Doc,
    op: &Doc,
    effect: &Doc,
    checkpoint: &Doc,
    volume: &Doc,
) -> Result<()> {
    let refs = effect.body["evidence_refs"].as_array().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "volume creation has no settlement receipt",
        )
    })?;
    let reconciled = match (effect.body["phase"].as_str(), refs.as_slice()) {
        (Some("EffectConfirmed"), [_]) => None,
        (Some("EffectReconciled"), [_, evidence]) => Some(evidence),
        _ => {
            return Err(err(
                "RecoveryRequired",
                "volume creation settlement phase disagrees with evidence",
            ));
        }
    };
    let artifact = resolve(conn, &refs[0])?;
    let run = needed(conn, field_of(&task.body, "current_run_id")?)?;
    let expected_bytes = serde_json::to_vec(&volume.reference())?;
    let volume_seq = revision_seq(conn, volume)?;
    let receipt_seq = revision_seq(conn, &artifact)?;
    require(
        effect.body["outcome_knowledge"] == "Known"
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == CREATE
            && artifact.rev == 1
            && crate::artifact::is_json(&artifact)
            && crate::artifact::is_task_output(&artifact, task)
            && artifact.spec["operation_id"] == op.id
            && artifact.spec["digest"] == psios_api::sha256(&expected_bytes)
            && artifact.spec["size_bytes"] == expected_bytes.len().to_string()
            && task.body["result"]["availability"] == "Available"
            && task.body["result"]["artifact_refs"] == json!([artifact.reference()])
            && run.kind == "Run"
            && run.owner == task.owner
            && run.body["operation_refs"] == json!([op.reference()])
            && run.body["output_refs"] == json!([artifact.reference()])
            && revision_seq(conn, checkpoint)? < volume_seq
            && volume_seq < receipt_seq
            && receipt_seq < revision_seq(conn, effect)?,
        "RecoveryRequired",
        "volume creation settlement does not retain its exact catalog receipt",
    )?;
    if let Some(reference) = reconciled {
        let evidence = resolve(conn, reference)?;
        let original = resolve(conn, &evidence.body["operation_ref"])?;
        let effect_ref = resolve(conn, &artifact.spec["effect_ref"])?;
        let details = &evidence.body["evidence"];
        let observed = resolve(conn, &details["observation_ref"])?;
        let observer = needed(conn, field_of(&observed.spec, "task_id")?)?;
        let observer_operation = resolve(conn, &observer.body["operation_refs"][0])?;
        let observer_plan = resolve(conn, &observer_operation.body["plan_ref"])?;
        let observed_body: Value =
            serde_json::from_slice(&crate::artifact::checkpoint_bytes(conn, &observed, 4096)?)?;
        let evidence_seq = revision_seq(conn, &evidence)?;
        require(
            evidence.kind == "RecoveryEvidence"
                && evidence.rev == 1
                && evidence.owner == op.owner
                && details["conclusion"] == "RestorePublicationConfirmed"
                && details["checkpoint_ref"] == checkpoint.reference()
                && evidence.body["target_ref"] == checkpoint.body["create_source"]["pool_ref"]
                && original.kind == "Operation"
                && original.owner == op.owner
                && original.id == op.id
                && original.rev + 1 == op.rev
                && original.body["outcome_knowledge"] == "Unknown"
                && effect_ref.id == effect.id
                && effect_ref.reference() == original.body["effect_refs"][0]
                && artifact.spec["reconciliation_evidence_ref"] == evidence.reference()
                && crate::artifact::is_json(&observed)
                && crate::artifact::is_task_output(&observed, &observer)
                && observer.kind == "Task"
                && observer.spec["task_type"] == INSPECT
                && observer.body["lifecycle"] == "Succeeded"
                && observer.body["result"]["artifact_refs"] == json!([observed.reference()])
                && observer_operation.kind == "Operation"
                && observed.spec["operation_id"] == observer_operation.id
                && observer_plan.body["allowed_effects"][0]["resource_ref"] == original.reference()
                && observed_body["operation_ref"] == original.reference()
                && observed_body["checkpoint_ref"] == checkpoint.reference()
                && observed_body["publication"] == "Published"
                && observed_body
                    .get("restore_proof")
                    .is_some_and(Value::is_null)
                && volume_seq < evidence_seq
                && revision_seq(conn, &original)? < revision_seq(conn, &observed)?
                && revision_seq(conn, &observed)? < evidence_seq
                && evidence_seq < receipt_seq,
            "RecoveryRequired",
            "volume creation has no exact Published reconciliation evidence",
        )?;
    } else {
        require(
            artifact.spec["effect_id"] == effect.id
                && artifact.spec.get("reconciliation_evidence_ref").is_none(),
            "RecoveryRequired",
            "volume creation direct receipt has another effect",
        )?;
    }
    Ok(())
}

fn revision_seq(conn: &Connection, doc: &Doc) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![doc.id, doc.rev],
        |row| row.get(0),
    )?)
}

fn field_of<'a>(body: &'a Value, key: &str) -> Result<&'a str> {
    body[key].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            format!("volume creation is missing {key}"),
        )
    })
}

struct CloneCustody {
    binding: Source,
    source: Doc,
    checkpoint: Option<Doc>,
}

fn clone_custody(conn: &Connection, op: &Doc) -> Result<CloneCustody> {
    let (binding, source) = Source::load(conn, op)?;
    let intent = needed(conn, &intent_id(&op.id))?;
    require(
        binding.tool() == super::super::cloning::TOOL
            && belongs(&intent, op, INTENT)
            && binding.matches(&intent, &source, false),
        "RecoveryRequired",
        "dispatched clone has no exact witnessed intent",
    )?;
    let checkpoint = find(conn, &checkpoint_id(&op.id))?;
    if let Some(checkpoint) = &checkpoint {
        verify_custody(conn, op, checkpoint, Some(&intent))?;
        require(
            binding.matches(checkpoint, &source, true),
            "RecoveryRequired",
            "clone checkpoint has another source",
        )?;
        binding.check_receipt(op, &checkpoint.body["receipt"])?;
    } else {
        require(
            intent.body.get("checkpoint_ref").is_none(),
            "RecoveryRequired",
            "clone publication checkpoint disappeared",
        )?;
    }
    Ok(CloneCustody {
        binding,
        source,
        checkpoint,
    })
}

impl CloneCustody {
    fn catalog(&self, conn: &Connection, op: &Doc, current: &Doc) -> Result<&Doc> {
        let checkpoint = self
            .checkpoint
            .as_ref()
            .ok_or_else(|| err("RecoveryRequired", "cataloged clone lacks a checkpoint"))?;
        let receipt = &checkpoint.body["receipt"];
        let original: Doc = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=1",
            [&current.id],
            row,
        )?;
        let expected = self
            .binding
            .catalog(&self.source, &receipt["restore_proof"]);
        require(
            op.body["outcome_knowledge"] == "Known"
                && matches!(
                    op.body["lifecycle"].as_str(),
                    Some("Succeeded" | "Cancelled")
                )
                && original.kind == "Volume"
                && original.owner == op.owner
                && current.kind == "Volume"
                && original.id == receipt["id"]
                && original.id != self.source.id
                && original.body["data_incarnation"] == receipt["incarnation"]
                && original.body["data_incarnation"] != self.source.body["data_incarnation"]
                && current.body["data_incarnation"] == original.body["data_incarnation"]
                && original.spec["device"] == receipt["device"]
                && original.spec["inode"] == receipt["inode"]
                && original.spec["storage_restore_phase"] == "AwaitingActivation"
                && original.spec["name"] == expected["name"]
                && original.spec["cloned_from"] == expected["cloned_from"]
                && original.spec["restore_proof"] == expected["restore_proof"]
                && current.spec["device"] == original.spec["device"]
                && current.spec["inode"] == original.spec["inode"]
                && current.spec["cloned_from"] == original.spec["cloned_from"]
                && current.spec["restore_proof"] == original.spec["restore_proof"],
            "RecoveryRequired",
            "cataloged clone contradicts its exact publication and source lineage",
        )?;
        Ok(checkpoint)
    }
}

pub(crate) fn clone_origin(conn: &Connection, volume: &Doc) -> Result<(Doc, Value)> {
    let id = volume.id.strip_prefix("volume-").ok_or_else(|| {
        err(
            "RecoveryRequired",
            "clone has no originating Operation identity",
        )
    })?;
    let op = needed(conn, id)?;
    let custody = clone_custody(conn, &op)?;
    let checkpoint = custody.catalog(conn, &op, volume)?;
    let task_id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "clone has no originating Task identity"))?;
    let task = needed(conn, task_id)?;
    require(
        task.body["operation_refs"] == json!([op.reference()])
            && task.body["outcome_knowledge"] == "Known"
            && task.body["lifecycle"] == op.body["lifecycle"],
        "RecoveryRequired",
        "published clone does not match its settled Task",
    )?;
    let reference = checkpoint.reference();
    Ok((custody.source, reference))
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
        let custody = clone_custody(conn, &op)?;
        if let Some(volume) = find(conn, &format!("volume-{}", op.id))? {
            custody.catalog(conn, &op, &volume)?;
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
