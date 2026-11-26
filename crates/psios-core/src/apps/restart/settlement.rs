//! Verify original and reconciled OCI restart receipts without consulting Docker.
use super::{
    journal::{self, Custody, Journal, needed},
    *,
};
use crate::{
    artifact,
    db::{find, resolve},
};
use rusqlite::{Connection, OptionalExtension};

const CONFIRMATION: &str = "OCIRestartConfirmation";

fn confirmation_id(op: &str) -> String {
    format!("oci-restart-confirmation-{op}")
}

fn result_matches(receipt: &Doc, value: &Value) -> Result<bool> {
    Ok(artifact::is_json(receipt)
        && receipt.rev == 1
        && receipt.spec["digest"] == psios_api::sha256(&serde_json::to_vec(value)?))
}

fn published_invocation(
    conn: &Connection,
    op: &Doc,
    custody: &Custody,
    checkpoint: &Doc,
    published: &Doc,
    recovered: bool,
) -> Result<Doc> {
    let instance = resolve(conn, &published.body["instance_ref"])?;
    let after = &published.body["last_observation"];
    let (health, readiness) = crate::apps::health::projection(after, recovered);
    require(
        published.kind == "Application"
            && published.id == custody.app.id
            && published.rev == custody.app.rev + 1
            && published.owner == custody.app.owner
            && published.spec == custody.app.spec
            && published.body["last_restart_checkpoint_ref"] == checkpoint.reference()
            && published.body["lifecycle"] == "Running"
            && custody.running(after)
            && changed(&checkpoint.body["before"], after)
            && ["runtime_state", "observed_at"]
                .iter()
                .all(|field| published.body[*field] == after[*field])
            && published.body["health"] == health
            && published.body["readiness"] == readiness
            && instance.kind == "RuntimeInstance"
            && instance.id == custody.instance.id
            && instance.rev == custody.instance.rev + 1
            && instance.owner == custody.instance.owner
            && instance.spec == custody.instance.spec
            && instance.body["last_restart_operation_id"] == op.id
            && instance.body["lifecycle"] == "Running"
            && instance.body["observation"] == *after
            && instance.body["observed_at"] == after["observed_at"],
        "RecoveryRequired",
        "OCI restart result does not confirm the checkpointed invocation",
    )?;
    Ok(instance)
}

pub(crate) fn record_settlement(db: &Db<'_>, op: &Doc, receipt: &Doc) -> Result<Option<Doc>> {
    let record = journal::load(db.c, op, journal::epoch(db.c)?)?;
    if !record.modern {
        return Ok(None);
    }
    let checkpoint = record.checkpoint.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI restart settlement lacks its checkpoint",
        )
    })?;
    let reference = artifact::read(db, receipt, Some(32 * 1024))?;
    let app = db.resolve(&reference)?;
    let instance = published_invocation(
        db.c,
        op,
        &record.custody,
        &checkpoint,
        &app,
        op.body["outcome_knowledge"] == "Unknown",
    )?;
    Ok(Some(db.create(CONFIRMATION,&confirmation_id(&op.id),&op.owner,json!({"operation_id":op.id}),json!({
        "checkpoint_ref":checkpoint.reference(),"operation_ref":op.reference(),"effect_ref":record.effect.reference(),
        "result_ref":receipt.reference(),"application_ref":app.reference(),"instance_ref":instance.reference(),
        "observation":app.body["last_observation"],"reconciliation_evidence_ref":receipt.spec["reconciliation_evidence_ref"]
    }))?))
}

