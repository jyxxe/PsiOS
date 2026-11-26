//! Verify historical rollback custody without contacting NetworkManager.
use super::{
    Enrollment, INSPECT, JOURNAL, Journal, Phase, ROLLBACK_RECEIPT, TOOL, WAITING, journal_id,
    managed, needed, rollback_receipt_id,
};
use crate::{
    Result, artifact,
    db::{Doc, resolve, row},
    decode, err, label, policy, require,
};
use psios_api::{canonical, sha256};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

struct Revision {
    seq: i64,
    doc: Doc,
    correlation: String,
    incarnation: String,
}

fn revision(record: &Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        seq: record.get("seq")?,
        doc: row(record)?,
        correlation: record.get("correlation")?,
        incarnation: record.get("incarnation")?,
    })
}

fn invalid() -> crate::Error {
    err("RecoveryRequired", "invalid network trial rollback history")
}

fn position(conn: &Connection, doc: &Doc) -> Result<Revision> {
    conn.prepare_cached("SELECT * FROM versions WHERE id=?1 AND rev=?2")?
        .query_row(params![doc.id, doc.rev], revision)
        .optional()?
        .ok_or_else(invalid)
}

fn preceding(conn: &Connection, id: &str, seq: i64) -> Result<Doc> {
    conn.prepare_cached("SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1")?
        .query_row(params![id, seq], row)
        .optional()?
        .ok_or_else(invalid)
}

struct Issued {
    journal: Journal,
    prior_op: Doc,
    task: Doc,
    run: Doc,
    effect: Doc,
    link: Doc,
}

fn parked(subject: &Doc, op: &Doc) -> bool {
    subject.owner == op.owner
        && subject.body["lifecycle"] == WAITING
        && subject.body["outcome_knowledge"] == "InFlight"
        && subject.body["operation_refs"] == json!([op.reference()])
}

fn numbered_path(path: &str, kind: &str) -> bool {
    path.strip_prefix("/org/freedesktop/NetworkManager/")
        .and_then(|path| path.split_once('/'))
        .is_some_and(|(entry_kind, suffix)| {
            entry_kind == kind
                && suffix
                    .parse::<u32>()
                    .is_ok_and(|number| number > 0 && number.to_string() == suffix)
        })
}

fn exact_scope(journal: &Journal, link: &Doc) -> Result<()> {
    let enrollment = Enrollment::read(link)?;
    let scope = journal.snapshot.as_ref().ok_or_else(invalid)?;
    let checkpoint = journal.checkpoint.as_ref().ok_or_else(invalid)?;
    let activation = journal.activation.as_ref().ok_or_else(invalid)?;
    let deadline = journal.deadline.as_ref().ok_or_else(invalid)?;
    let started = journal.started_boottime_ms.ok_or_else(invalid)?;
    let duration = i64::from(checkpoint.rollback_timeout_seconds) * 1000;
    require(
        scope.link.interface == enrollment.interface
            && scope.link.mac == enrollment.mac
            && scope.link.ifindex > 0
            && numbered_path(&scope.device_path, "Devices")
            && numbered_path(&scope.baseline.path, "ActiveConnection")
            && numbered_path(&checkpoint.path, "Checkpoint")
            && numbered_path(&activation.active.path, "ActiveConnection")
            && scope.baseline.profile_uuid == enrollment.original_uuid
            && scope.profiles[0].uuid == enrollment.original_uuid
            && scope.profiles[1].uuid == enrollment.candidate_uuid
            && checkpoint.snapshot_digest == sha256(&canonical(&serde_json::to_value(scope)?))
            && checkpoint.rollback_timeout_seconds == enrollment.confirmation_seconds
            && checkpoint.path == activation.checkpoint_path
            && activation.active.profile_uuid == enrollment.candidate_uuid
            && deadline.boot_id == scope.identity.boot_id
            && deadline.time_namespace == scope.identity.time_namespace_inode.to_string()
            && started >= 0
            && started <= checkpoint.created_boottime_ms
            && started.checked_add(duration) == Some(deadline.boot_millis)
            && checkpoint.created_boottime_ms.checked_add(duration)
                == Some(checkpoint.deadline_boottime_ms)
            && chrono::DateTime::parse_from_rfc3339(&deadline.expires_at).is_ok(),
        "RecoveryRequired",
        "network rollback scope differs from its enrolled link and checkpoint",
    )
}

