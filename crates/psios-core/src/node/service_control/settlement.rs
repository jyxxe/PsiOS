//! Keep the sampled postcondition and the receipt binding independently of Artifact bytes.
use super::*;

fn existing(conn: &Connection, id: &Value) -> Result<Doc> {
    let id = id.as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd receipt has no provenance identity",
        )
    })?;
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "systemd receipt provenance is missing"))
}

fn published_state(service: &Doc, op: &Doc, after: &Doc, observation: &Value) -> bool {
    after.id == service.id
        && after.kind == "HostService"
        && after.owner == service.owner
        && after.spec == service.spec
        && after.rev == service.rev + 1
        && after.body["observation"] == *observation
        && if observation["active_state"] == "active" {
            after.body["start_operation_id"] == op.id
        } else {
            after.body.get("start_operation_id").is_none()
        }
}

pub(crate) fn record_settlement(db: &Db<'_>, op: &Doc, result: &Doc) -> Result<Option<Doc>> {
    let Some(intent) = intent(db.c, op)? else {
        return Ok(None);
    };
    let target = db.resolve(&intent.body["service_ref"])?;
    let control = Control::from_tool(&intent.body["tool"])
        .ok_or_else(|| err("RecoveryRequired", "systemd control verb is invalid"))?;
    let checkpoint = checkpoint(db.c, op, &intent, &target, control)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd settlement lacks an invocation checkpoint",
        )
    })?;
    let receipt = artifact::read(db, result, Some(32 * 1024))?;
    let observed = db.resolve(&receipt["service_ref"])?;
    require(
        receipt["checkpoint_ref"] == checkpoint.reference()
            && published_state(&target, op, &observed, &receipt["observation"])
            && confirms(
                control,
                &target,
                &checkpoint.body["before"],
                &receipt["observation"],
            ),
        "RecoveryRequired",
        "systemd result is outside its confirmed postcondition",
    )?;
    let confirmation = db.create(CONFIRMATION, &confirmation_id(&op.id), &op.owner,
        json!({"operation_id":op.id}), json!({"operation_ref":op.reference(),
            "checkpoint_ref":checkpoint.reference(),"effect_ref":op.body["effect_refs"][0],
            "result_ref":result.reference(),"service_ref":observed.reference(),
            "observation":receipt["observation"],"reconciliation_evidence_ref":result.spec["reconciliation_evidence_ref"]}))?;
    Ok(Some(confirmation))
}

pub(super) struct State<'a> {
    op: &'a Doc,
    target: &'a Doc,
    checkpoint: Option<&'a Doc>,
    task: Doc,
    run: Doc,
    effect: Doc,
    control: Control,
}

