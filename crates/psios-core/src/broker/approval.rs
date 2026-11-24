use super::*;

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