fn issued(conn: &Connection, entry: &Revision) -> Result<Issued> {
    let record = &entry.doc;
    let journal: Journal = decode(&record.body)?;
    let issue = journal.rollback_issue.as_ref().ok_or_else(invalid)?;
    label(&issue.attempt)?;
    let parked_op = resolve(conn, &issue.operation_ref)?;
    let prior = preceding(conn, &record.id, entry.seq)?;
    let link = resolve(conn, &journal.link_ref)?;
    let initial_op = resolve(conn, &journal.operation_ref)?;
    let initial_task = resolve(conn, &journal.task_ref)?;
    let initial_run = resolve(conn, &journal.run_ref)?;
    let initial_effect = resolve(conn, &journal.effect_ref)?;
    let task = preceding(conn, &initial_task.id, entry.seq)?;
    let run = preceding(conn, &initial_run.id, entry.seq)?;
    let effect = resolve(conn, &parked_op.body["effect_refs"][0])?;
    let mut expected = prior.body.clone();
    expected["phase"] = json!(Phase::RollbackIssued);
    expected["rollback_issue"] = serde_json::to_value(issue)?;
    require(
        record.id == journal_id(&parked_op.id)
            && record.owner == policy::OWNER
            && record.spec == json!({"operation_id":parked_op.id,"link_id":link.id})
            && record.generation == prior.generation
            && record.spec == prior.spec
            && record.owner == prior.owner
            && prior.kind == JOURNAL
            && prior.rev.checked_add(1) == Some(record.rev)
            && prior.reference() == issue.previous_journal_ref
            && prior.body["phase"] == "AwaitingConfirmation"
            && prior.body.get("rollback_issue").is_none()
            && record.body == expected
            && journal.phase == Phase::RollbackIssued
            && journal.incarnation == entry.incarnation
            && journal.confirmation_attempt.is_none()
            && journal.rollback_receipt_ref.is_none()
            && parked_op.kind == "Operation"
            && parked_op.owner == record.owner
            && parked_op.body["lifecycle"] == "WaitingDependency"
            && parked_op.body["outcome_knowledge"] == "InFlight"
            && preceding(conn, &parked_op.id, entry.seq)?.reference() == parked_op.reference()
            && preceding(conn, &link.id, entry.seq)?.reference() == journal.link_ref
            && initial_op.id == parked_op.id
            && initial_op.kind == "Operation"
            && initial_op.owner == parked_op.owner
            && initial_op.spec == parked_op.spec
            && initial_op.body["plan_ref"] == parked_op.body["plan_ref"]
            && initial_op.body["effect_refs"] == json!([journal.effect_ref])
            && initial_task.kind == "Task"
            && initial_task.id == parked_op.spec["task_id"]
            && initial_task.owner == parked_op.owner
            && initial_task.spec["task_type"] == TOOL
            && initial_task.body["operation_refs"] == json!([journal.operation_ref])
            && initial_task.body["current_run_id"] == initial_run.id
            && initial_run.kind == "Run"
            && initial_run.owner == parked_op.owner
            && initial_run.body["task_id"] == initial_task.id
            && initial_run.body["operation_refs"] == json!([journal.operation_ref])
            && task.kind == "Task"
            && task.spec == initial_task.spec
            && task.body["current_run_id"] == run.id
            && run.kind == "Run"
            && run.body["task_id"] == task.id
            && run.body["claim_id"] == initial_run.body["claim_id"]
            && run.body["claim_epoch"] == initial_run.body["claim_epoch"]
            && run.body["recovery_incarnation"] == journal.incarnation
            && parked(&task, &parked_op)
            && parked(&run, &parked_op)
            && initial_effect.kind == "Effect"
            && initial_effect.owner == parked_op.owner
            && initial_effect.body["operation_id"] == parked_op.id
            && initial_effect.body["action"] == TOOL
            && initial_effect.body["phase"] == "PermitBound"
            && initial_effect.body["resource_ref"] == journal.link_ref
            && effect.kind == "Effect"
            && effect.id == initial_effect.id
            && effect.spec == initial_effect.spec
            && effect.owner == parked_op.owner
            && effect.body["action"] == TOOL
            && effect.body["phase"] == "EffectStarted"
            && effect.body["outcome_knowledge"] == "InFlight"
            && effect.body["operation_id"] == parked_op.id
            && effect.body["resource_ref"] == journal.link_ref
            && parked_op.body["effect_refs"] == json!([effect.reference()]),
        "RecoveryRequired",
        "network rollback issuance does not bind its original parked trial",
    )?;
    exact_scope(&journal, &link)?;
    Ok(Issued {
        journal,
        prior_op: parked_op,
        task,
        run,
        effect,
        link,
    })
}