pub(super) fn verify_state<'a>(
    conn: &Connection,
    op: &'a Doc,
    intent: &Doc,
    target: &'a Doc,
    checkpoint: Option<&'a Doc>,
) -> Result<State<'a>> {
    let task = existing(conn, &op.spec["task_id"])?;
    let run = existing(conn, &task.body["current_run_id"])?;
    let original = resolve(conn, &intent.body["run_ref"])?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let control = Control::from_tool(&intent.body["tool"])
        .ok_or_else(|| err("RecoveryRequired", "systemd control verb is invalid"))?;
    let knowledge = &op.body["outcome_knowledge"];
    let outstanding = matches!(knowledge.as_str(), Some("InFlight" | "Unknown"));
    holds(conn, target, op, outstanding)?;
    let queue: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [&task.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let wanted_phase = match knowledge.as_str() {
        Some("InFlight") => "Dispatched",
        Some("Unknown") => "Unknown",
        _ => "Done",
    };
    let queued = if let Some((id, phase)) = queue {
        id == op.id && phase == wanted_phase
    } else {
        let archive = existing(conn, &json!(format!("archive-{}", task.id)))?;
        wanted_phase == "Done"
            && archive.kind == "TaskArchive"
            && archive.rev == 1
            && archive.owner == task.owner
            && archive.spec["task_id"] == task.id
            && archive.body["task_ref"] == task.reference()
            && archive.body["result"] == task.body["result"]
            && archive.body["operation_refs"] == task.body["operation_refs"]
    };
    let initial_effect = resolve(conn, &intent.body["effect_ref"])?;
    let stage = match knowledge.as_str() {
        Some("InFlight") => {
            op.body["lifecycle"] == "Running"
                && matches!(
                    task.body["lifecycle"].as_str(),
                    Some("Running" | "Cancelling")
                )
                && effect.reference()
                    == checkpoint
                        .map_or(
                            &intent.body["effect_ref"],
                            |saved| &saved.body["effect_ref"],
                        )
                        .clone()
        }
        Some("Unknown") => {
            checkpoint.is_some()
                && effect.rev == initial_effect.rev + 2
                && effect.body["phase"] == "EffectStarted"
                && matches!(
                    task.body["lifecycle"].as_str(),
                    Some("Paused" | "Cancelled")
                )
        }
        Some("NotStarted") => {
            checkpoint.is_none()
                && effect.rev == initial_effect.rev + 1
                && effect.body["phase"] == "PermitBound"
                && matches!(
                    task.body["lifecycle"].as_str(),
                    Some("Failed" | "Cancelled")
                )
        }
        Some("Known") => {
            checkpoint.is_some()
                && matches!(
                    task.body["lifecycle"].as_str(),
                    Some("Succeeded" | "Cancelled")
                )
                && matches!(
                    (effect.rev, effect.body["phase"].as_str()),
                    (3, Some("EffectConfirmed")) | (4, Some("EffectReconciled"))
                )
        }
        _ => false,
    };
    require(
        stage
            && queued
            && op.kind == "Operation"
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == intent.body["tool"]
            && task.body["operation_refs"] == json!([op.reference()])
            && task.body["outcome_knowledge"] == *knowledge
            && run.kind == "Run"
            && run.id == original.id
            && run.spec == original.spec
            && run.owner == op.owner
            && run.body["task_id"] == task.id
            && run.body["operation_refs"] == task.body["operation_refs"]
            && run.body["outcome_knowledge"] == *knowledge
            && effect.kind == "Effect"
            && effect.id == initial_effect.id
            && effect.spec == initial_effect.spec
            && effect.owner == op.owner
            && effect.body["resource_ref"] == target.reference()
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == intent.body["tool"]
            && effect.body["outcome_knowledge"] == *knowledge
            && find(conn, &effect.id)?.is_some_and(|current| current.rev == effect.rev),
        "RecoveryRequired",
        "systemd checkpoint disagrees with the recorded outcome",
    )?;
    Ok(State {
        op,
        target,
        checkpoint,
        task,
        run,
        effect,
        control,
    })
}

fn result_matches(artifact: &Doc, value: &Value) -> Result<bool> {
    Ok(artifact::is_json(artifact)
        && artifact.rev == 1
        && artifact.spec["digest"] == psios_api::sha256(&serde_json::to_vec(value)?))
}

fn check_inspection(
    conn: &Connection,
    original: &Doc,
    checkpoint: &Doc,
    target: &Doc,
    evidence: &Doc,
    observation: &Value,
) -> Result<()> {
    let proof = resolve(conn, &evidence.body["evidence"]["observation_ref"])?;
    let task = existing(conn, &proof.spec["task_id"])?;
    let op = existing(conn, &proof.spec["operation_id"])?;
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let fresh: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions sample JOIN versions lost ON sample.seq>lost.seq
       WHERE sample.kind='Run' AND sample.id=?1 AND sample.rev=1 AND lost.id=?2 AND lost.rev=?3)",
        params![proof.spec["run_id"].as_str(), original.id, original.rev],
        |r| r.get(0),
    )?;
    require(
        fresh
            && evidence.kind == "RecoveryEvidence"
            && evidence.rev == 1
            && evidence.owner == original.owner
            && evidence.body["operation_ref"] == original.reference()
            && evidence.body["target_ref"] == target.reference()
            && evidence.body["evidence"]["checkpoint_ref"] == checkpoint.reference()
            && task.kind == "Task"
            && task.spec["task_type"] == "service.inspect"
            && task.body["lifecycle"] == "Succeeded"
            && task.body["result"]["artifact_refs"] == json!([proof.reference()])
            && artifact::is_task_output(&proof, &task)
            && op.kind == "Operation"
            && op.spec["task_id"] == task.id
            && task.body["operation_refs"] == json!([op.reference()])
            && op.owner == task.owner
            && op.body["lifecycle"] == "Succeeded"
            && op.body["outcome_knowledge"] == "Known"
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == "service.inspect"
            && plan.body["allowed_effects"][0]["resource_ref"] == target.reference()
            && effect.kind == "Effect"
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == "service.inspect"
            && effect.body["phase"] == "EffectConfirmed"
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["evidence_refs"] == json!([proof.reference()])
            && proof.spec["effect_id"] == effect.id
            && result_matches(&proof, observation)?,
        "RecoveryRequired",
        "systemd confirmation lacks its exact fresh inspection",
    )
}

