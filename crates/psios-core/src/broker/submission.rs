use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    tool: String,
    input: Value,
    key: String,
    delegation_id: String,
    deadline_at: Option<String>,
}
pub fn prepare(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Prepare = decode(p)?;
    label(&p.key)?;
    Action::parse(&p.tool, &p.input)?;
    let d = policy::delegation(db, actor, &p.delegation_id, "task.submit")?;
    policy::delegation(db, actor, &d.id, &p.tool)?;
    let input = policy::intern_input(db, actor, &p.input)?;
    let mut s = json!({
        "schema_version": VERSION,
        "task_type": p.tool,
        "origin_ref": {
            "authority_id": actor,
            "origin_type": if actor==policy::OWNER{"HumanRequest"}else{"Service"},
            "origin_id": p.key
        },
        "idempotency_key": p.key,
        "cancel_scope_id": p.key,
        "input_ref": input.reference(),
        "delegation_ref": d.reference(),
        "budget_ref": d.body["budget_ref"]
    });
    if let Some(t) = p.deadline_at {
        s["deadline_at"] = json!(t)
    }
    s["request_digest"] = json!(digest("task-submit", "request_digest", &s)?);
    contract("task-submit", &s)?;
    Ok(s)
}
fn origin(actor: &str, s: &Value) -> Result<String> {
    let o = &s["origin_ref"];
    require(
        o["authority_id"] == actor
            && o["origin_type"]
                == if actor == policy::OWNER {
                    "HumanRequest"
                } else {
                    "Service"
                },
        "PermissionDenied",
        "caller cannot represent this origin",
    )?;
    require(
        s["cancel_scope_id"] == o["origin_id"],
        "UnsupportedCapability",
        "local cancellation scope must equal origin ID",
    )?;
    Ok(String::from_utf8(canonical(o)).unwrap())
}
pub(super) fn authority(db: &Db<'_>, actor: &str, s: &Value) -> Result<(Doc, Doc)> {
    policy::active(db, actor)?;
    let d = db.resolve(&s["delegation_ref"])?;
    policy::delegation(db, actor, &d.id, "task.submit")?;
    policy::delegation(db, actor, &d.id, field(s, "task_type"))?;
    require(
        d.body["budget_ref"] == s["budget_ref"],
        "PermissionDenied",
        "budget outside delegation",
    )?;
    let input = db.resolve(&s["input_ref"])?;
    require(
        input.kind == "Input" && input.owner == actor,
        "PermissionDenied",
        "input outside delegation",
    )?;
    Ok((d, input))
}
pub(super) fn deadline(s: &Value) -> Result<()> {
    if let Some(t) = s["deadline_at"].as_str() {
        require(!expired(t)?, "DeadlineExceeded", "task deadline passed")?
    }
    Ok(())
}
fn validate_submit(actor: &str, s: &Value) -> Result<String> {
    contract("task-submit", s)?;
    origin(actor, s)
}
struct Submission {
    digest: String,
    scope: String,
    receipt: Option<Value>,
    cancelled: bool,
}
fn prior(db: &Db<'_>, owner: &str, origin: &str, key: &str) -> Result<Option<Submission>> {
    Ok(db.c.query_row("SELECT digest,scope,receipt,cancelled FROM submissions WHERE owner=?1 AND origin=?2 AND key=?3",params![owner,origin,key],
        |r|Ok(Submission{digest:r.get(0)?,scope:r.get(1)?,receipt:r.get(2)?,cancelled:r.get(3)?})).optional()?)
}
fn check_key(s: &Value, old: &Submission) -> Result<()> {
    require(
        s["request_digest"] == old.digest && s["cancel_scope_id"] == old.scope,
        "IdempotencyConflict",
        "submission key bound to another request",
    )
}
fn plan(db: &Db<'_>, actor: &str, s: &Value, target: &Doc) -> Result<Doc> {
    let profile = db.get(policy::PROFILE)?.reference();
    let tool = field(s, "task_type");
    let target = target.reference();
    let pid = id("plan");
    let d = db.resolve(&s["delegation_ref"])?;
    let g = db.get(field(&d.body, "grant_id"))?;
    let grant_expiry = field(&g.spec, "valid_until").min(field(&d.body, "valid_until"));
    let requested = s["deadline_at"].as_str().unwrap_or(grant_expiry);
    let deadline = if chrono::DateTime::parse_from_rfc3339(requested).unwrap()
        < chrono::DateTime::parse_from_rfc3339(grant_expiry).unwrap()
    {
        requested
    } else {
        grant_expiry
    };
    let mut p = json!({
        "schema_version": VERSION,
        "plan_id": pid,
        "purpose_ref": s["input_ref"],
        "requester_principal_id": actor,
        "delegation_ref": s["delegation_ref"],
        "allowed_effects": [{"resource_ref":target,"action":tool,"scope_ref":s["delegation_ref"]}],
        "input_refs": [s["input_ref"]],
        "runtime_refs": [db.get(&format!("tool-{tool}"))?.reference()],
        "preconditions": [{"resource_ref":target,"kind":"ContentDigest","expected":target["digest"],"enforcement":"ConditionalApply"}],
        "budget_ref": s["budget_ref"],
        "deadline_at": deadline,
        "data_policy_ref": profile,
        "permission_delta_refs": [],
        "recovery_plan_ref": profile,
        "irreversible_effects": match tool { "volume.delete" => vec!["Volume content deletion"], "backup.delete" => vec!["Backup content deletion"], _ => vec![] },
        "use_scope": "SingleOperation",
        "digest_profile": {
            "digest_algorithm": "SHA-256",
            "canonicalization_version": "RFC8785",
            "domain": "psios/operation-plan/1"
        }
    });
    p["plan_digest"] = json!(digest("operation-plan", "plan_digest", &p)?);
    db.create("OperationPlan", &pid, actor, json!({"action": tool}), p)
}
pub fn submit(db: &Db<'_>, actor: &str, s: &Value, retry: Option<&str>) -> Result<Value> {
    let origin = validate_submit(actor, s)?;
    let (d, input) = authority(db, actor, s)?;
    let tool = field(s, "task_type");
    let key = field(s, "idempotency_key");
    let digest = field(s, "request_digest");
    if let Some(old) = prior(db, actor, &origin, key)? {
        check_key(s, &old)?;
        if let Some(mut receipt) = old.receipt {
            policy::can(db, actor, "task.read")?;
            receipt["deduplicated"] = json!(true);
            return Ok(receipt);
        }
        return Err(err(
            "Cancelled",
            "submission was blocked by origin cancellation",
        ));
    }
    deadline(s)?;
    let a = Action::parse(tool, &input.body)?;
    let target = a.target(db, actor)?;
    let depth: u32 = db.c.query_row(
        "SELECT count(*) FROM queue WHERE phase NOT IN ('Done')",
        [],
        |r| r.get(0),
    )?;
    require(
        depth < db.cfg.queue_limit,
        "QueueFull",
        "durable queue full",
    )?;
    let budget = field(&s["budget_ref"], "resource_id");
    require(
        db.c.execute(
            "UPDATE budgets SET reserved=reserved+1 WHERE id=?1 AND reserved+spent<maximum",
            [budget],
        )? == 1,
        "QuotaExceeded",
        "delegation has no available execution units",
    )?;
    let plan = plan(db, actor, s, &target)?;
    let task = id("task");
    let op = id("op");
    let at = now();
    let approval = policy::approval(tool);
    let state = if approval {
        "WaitingApproval"
    } else {
        "Queued"
    };
    let opbody = json!({
        "schema_version": VERSION,
        "operation_id": op,
        "plan_ref": plan.reference(),
        "lifecycle": if approval{"WaitingApproval"}else{"Pending"},
        "outcome_knowledge": "NotStarted",
        "compensation_status": "NotRequired",
        "effect_refs": [],
        "conditions": [],
        "created_at": at,
        "updated_at": at
    });
    let operation = db.create("Operation", &op, actor, json!({"task_id": task}), opbody)?;
    let mut conditions = Vec::new();
    if approval {
        let req = id("approval");
        let b = json!({
            "schema_version": VERSION,
            "approval_request_id": req,
            "plan_ref": plan.reference(),
            "plan_digest": plan.body["plan_digest"],
            "requester_principal_id": actor,
            "approver_policy_ref": db.get(policy::PROFILE)?.reference(),
            "status": "Pending",
            "expires_at": plan.body["deadline_at"]
        });
        db.create("ApprovalRequest", &req, actor, json!({"task_id": task}), b)?;
        db.c.execute(
            "INSERT INTO approval_bindings(request,task) VALUES(?1,?2)",
            params![req, task],
        )?;
        conditions.push(condition("ApprovalRequired", &req))
    }
    let mut body = json!({
        "schema_version": VERSION,
        "task_id": task,
        "origin_ref": s["origin_ref"],
        "request_digest": digest,
        "lifecycle": state,
        "outcome_knowledge": "NotStarted",
        "cancel_scope_id": s["cancel_scope_id"],
        "result": {"availability": "Pending", "artifact_refs": []},
        "operation_refs": [operation.reference()],
        "conditions": conditions
    });
    if let Some(prev) = retry {
        body["retry_of_task_id"] = json!(prev)
    }
    let t = db.create("Task", &task, actor, s.clone(), body)?;
    let receipt = json!({
        "schema_version": VERSION,
        "task_id": task,
        "task_revision": t.version(),
        "accepted_at": at,
        "request_digest": digest,
        "origin_ref": s["origin_ref"],
        "idempotency_key": key,
        "deduplicated": false,
        "durability_profile": {
            "profile_ref": db.get(policy::PROFILE)?.reference(),
            "ack_level": "Durable",
            "fault_scope": ["ProcessCrash"],
            "rpo_seconds": "0"
        }
    });
    contract("task-accepted", &receipt)?;
    db.c.execute("INSERT INTO submissions(owner,origin,key,digest,scope,task,receipt) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![actor,origin,key,digest,field(s,"cancel_scope_id"),task,receipt])?;
    db.c.execute(
        "INSERT INTO queue(task,stamp,phase,budget,operation,grant_id,valid_until) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            task,
            db.c.last_insert_rowid(),
            state,
            budget,
            op,
            field(&d.body, "grant_id"),field(&plan.body,"deadline_at")
        ],
    )?;
    db.audit(actor, "Task.Accept", Some(&task), "Allowed")?;
    Ok(receipt)
}
pub fn lookup(db: &Db<'_>, actor: &str, s: &Value) -> Result<Value> {
    let origin = validate_submit(actor, s)?;
    policy::can(db, actor, "task.read")?;
    let d = db.resolve(&s["delegation_ref"])?;
    require(
        d.kind == "Delegation" && d.owner == actor,
        "PermissionDenied",
        "invalid submission namespace",
    )?;
    let grant = db.get(field(&d.body, "grant_id"))?;
    let mut out = json!({
        "schema_version": VERSION,
        "authority_id": db.meta("authority")?,
        "recovery_incarnation": db.incarnation,
        "checked_at": now(),
        "origin_ref": s["origin_ref"],
        "idempotency_key": s["idempotency_key"],
        "request_digest": s["request_digest"],
        "namespace_ref": d.reference()
    });
    let active = match policy::delegation(db, actor, &d.id, field(s, "task_type")) {
        Ok(_) => true,
        Err(e) if e.0.code == "PermissionDenied" => false,
        Err(e) => return Err(e),
    };
    if !active {
        out["status"] = json!("Expired");
        out["reason"] = json!("Delegation namespace closed");
    } else if let Some(old) = prior(db, actor, &origin, field(s, "idempotency_key"))? {
        check_key(s, &old)?;
        if let Some(r) = old.receipt {
            out["status"] = json!("Found");
            out["accepted"] = r
        } else {
            out["status"] = json!("Expired");
            out["reason"] = json!("Cancellation tombstone")
        }
    } else {
        out["status"] = json!("AbsentWithinWindow");
        out["window_not_after"] = d.body["valid_until"].clone();
        out["consistency_evidence_ref"] = grant.reference()
    }
    contract("task-lookup", &out)?;
    Ok(out)
}
pub fn cancel_origin(db: &Db<'_>, actor: &str, s: &Value) -> Result<Value> {
    contract("cancel-origin", s)?;
    policy::can(db, actor, "task.submit")?;
    let o = origin(actor, s)?;
    let key = field(s, "idempotency_key");
    if let Some(old) = prior(db, actor, &o, key)? {
        check_key(s, &old)?;
        if let Some(r) = old.receipt {
            let t = db.get(field(&r, "task_id"))?;
            return Ok(cancel_task(db, &t)?.body);
        }
        require(old.cancelled, "InternalError", "invalid submission record")?;
    } else {
        db.c.execute("INSERT INTO submissions(owner,origin,key,digest,scope,cancelled) VALUES(?1,?2,?3,?4,?5,1)",
            params![actor,o,key,field(s,"request_digest"),field(s,"cancel_scope_id")])?;
    }
    Ok(json!({"status": "Blocked", "cancel_request_id": s["cancel_request_id"]}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    id: String,
    key: String,
    expected_resource_version: String,
}
pub fn retry(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Retry = decode(p)?;
    let t = db.of(&p.id, "Task")?;
    require(
        t.owner == actor,
        "PermissionDenied",
        "retry requires original principal",
    )?;
    t.cas(&p.expected_resource_version)?;
    label(&p.key)?;
    require(
        matches!(t.body["lifecycle"].as_str(), Some("Failed" | "Cancelled")),
        "Conflict",
        "task is not retryable",
    )?;
    require(
        policy::readonly(field(&t.spec, "task_type"))
            || t.body["outcome_knowledge"] == "NotStarted",
        "OutcomeUnknown",
        "write retry requires independent reconciliation",
    )?;
    let mut s = t.spec.clone();
    require(
        s["idempotency_key"] != p.key,
        "InvalidArgument",
        "retry requires new key",
    )?;
    s["idempotency_key"] = json!(p.key);
    s["request_digest"] = json!(digest("task-submit", "request_digest", &s)?);
    submit(db, actor, &s, Some(&t.id))
}