fn verify_queue(conn: &Connection, op: &Doc, journal: &Journal) -> Result<()> {
    let task = &journal.task;
    let knowledge = &op.body["outcome_knowledge"];
    let expected = match knowledge.as_str() {
        Some("InFlight") => "Dispatched",
        Some("Unknown") => "Unknown",
        _ => "Done",
    };
    let queue: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [&task.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let consistent = if let Some((operation, phase)) = queue {
        operation == op.id && phase == expected
    } else {
        let archive = needed(conn, &json!(format!("archive-{}", task.id)))?;
        expected == "Done"
            && archive.kind == "TaskArchive"
            && archive.rev == 1
            && archive.owner == task.owner
            && archive.spec["task_id"] == task.id
            && archive.body["task_ref"] == task.reference()
            && archive.body["result"] == task.body["result"]
            && archive.body["operation_refs"] == task.body["operation_refs"]
    };
    require(
        consistent,
        "RecoveryRequired",
        "OCI restart queue disagrees with its retained operation",
    )?;
    journal.custody.holds(conn, op, expected != "Done")
}

fn check_inspection(
    conn: &Connection,
    original: &Doc,
    evidence: &Doc,
    checkpoint: &Doc,
    custody: &Custody,
    sample: &Value,
) -> Result<()> {
    let proof = resolve(conn, &evidence.body["evidence"]["observation_ref"])?;
    let task = needed(conn, &proof.spec["task_id"])?;
    let operation = needed(conn, &proof.spec["operation_id"])?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let effect = resolve(conn, &operation.body["effect_refs"][0])?;
    let fresh: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions sample JOIN versions lost ON sample.seq>lost.seq
             WHERE sample.kind='Run' AND sample.id=?1 AND sample.rev=1 AND lost.id=?2 AND lost.rev=?3)",
        params![proof.spec["run_id"].as_str(),original.id,original.rev],|r| r.get(0),
    )?;
    let mut audience = vec![custody.app.reference()];
    if let Some(volume) = &custody.volume {
        audience.push(volume.reference());
    }
    let observed_targets: Vec<_> = plan.body["preconditions"]
        .as_array()
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "OCI restart inspection audience is missing",
            )
        })?
        .iter()
        .map(|condition| condition["resource_ref"].clone())
        .collect();
    require(
        fresh
            && evidence.kind == "RecoveryEvidence"
            && evidence.rev == 1
            && evidence.owner == original.owner
            && evidence.body["operation_ref"] == original.reference()
            && evidence.body["target_ref"] == custody.app.reference()
            && evidence.body["evidence"]["checkpoint_ref"] == checkpoint.reference()
            && evidence.body["evidence"]["conclusion"] == "ObservedContainerRestart"
            && task.kind == "Task"
            && task.spec["task_type"] == INSPECT
            && task.body["lifecycle"] == "Succeeded"
            && task.body["result"]["artifact_refs"] == json!([proof.reference()])
            && artifact::is_task_output(&proof, &task)
            && operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Succeeded"
            && task.body["operation_refs"] == json!([operation.reference()])
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == INSPECT
            && plan.body["allowed_effects"][0]["resource_ref"] == custody.app.reference()
            && observed_targets == audience
            && effect.kind == "Effect"
            && effect.body["operation_id"] == operation.id
            && effect.body["resource_ref"] == custody.app.reference()
            && effect.body["action"] == INSPECT
            && effect.body["phase"] == "EffectConfirmed"
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["evidence_refs"] == json!([proof.reference()])
            && proof.spec["effect_id"] == effect.id
            && result_matches(&proof, sample)?,
        "RecoveryRequired",
        "OCI restart reconciliation does not bind its fresh exact inspection",
    )
}

