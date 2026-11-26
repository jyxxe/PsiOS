//! An exact review request and reply on the existing recipient's mailbox.
use super::*;
use crate::db::resolve;

mod verify;
pub(crate) use verify::verify;

// The older mailbox and generic resource projections do not know these kinds.
pub(crate) const FORMAT: u32 = crate::apps::create_recovery::FORMAT + 1;
pub(crate) const REQUEST: &str = "WorkItemReviewSignal";
pub(crate) const RECEIPT: &str = "WorkItemReviewReceipt";
pub(crate) const TOPIC: &str = "ReviewRequested";

pub(crate) fn private_kind(kind: &str) -> bool {
    matches!(kind, REQUEST | RECEIPT)
}

fn denied() -> crate::Error {
    err(
        "PermissionDenied",
        "review signal is outside authorized scope",
    )
}

fn sender(signal: &Doc) -> Result<&str> {
    signal.body["sender"].as_str().ok_or_else(invalid_state)
}

fn mailbox_id(signal: &Doc) -> Result<&str> {
    signal.body["mailbox_id"].as_str().ok_or_else(invalid_state)
}

fn receipt_id(signal: &Doc) -> String {
    format!("work-item-review-receipt-{}", signal.id)
}

fn item_scope(db: &Db<'_>, sender: &str, reviewer: &str, item: &Doc) -> Result<(Doc, Doc)> {
    let (project, current) = access_item(db, sender, &item.id, CONTRIBUTE)?;
    access(db, reviewer, &project.id, REVIEW)?;
    require(
        assigned(item)? == sender && reviewer != sender,
        "PermissionDenied",
        "review signal needs its submitting assignee and an independent reviewer",
    )?;
    Ok((project, current))
}

fn channel_scope(db: &Db<'_>, mailbox: &Doc, sender: &str) -> Result<()> {
    policy::can_target(db, sender, "mail.send", &mailbox.id)?;
    policy::can_target(db, &mailbox.owner, "mail.receive", &mailbox.id)
}

pub(crate) fn admit(db: &Db<'_>, sender: &str, mailbox: &Doc, item: &Doc) -> Result<Value> {
    channel_scope(db, mailbox, sender)?;
    let reviewer = &mailbox.owner;
    let (project, current) = item_scope(db, sender, reviewer, item)?;
    require(
        current.reference() == item.reference() && item.body["lifecycle"] == "InReview",
        "PreconditionFailed",
        "review request requires the current pending WorkItem revision",
    )?;
    let evidence = evidence::request_for_review(db, item, sender, reviewer, true)?;
    db.cfg.check_admission_space()?;
    Ok(json!({"project_ref":project.reference(),"evidence_ref":evidence.reference()}))
}

fn authorize(db: &Db<'_>, signal: &Doc, pending: bool) -> Result<Doc> {
    let mailbox = db.of(mailbox_id(signal)?, "Mailbox")?;
    let sender = sender(signal)?;
    require(
        mailbox.owner == signal.owner && mailbox.spec["sender_principal_id"] == sender,
        "RecoveryRequired",
        "review signal is not bound to its original mailbox",
    )?;
    channel_scope(db, &mailbox, sender)?;
    let item = db.resolve(&signal.body["resource_ref"])?;
    let (project, current) = item_scope(db, sender, &signal.owner, &item)?;
    require(
        project.reference() == signal.body["review_binding"]["project_ref"],
        "PermissionDenied",
        "review signal belongs to an earlier project membership revision",
    )?;
    if pending {
        require(
            mailbox.body["lifecycle"] == "Open"
                && current.reference() == item.reference()
                && item.body["lifecycle"] == "InReview",
            "PreconditionFailed",
            "review signal is no longer pending",
        )?;
    }
    let evidence = evidence::request_for_review(db, &item, sender, &signal.owner, pending)?;
    require(
        evidence.reference() == signal.body["review_binding"]["evidence_ref"],
        "RecoveryRequired",
        "review signal lost its pinned work item evidence",
    )?;
    Ok(item)
}

