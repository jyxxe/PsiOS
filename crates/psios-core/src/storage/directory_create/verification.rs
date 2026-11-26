//! Read-only authentication of directory creation evidence and pending work.
use super::*;
use crate::{artifact, broker::dependencies, storage::files};

pub(super) fn field<'a>(body: &'a Value, name: &str) -> Result<&'a str> {
    body[name]
        .as_str()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                format!("directory creation evidence is missing {name}"),
            )
        })
}

struct Binding {
    op: Doc,
    task: Doc,
    volume: Doc,
    current_volume: Doc,
    effect: Doc,
}

fn current(conn: &Connection, id: &str, kind: &str) -> Result<Doc> {
    db::find(conn, id)?
        .filter(|doc| doc.kind == kind)
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "directory creation references an unavailable resource",
            )
        })
}

fn at_revision(conn: &Connection, doc: &Doc, revision: i64) -> Result<Doc> {
    Ok(conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![doc.id, revision],
        db::row,
    )?)
}

fn seq(conn: &Connection, doc: &Doc) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![doc.id, doc.rev],
        |row| row.get(0),
    )?)
}

fn bind(conn: &Connection, document: &Doc) -> Result<Binding> {
    let op = current(conn, field(&document.spec, "operation_id")?, "Operation")?;
    let task = current(conn, field(&op.spec, "task_id")?, "Task")?;
    let volume = db::resolve(conn, &document.spec["volume_ref"])?;
    let current_volume = current(conn, &volume.id, "Volume")?;
    let plan = db::resolve(conn, &document.spec["plan_ref"])?;
    let effect = db::resolve(conn, &op.body["effect_refs"][0])?;
    let input = db::resolve(conn, &task.spec["input_ref"])?;
    let input = dependencies::tool_input(&input)?;
    let path = field(&document.spec, "path")?;
    require(
        document.kind == KIND
            && document.id == id(&op.id)
            && document.generation == 1
            && document.owner == op.owner
            && task.owner == op.owner
            && task.spec["task_type"] == TOOL
            && task.body["operation_refs"] == json!([op.reference()])
            && task.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && volume.kind == "Volume"
            && volume.body["lifecycle"] == "Available"
            && volume.body["data_incarnation"].is_string()
            && current_volume.body["data_incarnation"] == volume.body["data_incarnation"]
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && op.body["plan_ref"] == plan.reference()
            && plan.body["allowed_effects"][0]["resource_ref"] == volume.reference()
            && plan.body["allowed_effects"][0]["action"] == TOOL
            && input["target"]["id"] == volume.id
            && input["target"]["expected_resource_version"] == volume.version()
            && input["path"] == path
            && files::path(path, false).is_ok()
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && document.spec["effect_id"] == effect.id
            && effect.body["operation_id"] == op.id
            && effect.body["resource_ref"] == volume.reference()
            && effect.body["action"] == TOOL,
        "RecoveryRequired",
        "directory creation evidence disagrees with the original approved Volume and path",
    )?;
    Ok(Binding {
        op,
        task,
        volume,
        current_volume,
        effect,
    })
}

fn check_prepared(conn: &Connection, record: &Doc, binding: &Binding) -> Result<()> {
    let location: Location = decode(&record.body["location"])?;
    location.check(&binding.volume)?;
    let allocation = at_revision(conn, record, 2)?;
    let prepared = at_revision(conn, record, 3)?;
    let started = db::resolve(conn, &record.body["started_effect_ref"])?;
    let bound = at_revision(conn, &started, 1)?;
    let mut started_body = bound.body.clone();
    started_body["phase"] = json!("EffectStarted");
    require(
        allocation.spec == record.spec
            && allocation.body == json!({"lifecycle":"Allocating","parents":location.parents})
            && prepared.spec == record.spec
            && prepared.body
                == json!({"lifecycle":"Prepared","location":location,"started_effect_ref":started.reference()})
            && started.kind == "Effect"
            && started.rev == 2
            && started.id == binding.effect.id
            && started.owner == binding.op.owner
            && started.spec == bound.spec
            && started.spec == binding.effect.spec
            && started.body == started_body
            && bound.body["phase"] == "PermitBound"
            && bound.body["operation_id"] == binding.op.id
            && bound.body["resource_ref"] == binding.volume.reference()
            && bound.body["action"] == TOOL
            && seq(conn, &allocation)? < seq(conn, &started)?
            && seq(conn, &started)? < seq(conn, &prepared)?,
        "RecoveryRequired",
        "directory creation lacks an ordered allocation and rename witness",
    )
}