fn check_completed(conn: &Connection, op: &Doc, journal: &Journal) -> Result<bool> {
    let task = &journal.task;
    let effect = &journal.effect;
    let confirmation = find(conn, &confirmation_id(&op.id))?;
    let has_result = task.body["result"]["artifact_refs"]
        .as_array()
        .is_some_and(|refs| !refs.is_empty());
    let successful = op.body["outcome_knowledge"] == "Known" && has_result;
    if !successful {
        require(
            confirmation.is_none()
                && !has_result
                && effect.body["evidence_refs"] == json!([])
                && op.body["lifecycle"] != "Succeeded"
                && (!journal.modern || op.body["outcome_knowledge"] != "Known"),
            "RecoveryRequired",
            "unsettled OCI restart has unexpected result evidence",
        )?;
        return Ok(false);
    }
    require(
        matches!(
            task.body["lifecycle"].as_str(),
            Some("Succeeded" | "Cancelled")
        ) && op.body["lifecycle"] == task.body["lifecycle"],
        "RecoveryRequired",
        "OCI restart confirmation does not belong to a known successful result",
    )?;
    let checkpoint = journal.checkpoint.as_ref().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI restart result lacks its checkpoint",
        )
    })?;
    let receipt = resolve(conn, &task.body["result"]["artifact_refs"][0])?;
    require(
        artifact::is_task_output(&receipt, task)
            && receipt.spec["operation_id"] == op.id
            && task.body["result"]["artifact_refs"] == json!([receipt.reference()])
            && journal.run.body["output_refs"] == task.body["result"]["artifact_refs"],
        "RecoveryRequired",
        "OCI restart result is outside its original task and run",
    )?;
    let application = if journal.modern {
        let proof = confirmation
            .as_ref()
            .ok_or_else(|| err("RecoveryRequired", "OCI restart confirmation is missing"))?;
        resolve(conn, &proof.body["application_ref"])?
    } else {
        require(
            confirmation.is_none(),
            "RecoveryRequired",
            "legacy OCI restart has a newer confirmation profile",
        )?;
        // The historical direct result contains only the exact newly published Application reference.
        let bytes = artifact::checkpoint_bytes(conn, &receipt, 32 * 1024)?;
        resolve(conn, &psios_api::parse(&bytes)?)?
    };
    let instance = published_invocation(
        conn,
        op,
        &journal.custody,
        checkpoint,
        &application,
        effect.body["phase"] == "EffectReconciled",
    )?;
    require(
        result_matches(&receipt, &application.reference())?,
        "RecoveryRequired",
        "OCI restart result application was replaced",
    )?;
    let mut evidence_refs = vec![receipt.reference()];
    let reconciled = match effect.body["phase"].as_str() {
        Some("EffectConfirmed") => false,
        Some("EffectReconciled") => true,
        _ => {
            return Err(err(
                "RecoveryRequired",
                "successful OCI restart has no final effect",
            ));
        }
    };
    let mut original = None;
    if reconciled {
        let evidence = resolve(conn, &receipt.spec["reconciliation_evidence_ref"])?;
        let lost = resolve(conn, &evidence.body["operation_ref"])?;
        let lost_effect = resolve(conn, &receipt.spec["effect_ref"])?;
        require(
            lost.kind == "Operation"
                && lost.id == op.id
                && lost.rev + 1 == op.rev
                && lost.spec == op.spec
                && lost.owner == op.owner
                && lost.body["plan_ref"] == op.body["plan_ref"]
                && lost.body["outcome_knowledge"] == "Unknown"
                && lost.body["effect_refs"] == json!([lost_effect.reference()])
                && lost_effect.kind == "Effect"
                && lost_effect.id == effect.id
                && lost_effect.rev + 1 == effect.rev
                && lost_effect.spec == effect.spec
                && lost_effect.body["outcome_knowledge"] == "Unknown"
                && lost_effect.body["phase"] == "EffectStarted"
                && journal.run.body["lifecycle"] == "Lost"
                && receipt.spec.get("effect_id").is_none(),
            "RecoveryRequired",
            "OCI restart reconciliation does not close the original unknown effect",
        )?;
        check_inspection(
            conn,
            &lost,
            &evidence,
            checkpoint,
            &journal.custody,
            &application.body["last_observation"],
        )?;
        evidence_refs.push(evidence.reference());
        original = Some((lost.reference(), lost_effect.reference()));
    } else {
        require(
            receipt.spec["effect_id"] == effect.id
                && receipt.spec.get("effect_ref").is_none()
                && receipt.spec.get("reconciliation_evidence_ref").is_none()
                && journal.run.body["lifecycle"] == task.body["lifecycle"],
            "RecoveryRequired",
            "OCI restart result has no ordinary effect provenance",
        )?;
    }
    if let Some(proof) = confirmation {
        let (expected_op, expected_effect) = original.unwrap_or_else(|| {
            (
                checkpoint.body["started_operation_ref"].clone(),
                checkpoint.body["started_effect_ref"].clone(),
            )
        });
        require(
            proof.kind == CONFIRMATION
                && proof.rev == 1
                && proof.owner == op.owner
                && proof.spec["operation_id"] == op.id
                && proof.body
                    == json!({"checkpoint_ref":checkpoint.reference(),"operation_ref":expected_op,"effect_ref":expected_effect,
                "result_ref":receipt.reference(),"application_ref":application.reference(),"instance_ref":instance.reference(),
                "observation":application.body["last_observation"],"reconciliation_evidence_ref":receipt.spec["reconciliation_evidence_ref"]}),
            "RecoveryRequired",
            "OCI restart confirmation disagrees with the original effect or receipt",
        )?;
        evidence_refs.push(proof.reference());
    }
    require(
        effect.body["evidence_refs"] == json!(evidence_refs),
        "RecoveryRequired",
        "OCI restart effect has missing or swapped evidence",
    )?;
    Ok(journal.modern)
}

pub(crate) fn verify(conn: &Connection, format: u32) -> Result<()> {
    let epoch = if format >= CUSTODY_FORMAT {
        journal::epoch(conn)?
    } else {
        i64::MAX
    };
    let mut statement = conn.prepare("SELECT body->>'operation_id' FROM versions WHERE kind='Effect' AND rev=1 AND body->>'action'=?1")?;
    let mut counts = (0i64, 0i64, 0i64);
    for id in statement.query_map([TOOL], |r| r.get::<_, String>(0))? {
        let op = needed(conn, &json!(id?))?;
        let record = journal::load(conn, &op, epoch)?;
        verify_queue(conn, &op, &record)?;
        counts.0 += 1;
        counts.1 += i64::from(record.checkpoint.is_some());
        counts.2 += i64::from(check_completed(conn, &op, &record)?);
    }
    let actual: (i64, i64, i64) = conn.query_row(
        "SELECT count(*) FILTER (WHERE kind=?1), count(*) FILTER (WHERE kind=?2),
        count(*) FILTER (WHERE kind=?3) FROM current WHERE kind IN (?1,?2,?3)",
        [INTENT, CHECKPOINT, CONFIRMATION],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    require(
        actual == counts,
        "RecoveryRequired",
        "OCI restart has orphaned custody records",
    )
}