fn transaction(entry: &Revision, other: &Revision) -> bool {
    entry.incarnation == other.incarnation && entry.correlation == other.correlation
}

fn direct(
    conn: &Connection,
    issue: &Revision,
    start: &Issued,
    settled: &Revision,
    artifact: &Revision,
    receipt_ref: &Value,
) -> Result<()> {
    let receipt = resolve(conn, receipt_ref)?;
    let pos = position(conn, &receipt)?;
    let attempt = &start
        .journal
        .rollback_issue
        .as_ref()
        .ok_or_else(invalid)?
        .attempt;
    let scope = start.journal.snapshot.as_ref().ok_or_else(invalid)?;
    let acknowledgement: managed::RollbackReceipt = decode(&receipt.body["acknowledgement"])?;
    require(
        receipt.kind == ROLLBACK_RECEIPT
            && receipt.id == rollback_receipt_id(&start.prior_op.id)
            && receipt.owner == start.prior_op.owner
            && receipt.rev == 1
            && receipt.generation == 1
            && receipt.spec == json!({"operation_id":start.prior_op.id})
            && needed(conn, &receipt.id)?.reference() == *receipt_ref
            && receipt.body
                == json!({
                    "operation_ref":start.prior_op.reference(),
                    "issued_journal_ref":issue.doc.reference(),
                    "link_ref":start.link.reference(),
                    "result_artifact_ref":artifact.doc.reference(),
                    "attempt":attempt,
                    "acknowledgement":acknowledgement,
                })
            && acknowledgement.matches(scope)
            && issue.seq < artifact.seq
            && artifact.seq < pos.seq
            && pos.seq < settled.seq
            && transaction(settled, &pos)
            && transaction(settled, artifact)
            && artifact.doc.spec["effect_id"] == start.effect.id
            && artifact
                .doc
                .spec
                .get("reconciliation_evidence_ref")
                .is_none(),
        "RecoveryRequired",
        "network rollback acknowledgement does not match its exact issued device and result",
    )
}

