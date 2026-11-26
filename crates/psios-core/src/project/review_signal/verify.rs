//! Offline review Signal, mailbox position, and exact WorkItem outcome checks.
use super::*;
use crate::db::find;

fn before(conn: &Connection, id: &str, pivot: &str) -> Result<Doc> {
    Ok(conn.query_row(
        "SELECT p.* FROM versions p JOIN versions s ON s.id=?2 AND s.rev=1
         WHERE p.id=?1 AND p.seq<s.seq ORDER BY p.seq DESC LIMIT 1",
        params![id, pivot],
        row,
    )?)
}

fn request(conn: &Connection, signal: &Doc) -> Result<()> {
    let mailbox = find(conn, mailbox_id(signal)?)?.ok_or_else(invalid_state)?;
    let original = resolve(conn, &signal.body["resource_ref"])?;
    let project = resolve(conn, &signal.body["review_binding"]["project_ref"])?;
    let sender = sender(signal)?;
    let evidence = evidence::submitted_evidence(conn, &original, sender)?;
    let key = signal.spec["key"].as_str().ok_or_else(invalid_state)?;
    label(key)?;
    let seq = signal.body["seq"].as_u64().ok_or_else(invalid_state)?;
    let indexed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM signals WHERE mailbox=?1 AND seq=?2 AND id=?3)",
        params![mailbox.id, seq, signal.id],
        |r| r.get(0),
    )?;
    let prior_mailbox = before(conn, &mailbox.id, &signal.id)?;
    require(
        signal.rev == 1
            && signal.generation == 1
            && signal.id == crate::agent::mail::signal_id(&mailbox.id, sender, key)
            && signal.spec["mailbox_id"] == mailbox.id
            && signal.spec["topic"] == TOPIC
            && signal.body["topic"] == TOPIC
            && signal.spec["resource_ref"] == original.reference()
            && original.kind == ITEM
            && original.body["lifecycle"] == "InReview"
            && assigned(&original)? == sender
            && signal.body["review_binding"]["evidence_ref"] == evidence.reference()
            && project.kind == PROJECT
            && project.owner == original.owner
            && original.spec["project_id"] == project.id
            && before(conn, &project.id, &signal.id)?.reference() == project.reference()
            && before(conn, &original.id, &signal.id)?.reference() == original.reference()
            && policy::SYSTEM != sender
            && signal.owner != sender
            && role_allows(&project, sender, CONTRIBUTE)?
            && role_allows(&project, &signal.owner, REVIEW)?
            && mailbox.kind == "Mailbox"
            && mailbox.owner == signal.owner
            && mailbox.spec["sender_principal_id"] == sender
            && (1..=16384).contains(&seq)
            && prior_mailbox.spec == mailbox.spec
            && prior_mailbox.body["lifecycle"] == "Open"
            && prior_mailbox.body["received"]
                .as_u64()
                .and_then(|count| count.checked_add(1))
                == Some(seq)
            && mailbox.body["received"]
                .as_u64()
                .is_some_and(|count| count >= seq)
            && indexed,
        "RecoveryRequired",
        "review signal does not bind its original mailbox, project and evidence",
    )
}

pub(super) fn receipt(conn: &Connection, signal: &Doc, receipt: &Doc) -> Result<()> {
    let from = resolve(conn, &receipt.body["reviewed_item_ref"])?;
    let reviewed = resolve(conn, &receipt.body["work_item_ref"])?;
    let accepted = receipt.body["accepted"]
        .as_bool()
        .ok_or_else(invalid_state)?;
    let project = project_id(&from)?;
    require(
        receipt.kind == RECEIPT
            && receipt.rev == 1
            && receipt.generation == 1
            && receipt.id == receipt_id(signal)
            && receipt.owner == signal.owner
            && receipt.body["request_signal_ref"] == signal.reference()
            && signal.body["resource_ref"] == from.reference()
            && from.kind == ITEM
            && reviewed.kind == ITEM
            && from.id == reviewed.id
            && from.owner == reviewed.owner
            && from.spec == reviewed.spec
            && from.rev.checked_add(1) == Some(reviewed.rev)
            && before(conn, &reviewed.id, &receipt.id)?.reference() == reviewed.reference()
            && before(conn, project, &receipt.id)?.reference()
                == signal.body["review_binding"]["project_ref"]
            && from.body["lifecycle"] == "InReview"
            && reviewed.body["review_signal_ref"] == signal.reference()
            && reviewed.body["reviewed_item_ref"] == from.reference()
            && reviewed.body["reviewed_evidence_ref"]
                == signal.body["review_binding"]["evidence_ref"]
            && reviewed.body["review"]["accepted"] == accepted
            && reviewed.body["review"]["reviewer_principal_id"] == signal.owner
            && (if accepted {
                reviewed.body["lifecycle"] == "Verified"
                    && reviewed.body["evidence_ref"] == from.body["evidence_ref"]
            } else {
                reviewed.body["lifecycle"] == "Ready" && reviewed.body["evidence_ref"].is_null()
            }),
        "RecoveryRequired",
        "review reply does not attest the original conditional WorkItem decision",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut items = conn.prepare("SELECT * FROM current WHERE kind IN (?1,?2)")?;
    let mut replies = 0i64;
    for item in items.query_map([REQUEST, RECEIPT], row)? {
        let item = item?;
        if item.kind == REQUEST {
            request(conn, &item)?;
        } else {
            let signal = resolve(conn, &item.body["request_signal_ref"])?;
            require(
                signal.kind == REQUEST,
                "RecoveryRequired",
                "review reply has no request",
            )?;
            receipt(conn, &signal, &item)?;
            replies += 1;
        }
    }
    let missing = conn.query_row(
        "SELECT count(DISTINCT body->'review_signal_ref'->>'resource_id') FROM versions
         WHERE kind='WorkItem' AND body->>'review_signal_ref' IS NOT NULL",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    require(
        replies == missing,
        "RecoveryRequired",
        "one or more signal-linked work item decisions have no review reply",
    )
}
