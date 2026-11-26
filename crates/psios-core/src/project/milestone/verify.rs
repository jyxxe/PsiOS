//! Check milestone custody using versions alone, without reading WorkItem result payloads.
use super::{KIND, MAX_MILESTONES, MAX_WORK_ITEMS, creation_fields, reason_ok};
use crate::{
    Result,
    db::{Doc, find, row},
    decode, err, label,
    project::{ITEM, MANAGE, PROJECT, digest, role_allows, stable_id, text_ok, verify_review},
    require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn invalid() -> crate::Error {
    err("RecoveryRequired", "invalid milestone history")
}

fn preceding(conn: &Connection, id: &str, seq: i64) -> Result<Doc> {
    conn.prepare_cached("SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1")?
        .query_row(params![id, seq], row)
        .optional()?
        .ok_or_else(invalid)
}

fn manager(
    conn: &Connection,
    milestone: &Doc,
    actor: &str,
    seq: i64,
    project_ref: &Value,
    principal_ref: &Value,
) -> Result<()> {
    let project_id = milestone.spec["project_id"].as_str().ok_or_else(invalid)?;
    let project = preceding(conn, project_id, seq)?;
    let principal = preceding(conn, actor, seq)?;
    require(
        project.kind == PROJECT
            && project.owner == milestone.owner
            && project.reference() == *project_ref
            && role_allows(&project, actor, MANAGE)?
            && principal.kind == "Principal"
            && principal.body["lifecycle"] == "Active"
            && principal.reference() == *principal_ref,
        "RecoveryRequired",
        "milestone does not bind its active project manager and exact project revision",
    )
}

fn work_item(conn: &Connection, milestone: &Doc, id: &str, seq: i64) -> Result<Doc> {
    let item = preceding(conn, id, seq)?;
    require(
        item.kind == ITEM
            && item.owner == milestone.owner
            && item.spec["project_id"] == milestone.spec["project_id"],
        "RecoveryRequired",
        "milestone nominates a work item outside its project",
    )?;
    Ok(item)
}

fn initial(conn: &Connection, authority: &str, milestone: &Doc, seq: i64) -> Result<Vec<String>> {
    let spec = &milestone.spec;
    let project_id = spec["project_id"].as_str().ok_or_else(invalid)?;
    let actor = spec["created_by_principal_id"]
        .as_str()
        .ok_or_else(invalid)?;
    let title = spec["title"].as_str().ok_or_else(invalid)?;
    let description = spec["description"].as_str().ok_or_else(invalid)?;
    let key_digest = spec["key_digest"].as_str().ok_or_else(invalid)?;
    let nominees: Vec<String> = decode(&spec["work_item_ids"]).map_err(|_| invalid())?;
    require(
        milestone.rev == 1
            && milestone.kind == KIND
            && milestone.generation == 1
            && text_ok(title, description)
            && key_digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            && (1..=MAX_WORK_ITEMS).contains(&nominees.len())
            && nominees.iter().all(|id| label(id).is_ok())
            && nominees.windows(2).all(|pair| pair[0] < pair[1])
            && milestone.id
                == stable_id(
                    "milestone",
                    &json!([authority, project_id, actor, key_digest]),
                )
            && milestone.body == json!({"lifecycle":"Open","updated_by_principal_id":actor}),
        "RecoveryRequired",
        "invalid milestone creation identity, nominees or lifecycle",
    )?;

    let mut expected =
        creation_fields(project_id, title, description, &nominees, actor, key_digest);
    expected["creation_digest"] = json!(digest(&expected));
    expected["creation_project_ref"] = spec["creation_project_ref"].clone();
    expected["creation_principal_ref"] = spec["creation_principal_ref"].clone();
    require(
        *spec == expected,
        "RecoveryRequired",
        "milestone creation specification changed",
    )?;
    let project = find(conn, project_id)?.ok_or_else(invalid)?;
    require(
        project.kind == PROJECT && project.owner == milestone.owner,
        "RecoveryRequired",
        "milestone has no current project authority",
    )?;
    manager(
        conn,
        milestone,
        actor,
        seq,
        &spec["creation_project_ref"],
        &spec["creation_principal_ref"],
    )?;
    for id in &nominees {
        work_item(conn, milestone, id, seq)?;
    }
    Ok(nominees)
}

fn acceptance(
    conn: &Connection,
    milestone: &Doc,
    nominees: &[String],
    references: &Value,
    seq: i64,
) -> Result<()> {
    let references = references.as_array().ok_or_else(invalid)?;
    require(
        references.len() == nominees.len(),
        "RecoveryRequired",
        "milestone acceptance has an incomplete work item group",
    )?;
    for (id, reference) in nominees.iter().zip(references) {
        let item = work_item(conn, milestone, id, seq)?;
        require(
            item.reference() == *reference
                && item.body["lifecycle"] == "Closed"
                && item.body["review"]["accepted"] == true,
            "RecoveryRequired",
            "milestone acceptance does not pin the previously closed work item",
        )?;
        verify_review(conn, &item, &item.body["review"]).map_err(|error| {
            if matches!(
                error.0.code.as_str(),
                "PreconditionFailed" | "InvalidArgument"
            ) {
                invalid()
            } else {
                error
            }
        })?;
    }
    Ok(())
}

fn terminal(
    conn: &Connection,
    original: &Doc,
    milestone: &Doc,
    nominees: &[String],
    seq: i64,
) -> Result<()> {
    let state = milestone.body["lifecycle"].as_str().ok_or_else(invalid)?;
    let (field, actor_field, time_field) = match state {
        "Closed" => ("acceptance", "closed_by_principal_id", "closed_at"),
        "Abandoned" => ("abandonment", "abandoned_by_principal_id", "abandoned_at"),
        _ => return Err(invalid()),
    };
    let receipt = &milestone.body[field];
    let actor = receipt[actor_field].as_str().ok_or_else(invalid)?;
    let timestamp = receipt[time_field].as_str().ok_or_else(invalid)?;
    require(
        chrono::DateTime::parse_from_rfc3339(timestamp).is_ok()
            && receipt["previous_milestone_ref"] == original.reference(),
        "RecoveryRequired",
        "milestone terminal record is not bound to its original revision",
    )?;
    manager(
        conn,
        milestone,
        actor,
        seq,
        &receipt["project_ref"],
        &receipt["principal_ref"],
    )?;

    let mut expected = json!({
        "previous_milestone_ref":receipt["previous_milestone_ref"],
        "project_ref":receipt["project_ref"],
        "principal_ref":receipt["principal_ref"],
    });
    expected[actor_field] = json!(actor);
    expected[time_field] = json!(timestamp);
    if state == "Closed" {
        let references = &receipt["closed_work_item_refs"];
        acceptance(conn, milestone, nominees, references, seq)?;
        expected["closed_work_item_refs"] = references.clone();
    } else {
        let reason = receipt["reason"].as_str().ok_or_else(invalid)?;
        require(
            reason_ok(reason),
            "RecoveryRequired",
            "milestone abandonment requires a valid recorded reason",
        )?;
        expected["reason"] = json!(reason);
    }
    let mut body = json!({"lifecycle":state,"updated_by_principal_id":actor});
    body[field] = expected;
    require(
        body == milestone.body,
        "RecoveryRequired",
        "milestone terminal revision does not match its closure or abandonment receipt",
    )
}

pub(crate) fn check(conn: &Connection) -> Result<()> {
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    let mut ids = conn.prepare("SELECT DISTINCT id FROM versions WHERE kind=?1")?;
    let mut history =
        conn.prepare("SELECT seq, * FROM versions WHERE id=?1 ORDER BY rev LIMIT 3")?;
    let mut counts = BTreeMap::<String, i64>::new();
    for id in ids.query_map([KIND], |r| r.get::<_, String>(0))? {
        let revisions = history
            .query_map([id?], |r| Ok((r.get::<_, i64>(0)?, row(r)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        require(
            (1..=2).contains(&revisions.len()),
            "RecoveryRequired",
            "milestone has an invalid revision count",
        )?;
        let (seq, original) = &revisions[0];
        let nominees = initial(conn, &authority, original, *seq)?;
        let count = counts
            .entry(
                original.spec["project_id"]
                    .as_str()
                    .ok_or_else(invalid)?
                    .into(),
            )
            .or_default();
        *count += 1;
        require(
            *count <= MAX_MILESTONES,
            "RecoveryRequired",
            "milestone project limit exceeded",
        )?;
        if let Some((end_seq, final_doc)) = revisions.get(1) {
            require(
                end_seq > seq
                    && final_doc.rev == 2
                    && final_doc.kind == KIND
                    && final_doc.owner == original.owner
                    && final_doc.generation == original.generation
                    && final_doc.spec == original.spec
                    && final_doc.created == original.created,
                "RecoveryRequired",
                "milestone transition modified its immutable identity",
            )?;
            terminal(conn, original, final_doc, &nominees, *end_seq)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::OWNER;

    #[test]
    fn close_verifies_the_receipts_against_the_state_at_settlement() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../state.sql")).unwrap();
        conn.execute("INSERT INTO meta VALUES('authority','test-authority')", [])
            .unwrap();
        let stamp = "2025-01-01T00:00:00.000Z";
        let insert = |seq: i64, id: &str, rev: i64, kind: &str, spec: Value, body: Value| {
            conn.execute(
                "INSERT INTO versions VALUES(?1,?2,?3,1,?4,?5,?6,?7,?8,?8,?9,'test','test')",
                params![
                    seq,
                    id,
                    rev,
                    kind,
                    OWNER,
                    spec,
                    body,
                    stamp,
                    format!("event-{seq}")
                ],
            )
            .unwrap();
            conn.query_row("SELECT * FROM versions WHERE seq=?1", [seq], row)
                .unwrap()
        };
        let active = json!({"lifecycle":"Active"});
        let manager = insert(1, "manager", 1, "Principal", json!({}), active.clone());
        insert(2, "reviewer", 1, "Principal", json!({}), active);
        let original_project = insert(
            3,
            "project",
            1,
            PROJECT,
            json!({}),
            json!({"members":{"manager":"Manager","reviewer":"Reviewer"}}),
        );
        let work_spec = json!({"project_id":"project","assignee_principal_id":"contributor"});
        insert(
            4,
            "item",
            1,
            ITEM,
            work_spec.clone(),
            json!({"lifecycle":"Ready"}),
        );
        let key = digest(&json!("milestone-key"));
        let id = stable_id(
            "milestone",
            &json!(["test-authority", "project", "manager", key]),
        );
        let mut spec = creation_fields(
            "project",
            "Milestone",
            "",
            &["item".into()],
            "manager",
            &key,
        );
        spec["creation_digest"] = json!(digest(&spec));
        spec["creation_project_ref"] = original_project.reference();
        spec["creation_principal_ref"] = manager.reference();
        let open = insert(
            5,
            &id,
            1,
            KIND,
            spec.clone(),
            json!({"lifecycle":"Open","updated_by_principal_id":"manager"}),
        );
        let project = insert(
            6,
            "project",
            2,
            PROJECT,
            json!({}),
            json!({"members":{"manager":"Manager","reviewer":"Reviewer","new-viewer":"Viewer"}}),
        );
        let evidence = insert(
            7,
            "evidence",
            1,
            crate::project::evidence::KIND,
            json!({}),
            json!({"work_item_id":"item","project_id":"project","submitted_by":"contributor"}),
        );
        let in_review = insert(
            8,
            "item",
            2,
            ITEM,
            work_spec.clone(),
            json!({"lifecycle":"InReview","evidence_ref":evidence.reference()}),
        );
        let mut review = json!({
            "lifecycle":"Verified","review":{"reviewer_principal_id":"reviewer",
                "accepted":true,"reviewed_at":stamp},"reviewed_item_ref":in_review.reference(),
            "evidence_ref":evidence.reference(),"reviewed_evidence_ref":evidence.reference(),
        });
        insert(9, "item", 3, ITEM, work_spec.clone(), review.clone());
        review["lifecycle"] = json!("Closed");
        let closed = insert(10, "item", 4, ITEM, work_spec, review);
        let mut body = json!({
            "lifecycle":"Closed","updated_by_principal_id":"manager",
            "acceptance":{"previous_milestone_ref":open.reference(),"project_ref":project.reference(),
                "principal_ref":manager.reference(),"closed_by_principal_id":"manager","closed_at":stamp,
                "closed_work_item_refs":[closed.reference()]},
        });
        insert(11, &id, 2, KIND, spec, body.clone());
        insert(12, "project", 3, PROJECT, json!({}), json!({"members":{}}));
        insert(
            13,
            "manager",
            2,
            "Principal",
            json!({}),
            json!({"lifecycle":"Revoked"}),
        );
        check(&conn).unwrap();

        body["acceptance"]["project_ref"] = original_project.reference();
        conn.execute(
            "UPDATE versions SET body=?1 WHERE id=?2 AND rev=2",
            params![body, id],
        )
        .unwrap();
        let error = check(&conn).unwrap_err();
        assert_eq!(error.0.code, "RecoveryRequired");
        assert!(error.0.message.contains("active project manager"));

        body["acceptance"]["project_ref"] = project.reference();
        conn.execute(
            "UPDATE versions SET body=?1 WHERE id=?2 AND rev=2",
            params![body, id],
        )
        .unwrap();
        // The reference and digest remain exact, but the WorkItem was not Closed
        // when the milestone accepted it.
        conn.execute("UPDATE versions SET seq=14 WHERE id='item' AND rev=4", [])
            .unwrap();
        let error = check(&conn).unwrap_err();
        assert_eq!(error.0.code, "RecoveryRequired");
        assert!(error.0.message.contains("previously closed work item"));
    }
}