fn logical_ref(conn: &Connection, record: &Doc, lifecycle: &str) -> Result<Doc> {
    let logical = if record.body["lifecycle"] == "Reclaimed" {
        at_revision(conn, record, record.rev - 1)?
    } else {
        record.clone()
    };
    require(
        logical.body["lifecycle"] == lifecycle && logical.spec == record.spec,
        "RecoveryRequired",
        "directory creation lost its original logical settlement",
    )?;
    if record.body["lifecycle"] == "Reclaimed" {
        let mut reclaimed = logical.body.clone();
        reclaimed["lifecycle"] = json!("Reclaimed");
        reclaimed["disposal"] = record.body["disposal"].clone();
        require(
            reclaimed == record.body
                && matches!(
                    record.body["disposal"].as_str(),
                    Some("Removed" | "ConfirmedAbsent")
                ),
            "RecoveryRequired",
            "directory stage reclamation receipt is invalid",
        )?;
    }
    Ok(logical)
}

fn result_evidence(
    conn: &Connection,
    binding: &Binding,
    logical: &Doc,
    result: &Value,
    conclusion: &str,
) -> Result<()> {
    let Binding {
        op, task, effect, ..
    } = binding;
    let refs = effect.body["evidence_refs"]
        .as_array()
        .ok_or_else(|| err("RecoveryRequired", "directory settlement has no receipt"))?;
    let reconciliation = match (effect.body["phase"].as_str(), refs.as_slice()) {
        (Some("EffectConfirmed"), [_, proof])
            if conclusion == "DirectoryCreationConfirmed" && *proof == logical.reference() =>
        {
            None
        }
        (Some("EffectReconciled"), [_, evidence, proof]) if *proof == logical.reference() => {
            Some(evidence)
        }
        _ => {
            return Err(err(
                "RecoveryRequired",
                "directory settlement lacks exact terminal effect evidence",
            ));
        }
    };
    let receipt = db::resolve(conn, &refs[0])?;
    let expected = serde_json::to_vec(result)?;
    require(
        op.body["outcome_knowledge"] == "Known"
            && effect.body["outcome_knowledge"] == "Known"
            && artifact::is_json(&receipt)
            && artifact::is_task_output(&receipt, task)
            && receipt.rev == 1
            && receipt.spec["operation_id"] == op.id
            && receipt.spec["digest"] == psios_api::sha256(&expected)
            && task.body["result"]["artifact_refs"] == json!([receipt.reference()])
            && seq(conn, logical)? < seq(conn, &receipt)?
            && seq(conn, &receipt)? < seq(conn, effect)?,
        "RecoveryRequired",
        "directory settlement receipt does not authenticate the committed result",
    )?;
    if let Some(reference) = reconciliation {
        let evidence = db::resolve(conn, reference)?;
        let lost = db::resolve(conn, &evidence.body["operation_ref"])?;
        let prepared = at_revision(conn, logical, 3)?;
        require(
            evidence.kind == "RecoveryEvidence"
                && evidence.owner == op.owner
                && evidence.body["target_ref"] == binding.volume.reference()
                && evidence.body["evidence"]["checkpoint_ref"] == prepared.reference()
                && evidence.body["evidence"]["conclusion"] == conclusion
                && lost.kind == "Operation"
                && lost.id == op.id
                && lost.rev + 1 == op.rev
                && lost.body["outcome_knowledge"] == "Unknown"
                && receipt.spec["effect_ref"] == lost.body["effect_refs"][0]
                && receipt.spec["reconciliation_evidence_ref"] == evidence.reference()
                && seq(conn, logical)? < seq(conn, &evidence)?
                && seq(conn, &evidence)? < seq(conn, &receipt)?,
            "RecoveryRequired",
            "directory settlement is missing its exact independent observation",
        )?;
    } else {
        require(
            receipt.spec["effect_id"] == effect.id,
            "RecoveryRequired",
            "directory result names another effect",
        )?;
    }
    Ok(())
}

