//! Revise an unpinned assignment or end a work item while preserving its prior facts.
use super::*;
use rusqlite::OptionalExtension;
use serde::Serialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reassign {
    id: String,
    expected_resource_version: String,
    assignee_principal_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Abandon {
    id: String,
    expected_resource_version: String,
    reason: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    previous_work_item_ref: Value,
    project_ref: Value,
    previous_assignee_principal_id: String,
    assigned_by_principal_id: String,
    assigned_at: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Abandonment {
    previous_work_item_ref: Value,
    project_ref: Value,
    reason: String,
    prior_lifecycle: String,
    abandoned_by_principal_id: String,
    abandoned_at: String,
}

fn reason_ok(reason: &str) -> bool {
    !reason.trim().is_empty() && reason.len() <= 512 && !reason.chars().any(char::is_control)
}

pub(super) fn awaiting_assignment(item: &Doc) -> bool {
    item.body["lifecycle"] == "Ready" && item.body.get("evidence_ref").is_none()
}

fn may_abandon(item: &Doc) -> bool {
    match item.body["lifecycle"].as_str() {
        Some("Ready") => item.body.get("evidence_ref").is_none(),
        Some("InReview" | "Verified") => item.body["evidence_ref"].is_object(),
        _ => false,
    }
}

fn assignment_body(item: &Doc, change: &Assignment) -> Result<Value> {
    let mut body = item.body.clone();
    let state = body.as_object_mut().ok_or_else(invalid_state)?;
    for field in ["review", "reviewed_item_ref", "reviewed_evidence_ref"] {
        state.remove(field);
    }
    body["assignment"] = serde_json::to_value(change)?;
    body["updated_by_principal_id"] = json!(change.assigned_by_principal_id);
    Ok(body)
}

fn abandonment_body(item: &Doc, change: &Abandonment) -> Result<Value> {
    let mut body = item.body.clone();
    body["lifecycle"] = json!("Abandoned");
    body["abandonment"] = serde_json::to_value(change)?;
    body["updated_by_principal_id"] = json!(change.abandoned_by_principal_id);
    Ok(body)
}

pub(crate) fn reassign(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Reassign = decode(params)?;
    let (project, item) = access_item(db, actor, &request.id, MANAGE)?;
    item.cas(&request.expected_resource_version)?;
    require(
        awaiting_assignment(&item),
        "PreconditionFailed",
        "reassignment requires a ready work item without retained evidence",
    )?;
    let replacement = request.assignee_principal_id.as_str();
    require(
        role_allows(&project, replacement, CONTRIBUTE)?,
        "PermissionDenied",
        "new assignee is outside project contributor scope",
    )?;
    policy::active(db, replacement)?;
    policy::can_target(db, replacement, READ, &project.id)?;
    policy::can_target(db, replacement, CONTRIBUTE, &project.id)?;
    check_criterion_assignee(db.c, &item, replacement)?;
    let previous = assigned(&item)?;
    if previous == replacement {
        return item_view(db, &item, actor, false);
    }
    db.cfg.check_admission_space()?;
    let mut spec = item.spec.clone();
    spec["assignee_principal_id"] = json!(replacement);
    let change = Assignment {
        previous_work_item_ref: item.reference(),
        project_ref: project.reference(),
        previous_assignee_principal_id: previous.to_owned(),
        assigned_by_principal_id: actor.to_owned(),
        assigned_at: now(),
    };
    let updated = db.update(&item, spec, assignment_body(&item, &change)?)?;
    item_view(db, &updated, actor, false)
}

pub(crate) fn abandon(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Abandon = decode(params)?;
    let (project, item) = access_item(db, actor, &request.id, MANAGE)?;
    item.cas(&request.expected_resource_version)?;
    require(
        reason_ok(&request.reason),
        "InvalidArgument",
        "abandonment requires a reason up to 512 bytes",
    )?;
    require(
        may_abandon(&item),
        "PreconditionFailed",
        "work item cannot be abandoned in this state",
    )?;
    if item.body.get("evidence_ref").is_some() {
        evidence::abandon(db, &item)?;
    }
    let change = Abandonment {
        previous_work_item_ref: item.reference(),
        project_ref: project.reference(),
        reason: request.reason,
        prior_lifecycle: item.body["lifecycle"]
            .as_str()
            .ok_or_else(invalid_state)?
            .to_owned(),
        abandoned_by_principal_id: actor.to_owned(),
        abandoned_at: now(),
    };
    let updated = db.status(&item, abandonment_body(&item, &change)?)?;
    item_view(db, &updated, actor, false)
}

pub(super) fn assignment_view(item: &Doc) -> Value {
    let change = &item.body["assignment"];
    if change.is_null() {
        return Value::Null;
    }
    json!({"previous_assignee_principal_id":change["previous_assignee_principal_id"],
        "assigned_by_principal_id":change["assigned_by_principal_id"],"assigned_at":change["assigned_at"]})
}

pub(super) fn abandonment_view(item: &Doc) -> Value {
    let change = &item.body["abandonment"];
    if change.is_null() {
        return Value::Null;
    }
    json!({"reason":change["reason"],"prior_lifecycle":change["prior_lifecycle"],
        "abandoned_by_principal_id":change["abandoned_by_principal_id"],"abandoned_at":change["abandoned_at"]})
}

pub(super) fn sequence(conn: &Connection, doc: &Doc) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![doc.id, doc.rev],
        |row| row.get(0),
    )?)
}

