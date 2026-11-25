//! Verify historical and unresolved restore custody without consulting Docker.
use super::*;
use rusqlite::OptionalExtension;
use std::collections::HashMap;

fn existing(conn: &Connection, identity: &Value) -> Result<Doc> {
    let id = identity.as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI restore proof is missing a resource identity",
        )
    })?;
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "OCI restore proof resource is missing"))
}

fn approved_effect(conn: &Connection, op: &Doc, task: &Doc, binding: &Binding) -> Result<()> {
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let permit_binding = resolve(conn, &permit.body["auth_proof"]["record_ref"])?;
    let decision = resolve(conn, &permit.body["decision_ref"])?;
    let approval_use = existing(conn, &json!(format!("use-{}", decision.id)))?;
    let audience = json!(binding.audience());
    require(
        plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && plan.body["requester_principal_id"] == op.owner
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == TOOL
            && effect.body["resource_ref"] == json!(binding.application_ref)
            && permit.kind == "ExecutionPermit"
            && permit.owner == op.owner
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["audience_resource_refs"] == audience
            && permit_binding.kind == "PermitBinding"
            && permit_binding.owner == op.owner
            && permit_binding.body["permit_id"] == permit.id
            && permit_binding.body["audience_resource_refs"] == audience
            && decision.kind == "ApprovalDecision"
            && decision.owner == op.owner
            && decision.body["decision"] == "Approved"
            && decision.body["use_limit"] == "SingleOperation"
            && decision.body["approver_principal_id"] == policy::OWNER
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && decision.body["preconditions"] == plan.body["preconditions"]
            && approval_use.kind == "ApprovalUse"
            && approval_use.owner == op.owner
            && approval_use.body["decision_ref"] == decision.reference()
            && approval_use.body["operation_id"] == op.id
            && approval_use.body["plan_digest"] == plan.body["plan_digest"]
            && approval_use.body["binding_status"] == "Bound",
        "RecoveryRequired",
        "OCI restore effect disagrees with its bound approval or permit",
    )
}

fn confirmed_checkpoint(
    conn: &Connection,
    op: &Doc,
    task: &Doc,
    recorded: &Recorded,
) -> Result<()> {
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let run = existing(conn, &task.body["current_run_id"])?;
    let artifact = resolve(conn, &task.body["result"]["artifact_refs"][0])?;
    let phase = effect.body["phase"].as_str();
    let reconciled = phase == Some("EffectReconciled");
    let result_refs = json!([artifact.reference()]);
    require(
        matches!(phase, Some("EffectConfirmed" | "EffectReconciled"))
            && matches!(
                op.body["lifecycle"].as_str(),
                Some("Succeeded" | "Cancelled")
            )
            && op.body["outcome_knowledge"] == "Known"
            && task.body["lifecycle"] == op.body["lifecycle"]
            && task.body["outcome_knowledge"] == "Known"
            && op.body["effect_refs"] == json!([effect.reference()])
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && effect.body["action"] == TOOL
            && effect.body["operation_id"] == op.id
            && effect.body["resource_ref"] == json!(recorded.binding.application_ref)
            && effect.body["outcome_knowledge"] == "Known"
            && permit.kind == "ExecutionPermit"
            && permit.owner == op.owner
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && run.kind == "Run"
            && run.owner == op.owner
            && run.body["task_id"] == task.id
            && run.body["operation_refs"] == task.body["operation_refs"]
            && run.body["outcome_knowledge"] == "Known"
            && (reconciled && run.body["lifecycle"] == "Lost"
                || !reconciled && run.body["lifecycle"] == op.body["lifecycle"])
            && run.body["output_refs"] == result_refs
            && task.body["result"]["availability"] == "Available"
            && task.body["result"]["artifact_refs"] == result_refs
            && artifact::is_json(&artifact)
            && artifact::is_task_output(&artifact, task)
            && artifact.rev == 1
            && artifact.spec["operation_id"] == op.id,
        "RecoveryRequired",
        "OCI restore settled effect or result provenance is inconsistent",
    )?;
    let bytes = artifact::checkpoint_bytes(conn, &artifact, 32 * 1024)?;
    let result = psios_api::parse(&bytes)?;
    let observation = &result["observation"];
    require(
        cache_present(&recorded.binding, observation)
            && result == receipt(op, recorded, observation),
        "RecoveryRequired",
        "OCI restore result disagrees with its checkpoint or observed image",
    )?;
    if reconciled {
        check_reconciliation(conn, op, recorded, &effect, &artifact, observation)?;
    } else {
        require(
            effect.body["evidence_refs"] == result_refs
                && artifact.spec["effect_id"] == effect.id
                && artifact.spec.get("effect_ref").is_none()
                && artifact.spec.get("reconciliation_evidence_ref").is_none()
                && observation.get("checkpoint_ref").is_none()
                && observation.get("original_operation_ref").is_none(),
            "RecoveryRequired",
            "OCI restore result is not linked to its confirmed effect",
        )?;
    }
    Ok(())
}