pub(crate) fn replay_send(db: &Db<'_>, signal: &Doc) -> Result<()> {
    authorize(db, signal, false).map(|_| ())
}

pub(crate) fn delivery(db: &Db<'_>, actor: &str, signal: &Doc) -> Result<Value> {
    let redact = || {
        json!({"reference":null,
        "signal":{"seq":signal.body["seq"],"availability":"Restricted"}})
    };
    if signal.owner != actor {
        return Ok(redact());
    }
    match authorize(db, signal, true) {
        Ok(_) => Ok(json!({"reference":signal.reference(),"signal":{
            "mailbox_id":signal.body["mailbox_id"],"sender":signal.body["sender"],
            "topic":TOPIC,"seq":signal.body["seq"],"resource_ref":signal.body["resource_ref"],
        }})),
        Err(error)
            if matches!(
                error.0.code.as_str(),
                "PermissionDenied" | "PreconditionFailed" | "ResultEvicted"
            ) =>
        {
            Ok(redact())
        }
        Err(error) => Err(error),
    }
}

fn lookup(db: &Db<'_>, actor: &str, reference: &ResourceRef, reviewer_only: bool) -> Result<Doc> {
    let signal = db
        .find(&reference.resource_id)?
        .filter(|doc| doc.kind == REQUEST)
        .ok_or_else(denied)?;
    require(
        signal.owner == actor || (!reviewer_only && sender(&signal)? == actor),
        "PermissionDenied",
        "review signal is outside authorized scope",
    )?;
    Ok(signal)
}

fn check_reference(signal: &Doc, reference: &ResourceRef) -> Result<()> {
    require(
        signal.reference() == json!(reference),
        "PreconditionFailed",
        "review signal reference changed",
    )
}

pub(super) fn receipt_view(receipt: &Doc) -> Value {
    json!({"reference":receipt.reference(),"receipt":receipt.body})
}

pub(super) fn begin_reply(
    db: &Db<'_>,
    actor: &str,
    request: &Review,
    reference: &ResourceRef,
) -> Result<(Doc, Option<Doc>)> {
    let signal = lookup(db, actor, reference, true)?;
    let existing = db.find(&receipt_id(&signal))?;
    let item = authorize(db, &signal, existing.is_none())?;
    check_reference(&signal, reference)?;
    require(
        item.id == request.id && item.version() == request.expected_resource_version,
        "PreconditionFailed",
        "review signal belongs to another work item revision",
    )?;
    if let Some(receipt) = &existing {
        verify::receipt(db.c, &signal, receipt)?;
        require(
            receipt.body["accepted"] == request.accept,
            "IdempotencyConflict",
            "review signal already has a different committed reply",
        )?;
    }
    Ok((signal, existing))
}

pub(super) fn settle(
    db: &Db<'_>,
    signal: &Doc,
    original: &Doc,
    reviewed: &Doc,
    accepted: bool,
) -> Result<Value> {
    let receipt = db.create(
        RECEIPT,
        &receipt_id(signal),
        &signal.owner,
        json!({}),
        json!({"request_signal_ref":signal.reference(),"reviewed_item_ref":original.reference(),
            "work_item_ref":reviewed.reference(),"accepted":accepted}),
    )?;
    Ok(receipt_view(&receipt))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptRequest {
    signal_ref: ResourceRef,
}

pub(crate) fn get_receipt(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ReceiptRequest = decode(params)?;
    let signal = lookup(db, actor, &request.signal_ref, false)?;
    authorize(db, &signal, false)?;
    check_reference(&signal, &request.signal_ref)?;
    let receipt = db
        .find(&receipt_id(&signal))?
        .ok_or_else(|| err("NotFound", "review signal has no committed reply"))?;
    verify::receipt(db.c, &signal, &receipt)?;
    Ok(receipt_view(&receipt))
}