fn validate_disposal(conn: &Connection, record: &Doc, binding: &Binding) -> Result<()> {
    let Binding {
        op, volume, effect, ..
    } = binding;
    let pending = logical_ref(conn, record, "PendingCleanup")?;
    require(
        !held(conn, op, volume)?
            && matches!(op.body["lifecycle"].as_str(), Some("Failed" | "Cancelled")),
        "RecoveryRequired",
        "directory cleanup requires a settled original operation without a hold",
    )?;
    if pending.body.get("location").is_some() {
        check_prepared(conn, record, binding)?;
        let mut expected = at_revision(conn, record, 3)?.body;
        expected["lifecycle"] = json!("PendingCleanup");
        require(
            pending.rev == 4 && expected == pending.body,
            "RecoveryRequired",
            "prepared directory cleanup was not exact",
        )?;
        result_evidence(
            conn,
            binding,
            &pending,
            &json!({"path":record.spec["path"],"created":false}),
            "DirectoryCreationNotApplied",
        )
    } else {
        let parents: Parents = decode(&pending.body["parents"])?;
        parents.check(volume)?;
        require(
            pending.rev == 3
                && at_revision(conn, record, 2)?.body
                    == json!({"lifecycle":"Allocating","parents":parents})
                && pending.body == json!({"lifecycle":"PendingCleanup","parents":parents})
                && op.body["outcome_knowledge"] == "NotStarted"
                && effect.body["phase"] == "PermitBound",
            "RecoveryRequired",
            "directory prepublication cleanup has no reserved private name",
        )
    }
}

pub(super) fn disposal_volume(conn: &Connection, record: &Doc) -> Result<Doc> {
    let binding = bind(conn, record)?;
    validate_disposal(conn, record, &binding)?;
    require(
        record.body["lifecycle"] == "PendingCleanup"
            && binding.current_volume.body["lifecycle"] == "Available",
        "RecoveryRequired",
        "directory cleanup target is not an available Volume",
    )?;
    Ok(binding.current_volume)
}

fn validate_active(conn: &Connection, record: &Doc, binding: &Binding) -> Result<()> {
    let Binding {
        op,
        task,
        volume,
        current_volume,
        effect,
    } = binding;
    let phase = field(&record.body, "lifecycle")?;
    let queue: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    require(
        held(conn, op, volume)?
            && current_volume.reference() == volume.reference()
            && queue.is_some_and(|(operation, phase)| {
                operation == op.id
                    && (phase == "Dispatched" && op.body["outcome_knowledge"] == "InFlight"
                        || phase == "Unknown" && op.body["outcome_knowledge"] == "Unknown")
            }),
        "RecoveryRequired",
        "active directory creation lost its exact exclusive Volume hold or dispatch",
    )?;
    match phase {
        "Preparing" | "Allocating" => {
            if phase == "Allocating" {
                decode::<Parents>(&record.body["parents"])?.check(volume)?;
            }
            require(
                record.rev == if phase == "Preparing" { 1 } else { 2 }
                    && op.body["outcome_knowledge"] == "InFlight"
                    && effect.body["phase"] == "PermitBound",
                "RecoveryRequired",
                "unpublished directory has no exact pre-rename state",
            )
        }
        "Prepared" => {
            require(
                record.rev == 3
                    && matches!(
                        op.body["outcome_knowledge"].as_str(),
                        Some("InFlight" | "Unknown")
                    )
                    && effect.body["phase"] == "EffectStarted",
                "RecoveryRequired",
                "prepared directory does not match its in-flight effect",
            )?;
            check_prepared(conn, record, binding)
        }
        _ => Err(err(
            "RecoveryRequired",
            "indexed directory creation has an invalid active state",
        )),
    }
}

