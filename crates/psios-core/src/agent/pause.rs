//! Park an Agent session at a boundary that leaves admitted effects with the broker.
use super::*;

const REQUEST: &str = "operator_pause";
const REASON: &str = "OperatorPaused";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pause {
    id: String,
    expected_resource_version: String,
    reason: String,
}

pub(super) fn parked(session: &Doc) -> bool {
    state(session) == "Paused"
        && session.body["pause_reason"] == REASON
        && session.body["outcome_knowledge"] != "Unknown"
}

pub(crate) fn request(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let request: Pause = decode(request)?;
    let session = db.of(&request.id, "Session")?;
    policy::owned(actor, &session)?;
    session.cas(&request.expected_resource_version)?;
    require(
        !request.reason.trim().is_empty()
            && request.reason.len() <= 256
            && !request.reason.chars().any(char::is_control),
        "InvalidArgument",
        "session pause requires a bounded reason",
    )?;
    require(
        session.body["current_lease"].is_null()
            && session.body["outcome_knowledge"] != "Unknown"
            && (matches!(state(&session), "AwaitingModel" | "WaitingTask") || parked(&session)),
        "PreconditionFailed",
        "session has an unsettled model turn, unknown outcome or no pausable work",
    )?;
    if let Some(existing) = session.body.get(REQUEST) {
        require(
            existing["reason"] == request.reason,
            "Conflict",
            "session already has a different operator pause",
        )?;
        return Ok(response(&session));
    }
    let mut body = session.body.clone();
    let after = if state(&session) == "WaitingTask" {
        let task = session.body["last_task"].as_str().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "session has no task at its pause boundary",
            )
        })?;
        require(
            session_for_task(db, task)?.as_deref() == Some(&session.id),
            "RecoveryRequired",
            "session pause boundary refers to another task",
        )?;
        Some(task)
    } else {
        body["lifecycle"] = json!("Paused");
        body["pause_reason"] = json!(REASON);
        None
    };
    body[REQUEST] = json!({
        "reason": request.reason, "requested_by": actor, "requested_at": now(), "after_task_id": after,
    });
    let updated = db.status(&session, body)?;
    db.audit(actor, "Session.Pause", Some(&session.id), "Accepted")?;
    Ok(response(&updated))
}

pub(super) fn reached(session: &Doc, task: &Doc, body: &mut Value) -> bool {
    if session.body[REQUEST]["after_task_id"] != task.id
        || task.body["outcome_knowledge"] == "Unknown"
    {
        return false;
    }
    if state(session) == "Cancelled" || task.body["lifecycle"] != "Succeeded" {
        clear(body);
        return false;
    }
    body["pause_reason"] = json!(REASON);
    true
}

pub(super) fn clear(body: &mut Value) {
    let operator_reason = body["pause_reason"] == REASON;
    let fields = body.as_object_mut().unwrap();
    fields.remove(REQUEST);
    if operator_reason {
        fields.remove("pause_reason");
    }
}
