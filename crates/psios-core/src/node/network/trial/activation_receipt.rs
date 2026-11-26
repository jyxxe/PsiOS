//! Preserve a returned activation independently of a failed journal advance.
use super::{
    Activation, Custody, JOURNAL, Journal, Phase, TOOL, journal_id, needed,
    rollback_verify::{exact_scope, position, preceding},
};
use crate::{
    Result,
    db::{Db, Doc, resolve, row},
    decode, err, require,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const KIND: &str = "NetworkTrialActivationReceipt";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    issued_journal_ref: Value,
    in_flight_operation_ref: Value,
    activation: Activation,
}

fn receipt_id(operation: &str) -> String {
    format!("network-trial-activation-receipt-{operation}")
}

pub(super) fn record(
    db: &Db<'_>,
    op: &Doc,
    custody: &Custody,
    activation: &Activation,
) -> Result<()> {
    let receipt = db.create(
        KIND,
        &receipt_id(&op.id),
        &op.owner,
        json!({"operation_id":op.id}),
        serde_json::to_value(Receipt {
            issued_journal_ref: custody.document.reference(),
            in_flight_operation_ref: op.reference(),
            activation: activation.clone(),
        })?,
    )?;
    validate(db.c, &receipt)?;
    Ok(())
}

pub(super) fn get(db: &Db<'_>, op: &Doc, custody: &Custody) -> Result<(Activation, Value)> {
    let record = db.find(&receipt_id(&op.id))?.ok_or_else(|| {
        err(
            "OutcomeUnknown",
            "no original worker receipt proves the network activation returned",
        )
    })?;
    let receipt = validate(db.c, &record)?;
    require(
        receipt.issued_journal_ref == custody.document.reference(),
        "RecoveryRequired",
        "network activation receipt belongs to a different journal revision",
    )?;
    Ok((receipt.activation, record.reference()))
}

fn validate(conn: &Connection, record: &Doc) -> Result<Receipt> {
    let receipt: Receipt = decode(&record.body)?;
    let journal_doc = resolve(conn, &receipt.issued_journal_ref)?;
    let journal: Journal = decode(&journal_doc.body)?;
    let operation = resolve(conn, &receipt.in_flight_operation_ref)?;
    let link = resolve(conn, &journal.link_ref)?;
    let initial_run = resolve(conn, &journal.run_ref)?;
    let effect = resolve(conn, &operation.body["effect_refs"][0])?;
    let issue = position(conn, &journal_doc)?;
    let stored = position(conn, record)?;
    let run = preceding(conn, &initial_run.id, stored.seq)?;
    require(
        record.kind == KIND
            && record.rev == 1
            && record.generation == 1
            && record.id == receipt_id(&operation.id)
            && record.spec == json!({"operation_id":operation.id})
            && record.owner == operation.owner
            && journal_doc.id == journal_id(&operation.id)
            && journal_doc.kind == JOURNAL
            && journal_doc.owner == record.owner
            && journal_doc.spec == json!({"operation_id":operation.id,"link_id":link.id})
            && journal.phase == Phase::ActivateIssued
            && journal.activation.is_none()
            && journal.confirmation_attempt.is_none()
            && journal.rollback_issue.is_none()
            && journal.rollback_receipt_ref.is_none()
            && issue.seq < stored.seq
            && issue.correlation == stored.correlation
            && issue.incarnation == stored.incarnation
            && journal.incarnation == stored.incarnation
            && preceding(conn, &journal_doc.id, stored.seq)?.reference() == journal_doc.reference()
            && preceding(conn, &operation.id, stored.seq)?.reference() == operation.reference()
            && preceding(conn, &link.id, stored.seq)?.reference() == link.reference()
            && operation.kind == "Operation"
            && operation.body["lifecycle"] == "Running"
            && operation.body["outcome_knowledge"] == "InFlight"
            && run.kind == "Run"
            && run.owner == operation.owner
            && run.body["task_id"] == operation.spec["task_id"]
            && run.body["claim_id"] == initial_run.body["claim_id"]
            && run.body["claim_epoch"] == initial_run.body["claim_epoch"]
            && run.body["recovery_incarnation"] == journal.incarnation
            && run.body["operation_refs"] == json!([operation.reference()])
            && effect.kind == "Effect"
            && effect.id == journal.effect_ref["resource_id"]
            && effect.owner == operation.owner
            && effect.body["action"] == TOOL
            && effect.body["phase"] == "EffectStarted"
            && effect.body["outcome_knowledge"] == "InFlight"
            && effect.body["operation_id"] == operation.id
            && effect.body["resource_ref"] == link.reference(),
        "RecoveryRequired",
        "network activation receipt does not bind the original worker and checkpoint",
    )?;
    exact_scope(&journal, &link, &receipt.activation)?;
    Ok(receipt)
}

pub(super) fn verify(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare("SELECT * FROM current WHERE kind=?1")?;
    for record in statement.query_map([KIND], row)? {
        validate(conn, &record?)?;
    }
    let mut settled = conn.prepare(
        "SELECT s.* FROM current s JOIN versions i ON i.id=s.id AND i.rev=s.rev-1
         WHERE s.kind=?1 AND s.body->>'phase'='RolledBack' AND i.body->>'phase'='ActivateIssued'",
    )?;
    for document in settled.query_map([JOURNAL], row)? {
        let document = document?;
        let op = needed(
            conn,
            document.spec["operation_id"].as_str().unwrap_or_default(),
        )?;
        let effect = resolve(conn, &op.body["effect_refs"][0])?;
        let evidence = resolve(conn, &effect.body["evidence_refs"][1])?;
        let unknown = resolve(conn, &evidence.body["operation_ref"])?;
        let receipt = needed(conn, &receipt_id(&op.id))?;
        require(
            receipt.kind == KIND
                && op.kind == "Operation"
                && op.body["outcome_knowledge"] == "Known"
                && effect.kind == "Effect"
                && effect.body["phase"] == "EffectReconciled"
                && evidence.kind == "RecoveryEvidence"
                && unknown.id == op.id
                && unknown.kind == "Operation"
                && unknown.body["outcome_knowledge"] == "Unknown"
                && evidence.body["target_ref"] == document.body["link_ref"]
                && evidence.body["evidence"]["conclusion"] == "NetworkTrialRolledBack"
                && evidence.body["evidence"]["journal_ref"] == receipt.body["issued_journal_ref"]
                && evidence.body["evidence"]["activation_receipt_ref"] == receipt.reference(),
            "RecoveryRequired",
            "reconciled network activation lost its original worker receipt",
        )?;
    }
    Ok(())
}