fn reconciled(
    conn: &Connection,
    issue: &Revision,
    start: &Issued,
    settled: &Revision,
    artifact: &Revision,
    effect: &Doc,
) -> Result<()> {
    let proof = resolve(conn, &artifact.doc.spec["reconciliation_evidence_ref"])?;
    let pos = position(conn, &proof)?;
    let unknown_op = resolve(conn, &proof.body["operation_ref"])?;
    let unknown_effect = resolve(conn, &artifact.doc.spec["effect_ref"])?;
    let observation = resolve(conn, &proof.body["evidence"]["observation_ref"])?;
    let sample_task = needed(
        conn,
        observation.spec["task_id"].as_str().ok_or_else(invalid)?,
    )?;
    let sample_op = resolve(conn, &sample_task.body["operation_refs"][0])?;
    let observation_seq = position(conn, &observation)?.seq;
    let sample_plan = resolve(conn, &sample_op.body["plan_ref"])?;
    let sample_run = needed(
        conn,
        observation.spec["run_id"].as_str().ok_or_else(invalid)?,
    )?;
    let admission = conn
        .prepare_cached("SELECT seq FROM versions WHERE id=?1 AND rev=1")?
        .query_row([&sample_run.id], |row| row.get::<_, i64>(0))
        .optional()?
        .ok_or_else(invalid)?;
    require(
        proof.kind == "RecoveryEvidence"
            && proof.owner == start.prior_op.owner
            && proof.rev == 1
            && unknown_op.kind == "Operation"
            && unknown_op.id == start.prior_op.id
            && unknown_op.owner == start.prior_op.owner
            && unknown_op.spec == start.prior_op.spec
            && unknown_op.body["outcome_knowledge"] == "Unknown"
            && unknown_op.body["effect_refs"] == json!([unknown_effect.reference()])
            && unknown_effect.kind == "Effect"
            && unknown_effect.id == start.effect.id
            && unknown_effect.owner == start.effect.owner
            && unknown_effect.spec == start.effect.spec
            && unknown_effect.body["outcome_knowledge"] == "Unknown"
            && unknown_effect.body["phase"] == "EffectStarted"
            && proof.body["target_ref"] == start.link.reference()
            && proof.body["evidence"]["journal_ref"] == issue.doc.reference()
            && proof.body["evidence"]["conclusion"] == "NetworkTrialRolledBack"
            && artifact::is_json(&observation)
            && artifact::is_task_output(&observation, &sample_task)
            && sample_task.kind == "Task"
            && sample_task.owner == proof.owner
            && sample_task.spec["task_type"] == INSPECT
            && sample_task.body["lifecycle"] == "Succeeded"
            && sample_task.body["outcome_knowledge"] == "Known"
            && sample_task.body["result"]["artifact_refs"] == json!([observation.reference()])
            && sample_run.kind == "Run"
            && sample_run.owner == sample_task.owner
            && sample_run.body["task_id"] == sample_task.id
            && sample_run.body["output_refs"] == json!([observation.reference()])
            && sample_op.kind == "Operation"
            && sample_op.owner == sample_task.owner
            && sample_op.spec["task_id"] == sample_task.id
            && observation.spec["operation_id"] == sample_op.id
            && sample_plan.kind == "OperationPlan"
            && sample_plan.body["allowed_effects"][0]["action"] == INSPECT
            && sample_plan.body["allowed_effects"][0]["resource_ref"] == unknown_op.reference()
            && position(conn, &unknown_op)?.seq < admission
            && admission < observation_seq
            && observation_seq < settled.seq
            && effect.body["evidence_refs"] == json!([artifact.doc.reference(), proof.reference()])
            && issue.seq < settled.seq
            && settled.seq < pos.seq
            && pos.seq < artifact.seq
            && transaction(settled, &pos)
            && transaction(settled, artifact),
        "RecoveryRequired",
        "uncertain network rollback settled without its independent recovery evidence",
    )
}