pub(super) fn verify_confirmation(conn: &Connection, state: &State<'_>) -> Result<bool> {
    let State {
        op,
        target,
        checkpoint,
        task,
        run,
        effect,
        control,
    } = state;
    let saved = find(conn, &confirmation_id(&op.id))?;
    if op.body["outcome_knowledge"] != "Known" {
        require(
            saved.is_none() && effect.body["evidence_refs"] == json!([]),
            "RecoveryRequired",
            "unconfirmed systemd operation has settlement evidence",
        )?;
        return Ok(false);
    }
    let confirmation = saved.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd postcondition confirmation is missing",
        )
    })?;
    let checkpoint = checkpoint
        .ok_or_else(|| err("RecoveryRequired", "systemd confirmation has no checkpoint"))?;
    let original = resolve(conn, &confirmation.body["operation_ref"])?;
    let previous_effect = resolve(conn, &confirmation.body["effect_ref"])?;
    let receipt = resolve(conn, &confirmation.body["result_ref"])?;
    let after = resolve(conn, &confirmation.body["service_ref"])?;
    let observation = &confirmation.body["observation"];
    let refs = json!([receipt.reference()]);
    let reconciled = effect.body["phase"] == "EffectReconciled";
    require(
        confirmation.kind == CONFIRMATION
            && confirmation.rev == 1
            && confirmation.owner == op.owner
            && confirmation.spec["operation_id"] == op.id
            && confirmation.body["checkpoint_ref"] == checkpoint.reference()
            && original.kind == "Operation"
            && original.id == op.id
            && original.rev + 1 == op.rev
            && original.spec == op.spec
            && original.owner == op.owner
            && original.body["plan_ref"] == op.body["plan_ref"]
            && original.body["effect_refs"] == json!([previous_effect.reference()])
            && previous_effect.id == effect.id
            && previous_effect.kind == "Effect"
            && previous_effect.rev + 1 == effect.rev
            && previous_effect.spec == effect.spec
            && task.body["result"]["artifact_refs"] == refs
            && run.body["output_refs"] == refs
            && task.body["lifecycle"] == op.body["lifecycle"]
            && artifact::is_task_output(&receipt, task)
            && receipt.spec["operation_id"] == op.id
            && published_state(target, op, &after, observation)
            && confirms(*control, target, &checkpoint.body["before"], observation)
            && result_matches(
                &receipt,
                &json!({"service_ref":after.reference(),"observation":observation,"checkpoint_ref":checkpoint.reference()}),
            )?,
        "RecoveryRequired",
        "systemd settled receipt disagrees with its checkpoint or postcondition",
    )?;
    if reconciled {
        let evidence = resolve(conn, &confirmation.body["reconciliation_evidence_ref"])?;
        let started = resolve(conn, &checkpoint.body["effect_ref"])?;
        let dispatch = resolve(conn, &checkpoint.body["operation_ref"])?;
        let mut expected = started.body;
        expected["outcome_knowledge"] = json!("Unknown");
        require(
            original.body["outcome_knowledge"] == "Unknown"
                && previous_effect.body["outcome_knowledge"] == "Unknown"
                && original.rev == dispatch.rev + 1
                && previous_effect.body == expected
                && run.body["lifecycle"] == "Lost"
                && receipt.spec["reconciliation_evidence_ref"] == evidence.reference()
                && receipt.spec["effect_ref"] == previous_effect.reference()
                && effect.body["evidence_refs"]
                    == json!([
                        receipt.reference(),
                        evidence.reference(),
                        confirmation.reference()
                    ]),
            "RecoveryRequired",
            "systemd reconciliation receipt is unbound",
        )?;
        check_inspection(conn, &original, checkpoint, target, &evidence, observation)?;
    } else {
        require(
            original.reference() == checkpoint.body["operation_ref"]
                && previous_effect.reference() == checkpoint.body["effect_ref"]
                && receipt.spec["effect_id"] == effect.id
                && run.body["lifecycle"] == task.body["lifecycle"]
                && confirmation.body["reconciliation_evidence_ref"].is_null()
                && receipt.spec.get("reconciliation_evidence_ref").is_none()
                && effect.body["evidence_refs"]
                    == json!([receipt.reference(), confirmation.reference()]),
            "RecoveryRequired",
            "systemd worker receipt is unbound",
        )?;
    }
    Ok(true)
}