fn active(conn: &Connection) -> Result<()> {
    let mut query = conn.prepare("SELECT operation,volume,phase,eligible_at FROM volume_mkdir_stages ORDER BY operation LIMIT ?1")?;
    let mut rows = query.query([LIMIT + 1])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let operation: String = row.get(0)?;
        let volume_id: String = row.get(1)?;
        let phase: String = row.get(2)?;
        let due: String = row.get(3)?;
        count += 1;
        let record = current(conn, &id(&operation), KIND)?;
        let binding = bind(conn, &record)?;
        require(
            count <= LIMIT
                && binding.volume.id == volume_id
                && chrono::DateTime::parse_from_rfc3339(&due).is_ok(),
            "RecoveryRequired",
            "directory creation stage inventory is invalid",
        )?;
        if phase == "Active" {
            validate_active(conn, &record, &binding)?;
        } else {
            disposal_volume(conn, &record)?;
        }
    }
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current c WHERE kind='VolumeDirectoryCreation'
           AND body->>'lifecycle' IN ('Preparing','Allocating','Prepared','PendingCleanup')
           AND NOT EXISTS(SELECT 1 FROM volume_mkdir_stages WHERE operation=c.spec->>'operation_id'))",
        [], |row| row.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "a witnessed directory stage is missing from the cleanup inventory",
    )
}

fn check(conn: &Connection) -> Result<()> {
    active(conn)?;
    let epoch: String = conn.query_row("SELECT value FROM meta WHERE key=?1", [EPOCH], |row| {
        row.get(0)
    })?;
    let latest: i64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM versions", [], |row| {
        row.get(0)
    })?;
    let epoch = epoch
        .parse::<i64>()
        .ok()
        .filter(|epoch| (0..=latest).contains(epoch))
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "directory creation cutover epoch is invalid",
            )
        })?;
    let mut dispatches = conn.prepare("SELECT * FROM versions WHERE kind='Effect' AND rev=1 AND seq>?1 AND body->>'action'='volume.mkdir'")?;
    for initial_effect in dispatches.query_map([epoch], db::row)? {
        let initial_effect = initial_effect?;
        let operation = field(&initial_effect.body, "operation_id")?;
        let record = current(conn, &id(operation), KIND)?;
        let binding = bind(conn, &record)?;
        require(
            initial_effect.body["phase"] == "PermitBound" && initial_effect.id == binding.effect.id,
            "RecoveryRequired",
            "directory creation is missing its original Effect",
        )?;
        match field(&record.body, "lifecycle")? {
            "Preparing" | "Allocating" | "Prepared" | "PendingCleanup" => {} // checked in active()
            "NotStarted" => {
                require(
                    record.rev == 2
                        && record.body == json!({"lifecycle":"NotStarted"})
                        && binding.op.body["outcome_knowledge"] == "NotStarted"
                        && binding.effect.body["phase"] == "PermitBound",
                    "RecoveryRequired",
                    "directory creation pre-effect settlement is invalid",
                )?;
            }
            "Reclaimed" => validate_disposal(conn, &record, &binding)?,
            "Published" => {
                check_prepared(conn, &record, &binding)?;
                let updated = db::resolve(conn, &record.body["result_volume_ref"])?;
                let prepared = at_revision(conn, &record, 3)?;
                let mut expected = prepared.body.clone();
                expected["lifecycle"] = json!("Published");
                expected["result_volume_ref"] = updated.reference();
                let mut volume_body = binding.volume.body.clone();
                volume_body["resource_version"] = json!(updated.version());
                require(
                    record.rev == 4
                        && record.body == expected
                        && updated.kind == "Volume"
                        && updated.id == binding.volume.id
                        && updated.rev == binding.volume.rev + 1
                        && updated.spec == binding.volume.spec
                        && updated.body == volume_body
                        && seq(conn, &prepared)? < seq(conn, &updated)?
                        && seq(conn, &updated)? < seq(conn, &record)?,
                    "RecoveryRequired",
                    "directory publication does not match the exact successor Volume revision",
                )?;
                result_evidence(
                    conn,
                    &binding,
                    &record,
                    &json!({"volume_ref":updated.reference(),"receipt":{"path":record.spec["path"]}}),
                    "DirectoryCreationConfirmed",
                )?;
            }
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "directory creation lifecycle is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn custody(error: Error) -> Error {
    if error.0.code == "RecoveryRequired" {
        error
    } else {
        err(
            "RecoveryRequired",
            format!("directory creation custody: {}", error.0.message),
        )
    }
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    active(conn).map_err(custody)
}
pub(crate) fn verify(conn: &Connection) -> Result<()> {
    check(conn).map_err(custody)
}