fn settled(
    conn: &Connection,
    issue: &Revision,
    start: &Issued,
    final_journal: &Doc,
) -> Result<bool> {
    let record = position(conn, final_journal)?;
    let journal: Journal = decode(&final_journal.body)?;
    let mut expected = issue.doc.body.clone();
    expected["phase"] = json!(Phase::RolledBack);
    if let Some(reference) = &journal.rollback_receipt_ref {
        expected["rollback_receipt_ref"] = reference.clone();
    }
    require(
        final_journal.kind == JOURNAL
            && final_journal.owner == issue.doc.owner
            && final_journal.spec == issue.doc.spec
            && final_journal.generation == issue.doc.generation
            && issue.doc.rev.checked_add(1) == Some(final_journal.rev)
            && issue.seq < record.seq
            && journal.phase == Phase::RolledBack
            && final_journal.body == expected,
        "RecoveryRequired",
        "network rollback terminal journal does not preserve its invocation",
    )?;
    let op = needed(conn, &start.prior_op.id)?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let run = needed(conn, &start.run.id)?;
    let task = needed(conn, &start.task.id)?;
    let artifact_ref = &effect.body["evidence_refs"][0];
    let output = resolve(conn, artifact_ref)?;
    let output_pos = position(conn, &output)?;
    let expected_output = serde_json::to_vec(&json!({
        "publication":"RolledBack","link_ref":start.link.reference(),
    }))?;
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE holder=?1)
            OR EXISTS(SELECT 1 FROM queue WHERE task=?2 AND (phase!='Done' OR operation IS NOT ?1))",
        params![op.id, task.id],
        |r| r.get(0),
    )?;
    let terminal_task = conn
        .prepare_cached("SELECT * FROM versions WHERE id=?1 AND seq>?2 ORDER BY seq LIMIT 1")?
        .query_row(params![task.id, record.seq], revision)
        .optional()?
        .ok_or_else(invalid)?;
    let op_pos = position(conn, &op)?;
    let run_pos = position(conn, &run)?;
    let effect_pos = position(conn, &effect)?;
    let refs = json!([output.reference()]);
    let cancellation = task.body["lifecycle"] == "Cancelled";
    let direct_receipt = journal.rollback_receipt_ref.is_some();
    require(
        !held
            && op.kind == "Operation"
            && op.owner == start.prior_op.owner
            && op.spec == start.prior_op.spec
            && op.body["plan_ref"] == start.prior_op.body["plan_ref"]
            && op.body["outcome_knowledge"] == "Known"
            && op.body["effect_refs"] == json!([effect.reference()])
            && op.body["lifecycle"] == task.body["lifecycle"]
            && effect.kind == "Effect"
            && effect.id == start.effect.id
            && effect.spec == start.effect.spec
            && effect.owner == op.owner
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == TOOL
            && effect.body["resource_ref"] == start.link.reference()
            && effect.body["outcome_knowledge"] == "Known"
            && run.kind == "Run"
            && run.owner == op.owner
            && run.body["task_id"] == task.id
            && run.body["claim_id"] == start.run.body["claim_id"]
            && run.body["claim_epoch"] == start.run.body["claim_epoch"]
            && run.body["recovery_incarnation"] == start.run.body["recovery_incarnation"]
            && run.body["outcome_knowledge"] == "Known"
            && run.body["operation_refs"] == json!([op.reference()])
            && run.body["output_refs"] == refs
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec == start.task.spec
            && task.body["current_run_id"] == run.id
            && task.body["outcome_knowledge"] == "Known"
            && task.body["operation_refs"] == json!([op.reference()])
            && task.body["result"]["artifact_refs"] == refs
            && terminal_task.doc.spec == task.spec
            && terminal_task.doc.kind == "Task"
            && terminal_task.doc.owner == task.owner
            && terminal_task.doc.body["lifecycle"] == task.body["lifecycle"]
            && terminal_task.doc.body["outcome_knowledge"] == "Known"
            && terminal_task.doc.body["current_run_id"] == run.id
            && terminal_task.doc.body["operation_refs"] == task.body["operation_refs"]
            && terminal_task.doc.body["result"]
                == json!({"availability":"Available","artifact_refs":refs})
            && artifact::is_json(&output)
            && artifact::is_task_output(&output, &task)
            && output.spec["operation_id"] == op.id
            && output.spec["digest"] == sha256(&expected_output)
            && output.spec["size_bytes"] == expected_output.len().to_string()
            && record.seq.max(output_pos.seq) < effect_pos.seq
            && effect_pos.seq < op_pos.seq
            && op_pos.seq < run_pos.seq
            && run_pos.seq < terminal_task.seq
            && transaction(&record, &effect_pos)
            && transaction(&record, &op_pos)
            && transaction(&record, &run_pos)
            && transaction(&record, &terminal_task)
            && if direct_receipt {
                cancellation
                    && start.prior_op.rev.checked_add(1) == Some(op.rev)
                    && effect.body["phase"] == "EffectConfirmed"
                    && effect.body["evidence_refs"] == refs
                    && run.body["lifecycle"] == "Cancelled"
            } else {
                (cancellation || task.body["lifecycle"] == "Failed")
                    && effect.body["phase"] == "EffectReconciled"
                    && run.body["lifecycle"] == "Lost"
            },
        "RecoveryRequired",
        "network rollback result disagrees with its historical broker settlement",
    )?;

    if let Some(reference) = journal.rollback_receipt_ref {
        direct(conn, issue, start, &record, &output_pos, &reference)?;
    } else {
        reconciled(conn, issue, start, &record, &output_pos, &effect)?;
    }
    Ok(direct_receipt)
}

