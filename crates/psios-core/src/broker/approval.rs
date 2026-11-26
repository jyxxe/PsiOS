use super::*;
use crate::db::resolve;
use rusqlite::Connection;

pub(super) fn request(db: &Db<'_>, actor: &str, task: &str, plan: &Doc) -> Result<Value> {
    let id = id("approval");
    let body = json!({
        "schema_version": VERSION,
        "approval_request_id": id,
        "plan_ref": plan.reference(),
        "plan_digest": plan.body["plan_digest"],
        "requester_principal_id": actor,
        "approver_policy_ref": db.get(policy::RUNTIME_PROFILE)?.reference(),
        "status": "Pending",
        "expires_at": plan.body["deadline_at"]
    });
    db.create("ApprovalRequest", &id, actor, json!({"task_id":task}), body)?;
    db.c.execute(
        "INSERT INTO approval_bindings(request,task) VALUES(?1,?2)",
        params![id, task],
    )?;
    Ok(condition("ApprovalRequired", &id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    id: String,
    expected_resource_version: String,
    plan_digest: String,
    approve: bool,
}
pub fn decide(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Decision = decode(p)?;
    let req = db.of(&p.id, "ApprovalRequest")?;
    req.cas(&p.expected_resource_version)?;
    let t = db.of(field(&req.spec, "task_id"), "Task")?;
    require(
        req.body["status"] == "Pending"
            && req.body["plan_digest"] == p.plan_digest
            && !policy::deadline::Clock::sample()?.expired(
                db,
                &t.id,
                field(&req.body, "expires_at"),
            )?,
        "ApprovalInvalidated",
        "plan or approval validity changed",
    )?;
    require(
        t.body["lifecycle"] == "WaitingApproval",
        "Conflict",
        "task is no longer waiting",
    )?;
    if p.approve {
        dependencies::check_approval(db, &t)?;
    }
    let plan = db.resolve(&req.body["plan_ref"])?;
    let decision = id("decision");
    let evidence = db.record(
        "ApprovalEvidence",
        &t.owner,
        json!({
            "authenticated_approver": actor,
            "request_id": db.correlation,
            "plan_digest": p.plan_digest,
            "at": now()
        }),
    )?;
    let user_decision = if p.approve { "Approved" } else { "Rejected" };
    let b = json!({
        "schema_version": VERSION,
        "decision_id": decision,
        "request_ref": req.reference(),
        "plan_digest": p.plan_digest,
        "decision": user_decision,
        "approver_principal_id": actor,
        "decided_at": now(),
        "valid_until": req.body["expires_at"],
        "approval_policy_ref": req.body["approver_policy_ref"],
        "preconditions": plan.body["preconditions"],
        "use_limit": "SingleOperation",
        "verification_evidence_ref": evidence.reference()
    });
    let d = db.create("ApprovalDecision", &decision, &t.owner, json!({}), b)?;
    let mut rq = req.body.clone();
    rq["status"] = json!(user_decision);
    rq["decision_ref"] = d.reference();
    db.status(&req, rq)?;
    db.c.execute(
        "UPDATE approval_bindings SET decision=?1 WHERE request=?2",
        params![decision, p.id],
    )?;
    if p.approve {
        let uid = format!("use-{decision}");
        db.create(
            "ApprovalUse",
            &uid,
            &t.owner,
            json!({}),
            json!({
                "schema_version": VERSION,
                "approval_use_id": uid,
                "decision_ref": d.reference(),
                "plan_digest": p.plan_digest,
                "binding_status": "Unbound",
                "validity_status": "Valid"
            }),
        )?;
        let mut b = t.body.clone();
        b["lifecycle"] = json!("Queued");
        b["conditions"] = json!([]);
        db.status(&t, b)?;
        db.c.execute("UPDATE queue SET phase='Queued' WHERE task=?1", [&t.id])?;
    } else {
        close(
            db,
            &t,
            "Cancelled",
            "NotStarted",
            Some(&err("PermissionDenied", "plan rejected")),
            None,
        )?;
    }
    Ok(d.body)
}

pub(crate) fn verify_bound_approval(
    conn: &Connection,
    op: &Doc,
    task: &Doc,
    run: &Doc,
    plan: &Doc,
    effect: &Doc,
    tool: &str,
) -> Result<Doc> {
    let historical = |id: &str, revision: i64| -> Result<Doc> {
        conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![id, revision],
            crate::db::row,
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "original approval history revision is unavailable",
            )
        })
    };
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let proof = resolve(conn, &permit.body["auth_proof"]["record_ref"])?;
    let decision = resolve(conn, &permit.body["decision_ref"])?;
    let request = resolve(conn, &decision.body["request_ref"])?;
    let approval = historical(&request.id, 2)?;
    let used = historical(&format!("use-{}", decision.id), 2)?;
    let initial = historical(&used.id, 1)?;
    let indexed: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM approval_bindings WHERE request=?1 AND task=?2 AND decision=?3 AND operation=?4)",
        params![request.id,task.id,decision.id,op.id], |r|r.get(0))?;
    let audience: Vec<_> = plan.body["preconditions"]
        .as_array()
        .ok_or_else(|| err("RecoveryRequired", "approved permit audience is missing"))?
        .iter()
        .map(|condition| condition["resource_ref"].clone())
        .collect();
    require(
        indexed
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && plan.body["requester_principal_id"] == op.owner
            && plan.body["purpose_ref"] == task.spec["input_ref"]
            && plan.body["allowed_effects"][0]["action"] == tool
            && plan.body["allowed_effects"][0]["resource_ref"] == effect.body["resource_ref"]
            && permit.kind == "ExecutionPermit"
            && permit.rev == 1
            && permit.owner == op.owner
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["preconditions"] == plan.body["preconditions"]
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["audience_resource_refs"] == json!(audience)
            && permit.body["recovery_incarnation"] == run.body["recovery_incarnation"]
            && proof.kind == "PermitBinding"
            && proof.rev == 1
            && proof.owner == op.owner
            && proof.body["operation_id"] == op.id
            && proof.body["effect_id"] == effect.id
            && proof.body["permit_id"] == permit.id
            && proof.body["claim_id"] == run.body["claim_id"]
            && proof.body["claim_epoch"] == run.body["claim_epoch"]
            && proof.body["plan_digest"] == plan.body["plan_digest"]
            && proof.body["audience_resource_refs"] == permit.body["audience_resource_refs"]
            && decision.kind == "ApprovalDecision"
            && decision.rev == 1
            && decision.owner == op.owner
            && decision.body["decision"] == "Approved"
            && decision.body["approver_principal_id"] == crate::policy::OWNER
            && decision.body["use_limit"] == "SingleOperation"
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && decision.body["preconditions"] == plan.body["preconditions"]
            && request.kind == "ApprovalRequest"
            && request.rev == 1
            && request.owner == op.owner
            && request.spec["task_id"] == task.id
            && request.body["plan_ref"] == plan.reference()
            && request.body["status"] == "Pending"
            && approval.kind == request.kind
            && approval.spec == request.spec
            && approval.owner == request.owner
            && approval.body["status"] == "Approved"
            && approval.body["decision_ref"] == decision.reference()
            && used.kind == "ApprovalUse"
            && used.owner == op.owner
            && used.body["binding_status"] == "Bound"
            && used.body["validity_status"] == "Valid"
            && used.body["operation_id"] == op.id
            && used.body["plan_digest"] == plan.body["plan_digest"]
            && used.body["decision_ref"] == decision.reference()
            && initial.kind == used.kind
            && initial.owner == used.owner
            && initial.body["binding_status"] == "Unbound"
            && initial.body["decision_ref"] == decision.reference()
            && initial.body["plan_digest"] == plan.body["plan_digest"],
        "RecoveryRequired",
        "effect disagrees with the originally approved permit and run",
    )?;
    Ok(used)
}