fn check_reconciliation(
    conn: &Connection,
    op: &Doc,
    recorded: &Recorded,
    effect: &Doc,
    result: &Doc,
    observation: &Value,
) -> Result<()> {
    let evidence = resolve(conn, &result.spec["reconciliation_evidence_ref"])?;
    let original = resolve(conn, &evidence.body["operation_ref"])?;
    let unknown = resolve(conn, &result.spec["effect_ref"])?;
    let witness = resolve(conn, &evidence.body["evidence"]["observation_ref"])?;
    let task = existing(conn, &witness.spec["task_id"])?;
    let inspect_op = existing(conn, &witness.spec["operation_id"])?;
    let inspect_plan = resolve(conn, &inspect_op.body["plan_ref"])?;
    let inspect_effect = resolve(conn, &inspect_op.body["effect_refs"][0])?;
    let fresh: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions sample JOIN versions lost ON sample.seq>lost.seq
            WHERE sample.kind='Run' AND sample.id=?1 AND sample.rev=1 AND lost.id=?2 AND lost.rev=?3)",
        params![witness.spec["run_id"].as_str(), original.id, original.rev],
        |row| row.get(0),
    )?;
    require(
        effect.body["evidence_refs"] == json!([result.reference(), evidence.reference()])
            && result.spec.get("effect_id").is_none()
            && evidence.kind == "RecoveryEvidence"
            && evidence.rev == 1
            && evidence.owner == op.owner
            && evidence.body["target_ref"] == json!(recorded.binding.application_ref)
            && evidence.body["evidence"]["checkpoint_ref"] == recorded.checkpoint.reference()
            && evidence.body["evidence"]["conclusion"] == "ObservedExactOciImageCache"
            && original.kind == "Operation"
            && original.id == op.id
            && original.owner == op.owner
            && original.spec["task_id"] == op.spec["task_id"]
            && original.body["plan_ref"] == op.body["plan_ref"]
            && original.body["outcome_knowledge"] == "Unknown"
            && original.body["effect_refs"] == json!([unknown.reference()])
            && unknown.kind == "Effect"
            && unknown.id == effect.id
            && unknown.owner == op.owner
            && unknown.spec == effect.spec
            && unknown.body["operation_id"] == op.id
            && unknown.body["outcome_knowledge"] == "Unknown"
            && observation["checkpoint_ref"] == recorded.checkpoint.reference()
            && observation["original_operation_ref"] == original.reference()
            && fresh
            && artifact::is_json(&witness)
            && artifact::is_task_output(&witness, &task)
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == INSPECT
            && task.body["lifecycle"] == "Succeeded"
            && task.body["result"]["artifact_refs"] == json!([witness.reference()])
            && task.body["operation_refs"] == json!([inspect_op.reference()])
            && inspect_op.kind == "Operation"
            && inspect_op.owner == op.owner
            && inspect_op.spec["task_id"] == task.id
            && inspect_op.body["lifecycle"] == "Succeeded"
            && inspect_op.body["outcome_knowledge"] == "Known"
            && inspect_plan.kind == "OperationPlan"
            && inspect_plan.body["allowed_effects"][0]["action"] == INSPECT
            && inspect_plan.body["allowed_effects"][0]["resource_ref"] == original.reference()
            && inspect_effect.kind == "Effect"
            && inspect_effect.body["operation_id"] == inspect_op.id
            && inspect_effect.body["action"] == INSPECT
            && inspect_effect.body["phase"] == "EffectConfirmed"
            && inspect_effect.body["outcome_knowledge"] == "Known"
            && inspect_effect.body["evidence_refs"] == json!([witness.reference()])
            && witness.spec["effect_id"] == inspect_effect.id,
        "RecoveryRequired",
        "OCI restore reconciliation does not bind a fresh inspection to the original effect",
    )?;
    let bytes = artifact::checkpoint_bytes(conn, &witness, 32 * 1024)?;
    require(
        psios_api::parse(&bytes)? == *observation,
        "RecoveryRequired",
        "OCI restore receipt differs from the retained reconciliation inspection",
    )
}