fn records(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT * FROM versions WHERE kind=?1 AND body->>'phase'='RollbackIssued'")?;
    let mut acknowledged = 0i64;
    let mut finished = 0i64;
    for entry in statement.query_map([JOURNAL], revision)? {
        let entry = entry?;
        let start = issued(conn, &entry)?;
        let current = needed(conn, &entry.doc.id)?;
        if current.reference() != entry.doc.reference() {
            acknowledged += i64::from(settled(conn, &entry, &start, &current)?);
            finished += 1;
        }
    }
    let (receipts, terminals, misplaced): (i64, i64, bool) = conn.query_row(
        "SELECT (SELECT count(*) FROM versions WHERE kind=?1),
                (SELECT count(*) FROM current WHERE kind=?2 AND body->>'phase'='RolledBack'
                    AND body->'rollback_issue' IS NOT NULL),
                EXISTS(SELECT 1 FROM current WHERE kind=?2 AND (
                    (body->'rollback_issue' IS NOT NULL
                        AND body->>'phase' NOT IN ('RollbackIssued','RolledBack'))
                    OR (body->'rollback_receipt_ref' IS NOT NULL
                        AND (body->>'phase' IS NOT 'RolledBack' OR body->'rollback_issue' IS NULL))))",
        params![ROLLBACK_RECEIPT, JOURNAL],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    require(
        acknowledged == receipts && finished == terminals && !misplaced,
        "RecoveryRequired",
        "network rollback has an orphaned or missing terminal receipt",
    )?;

    // Before explicit rollback existed, the only rollback terminal came from
    // independent reconciliation. It cannot cover an effect confirmed by a reply.
    let mut legacy = conn.prepare(
        "SELECT * FROM current WHERE kind=?1 AND body->>'phase'='RolledBack'
         AND body->'rollback_issue' IS NULL",
    )?;
    for record in legacy.query_map([JOURNAL], row)? {
        let record = record?;
        let op_id = record.spec["operation_id"].as_str().ok_or_else(invalid)?;
        let op = needed(conn, op_id)?;
        let effect = resolve(conn, &op.body["effect_refs"][0])?;
        require(
            record.body.get("rollback_receipt_ref").is_none()
                && op.body["outcome_knowledge"] == "Known"
                && effect.body["phase"] == "EffectReconciled",
            "RecoveryRequired",
            "network rollback confirmed by acknowledgement is missing its issuance",
        )?;
    }
    Ok(())
}

pub(crate) fn check(conn: &Connection) -> Result<()> {
    records(conn).map_err(|error| match error.0.code.as_str() {
        "InvalidArgument" | "NotFound" | "PreconditionFailed" | "PermissionDenied" => err(
            "RecoveryRequired",
            format!("network rollback history: {}", error.0.message),
        ),
        _ => error,
    })
}