pub(super) fn preceding(conn: &Connection, id: &str, kind: &str, position: i64) -> Result<Doc> {
    conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND kind=?2 AND seq<?3 ORDER BY seq DESC LIMIT 1",
        params![id, kind, position],
        row,
    )
    .optional()?
    .ok_or_else(invalid_state)
}

pub(super) fn manager_project(
    conn: &Connection,
    item: &Doc,
    reference: &Value,
    actor: &str,
    position: i64,
) -> Result<Doc> {
    let project = crate::db::resolve(conn, reference)?;
    require(
        project.kind == PROJECT
            && project.id == project_id(item)?
            && project.owner == item.owner
            && project.reference() == *reference
            && preceding(conn, &project.id, PROJECT, position)?.reference() == *reference
            && role_allows(&project, actor, MANAGE)?
            && preceding(conn, actor, "Principal", position)?.body["lifecycle"] == "Active",
        "RecoveryRequired",
        "work item maintenance does not bind its project manager",
    )?;
    Ok(project)
}

fn verify_assignment(conn: &Connection, previous: &Doc, next: &Doc) -> Result<()> {
    let change: Assignment = decode(&next.body["assignment"]).map_err(|_| invalid_state())?;
    let assignee = assigned(next)?;
    let position = sequence(conn, next)?;
    let project = manager_project(
        conn,
        next,
        &change.project_ref,
        &change.assigned_by_principal_id,
        position,
    )?;
    let mut expected_spec = previous.spec.clone();
    expected_spec["assignee_principal_id"] = json!(assignee);
    check_criterion_assignee(conn, next, assignee).map_err(|_| invalid_state())?;
    require(
        awaiting_assignment(previous)
            && awaiting_assignment(next)
            && next.generation == previous.generation + 1
            && previous.reference() == change.previous_work_item_ref
            && change.previous_assignee_principal_id == assigned(previous)?
            && assignee != assigned(previous)?
            && role_allows(&project, assignee, CONTRIBUTE)?
            && !change.assigned_at.is_empty()
            && preceding(conn, assignee, "Principal", position)?.body["lifecycle"] == "Active"
            && next.spec == expected_spec
            && next.body == assignment_body(previous, &change)?,
        "RecoveryRequired",
        "work item assignment disagrees with its previous revision",
    )?;
    Ok(())
}

fn verify_abandonment(conn: &Connection, previous: &Doc, next: &Doc) -> Result<()> {
    let change: Abandonment = decode(&next.body["abandonment"]).map_err(|_| invalid_state())?;
    manager_project(
        conn,
        next,
        &change.project_ref,
        &change.abandoned_by_principal_id,
        sequence(conn, next)?,
    )?;
    require(
        may_abandon(previous)
            && previous.reference() == change.previous_work_item_ref
            && previous.body["lifecycle"] == change.prior_lifecycle
            && reason_ok(&change.reason)
            && !change.abandoned_at.is_empty()
            && next.body == abandonment_body(previous, &change)?,
        "RecoveryRequired",
        "work item abandonment disagrees with its previous revision",
    )
}