fn queue_phase(conn: &Connection, task: &Doc, op: &Doc) -> Result<String> {
    let queue: Option<(String, String)> = conn
        .query_row(
            "SELECT phase,operation FROM queue WHERE task=?1",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((phase, operation)) = queue {
        require(
            operation == op.id && matches!(phase.as_str(), "Dispatched" | "Unknown" | "Done"),
            "RecoveryRequired",
            "OCI restore intent disagrees with its durable queue entry",
        )?;
        return Ok(phase);
    }

    let archive = existing(conn, &json!(format!("archive-{}", task.id)))?;
    require(
        archive.kind == "TaskArchive"
            && archive.rev == 1
            && archive.owner == task.owner
            && archive.spec["task_id"] == task.id
            && archive.body["task_ref"] == task.reference()
            && archive.body["result"] == task.body["result"]
            && archive.body["operation_refs"] == task.body["operation_refs"]
            && matches!(
                task.body["lifecycle"].as_str(),
                Some("Succeeded" | "Failed" | "Cancelled")
            )
            && task.body["outcome_knowledge"] == "Known",
        "RecoveryRequired",
        "OCI restore is missing its bound archived Task",
    )?;
    Ok("Done".into())
}

fn normalized_digest<'a>(
    conn: &Connection,
    archive: &Doc,
    binding: &Binding,
    cache: &'a mut HashMap<String, Option<String>>,
) -> Result<Option<&'a str>> {
    use std::collections::hash_map::Entry;
    let computed = match cache.entry(archive.id.clone()) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            let digest = match artifact::open_verified_raw(
                conn,
                archive,
                artifact::ingress::MAX_BYTES as usize,
            ) {
                Err(failure) if failure.0.code == "ResultEvicted" => None,
                reader => Some(restore_archive::write_load(
                    reader?,
                    std::io::sink(),
                    &binding.image_id,
                    &binding.diff_ids,
                )?),
            };
            entry.insert(digest)
        }
    };
    Ok(computed.as_deref())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut intents = conn.prepare("SELECT * FROM current WHERE kind=?1")?;
    let mut locked = 0i64;
    let mut checkpoints = 0i64;
    let mut normalized = HashMap::new();
    for document in intents.query_map([INTENT], row)? {
        let document = document?;
        let op = existing(conn, &document.spec["operation_id"])?;
        let task = existing(conn, &op.spec["task_id"])?;
        let binding = intent(conn, &op, &document)?;
        let proof = resolve(conn, &json!(binding.image_capture_ref))?;
        let archive = resolve(conn, &json!(binding.image_archive_ref))?;
        let verified = capture::require_archive(conn, &archive, &binding.image_id)?;
        require(
            verified.reference() == proof.reference()
                && proof.body["image"]["diff_ids"] == json!(binding.diff_ids)
                && task.kind == "Task"
                && task.owner == op.owner
                && task.spec["task_type"] == TOOL
                && task.body["operation_refs"] == json!([op.reference()]),
            "RecoveryRequired",
            "OCI restore intent disagrees with captured bytes or original task identity",
        )?;
        approved_effect(conn, &op, &task, &binding)?;
        let phase = queue_phase(conn, &task, &op)?;
        let active = matches!(phase.as_str(), "Dispatched" | "Unknown");
        let checkpoint = find(conn, &checkpoint_id(&op.id))?;
        let archive = release_archive::Archive {
            artifact: archive,
            proof,
        };
        if active {
            held(conn, &op, &binding)?;
            release_archive::check_pin(
                conn,
                "Release",
                &binding.release_ref.resource_id,
                Some(&archive),
            )?;
            locked += 1;
        }
        if let Some(checkpoint) = checkpoint {
            let recorded = recorded(conn, &op, &document, checkpoint)?;
            let digest = normalized_digest(conn, &archive.artifact, &binding, &mut normalized)?;
            require(
                digest.is_some_and(|got| {
                    recorded.checkpoint.body["normalized_archive_digest"] == got
                }) || !active && digest.is_none(),
                "RecoveryRequired",
                "OCI restore checkpoint differs from the normalized retained archive",
            )?;
            if !active {
                confirmed_checkpoint(conn, &op, &task, &recorded)?;
            }
            checkpoints += 1;
        } else {
            require(
                phase != "Unknown"
                    && op.body["lifecycle"] != "Succeeded"
                    && task.body["result"]["availability"] != "Available",
                "RecoveryRequired",
                "OCI restore effect is missing its durable load checkpoint",
            )?;
        }
    }
    require(
        conn.query_row("SELECT count(*) FROM oci_image_restore_holds", [], |r| r.get::<_, i64>(0))? == locked
            && conn.query_row("SELECT count(*) FROM current WHERE kind=?1", [CHECKPOINT], |r| r.get::<_, i64>(0))? == checkpoints
            && !conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM queue q JOIN current t ON t.id=q.task
                    WHERE t.spec->>'task_type'=?1 AND q.phase IN ('Dispatched','Unknown')
                    AND NOT EXISTS(SELECT 1 FROM current i WHERE i.id='oci-image-restore-intent-'||q.operation))
                  OR EXISTS(SELECT 1 FROM current t LEFT JOIN current o ON o.id=t.body->'operation_refs'->0->>'resource_id'
                    WHERE t.kind='Task' AND t.spec->>'task_type'=?1 AND t.body->'result'->>'availability'='Available'
                    AND NOT EXISTS(SELECT 1 FROM current i WHERE i.id='oci-image-restore-intent-'||o.id))",
                [TOOL], |r| r.get::<_, bool>(0),
            )?,
        "RecoveryRequired",
        "OCI restore holds or checkpoints do not match their operations",
    )
}