fn verify_lifecycle(conn: &Connection, previous: &Doc, next: &Doc) -> Result<()> {
    let prior = previous.body["lifecycle"].as_str();
    let state = next.body["lifecycle"].as_str();
    let valid = match (prior, state) {
        (Some("Ready"), Some("InReview")) => {
            let position = sequence(conn, next)?;
            for id in blocker_ids(next)? {
                let blocker = preceding(conn, &id, ITEM, position)?;
                require(
                    blocker.spec["project_id"] == next.spec["project_id"]
                        && blocker.body["lifecycle"] == "Closed",
                    "RecoveryRequired",
                    "work item evidence bypassed an unresolved blocker",
                )?;
            }
            awaiting_assignment(previous)
                && next.body["evidence_ref"].is_object()
                && next.body.get("review").is_none()
                && next.body["updated_by_principal_id"] == assigned(next)?
        }
        (Some("InReview"), Some("Ready" | "Verified")) => {
            verify_review(conn, next, &next.body["review"])?;
            next.body["reviewed_item_ref"] == previous.reference()
                && next.body["updated_by_principal_id"]
                    == next.body["review"]["reviewer_principal_id"]
                && if state == Some("Ready") {
                    next.body.get("evidence_ref").is_none()
                } else {
                    next.body["evidence_ref"] == previous.body["evidence_ref"]
                }
        }
        (Some("Verified"), Some("Closed")) => {
            let mut expected = previous.body.clone();
            expected["lifecycle"] = json!("Closed");
            expected["updated_by_principal_id"] = next.body["updated_by_principal_id"].clone();
            next.body == expected
        }
        _ => false,
    };
    require(
        valid,
        "RecoveryRequired",
        "invalid work item lifecycle transition",
    )
}

pub(super) fn verify_history(
    conn: &Connection,
    original: &Doc,
    current: &Doc,
    format: u32,
) -> Result<Option<Doc>> {
    let maintenance_enabled = format >= MAINTENANCE_FORMAT;
    require(
        original.generation == 1
            && original.body
                == json!({"lifecycle":"Ready","updated_by_principal_id":original.spec["created_by_principal_id"]}),
        "RecoveryRequired",
        "invalid work item creation state",
    )?;
    verify_principal(conn, assigned(original)?)?;
    let mut statement =
        conn.prepare("SELECT * FROM versions WHERE id=?1 AND rev>1 ORDER BY rev")?;
    let mut previous = original.clone();
    let mut abandoned = None;
    for row in statement.query_map([&current.id], row)? {
        let next = row?;
        require(
            next.kind == ITEM
                && next.id == previous.id
                && next.owner == previous.owner
                && next.rev == previous.rev + 1,
            "RecoveryRequired",
            "work item has an invalid revision sequence",
        )?;
        if priority::changed(&previous, &next) {
            require(
                format >= priority::FORMAT,
                "RecoveryRequired",
                "work item reprioritization requires the priority format",
            )?;
            priority::verify_change(conn, &previous, &next)?;
        } else if next.spec != previous.spec {
            require(
                maintenance_enabled,
                "RecoveryRequired",
                "work item assignment requires the maintenance format",
            )?;
            verify_assignment(conn, &previous, &next)?;
        } else {
            require(
                next.generation == previous.generation
                    && next.body.get("assignment") == previous.body.get("assignment"),
                "RecoveryRequired",
                "work item generation or assignment changed on a lifecycle transition",
            )?;
            if next.body["lifecycle"] == "Abandoned" {
                require(
                    maintenance_enabled,
                    "RecoveryRequired",
                    "work item abandonment requires the maintenance format",
                )?;
                verify_abandonment(conn, &previous, &next)?;
                abandoned = Some(previous);
            } else {
                require(
                    next.body.get("abandonment").is_none(),
                    "RecoveryRequired",
                    "active work item carries terminal abandonment",
                )?;
                verify_lifecycle(conn, &previous, &next)?;
            }
        }
        previous = next;
    }
    require(
        previous.reference() == current.reference() && previous.spec == current.spec,
        "RecoveryRequired",
        "work item current revision differs from its verified history",
    )?;
    Ok(abandoned)
}
