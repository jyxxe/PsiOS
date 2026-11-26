use super::*;
use crate::node::work_admission;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    tool: String,
    input: Value,
    key: String,
    delegation_id: String,
    deadline_at: Option<String>,
    #[serde(default)]
    requires: Vec<psios_api::TaskOutputRequirement>,
}

#[derive(Clone, Copy)]
enum LocalOrigin<'a> {
    Direct,
    ClaimedModel(&'a str),
    Schedule(&'a str),
    Workflow { run: &'a str, step_key: &'a str },
}

impl<'a> LocalOrigin<'a> {
    fn id(self) -> Option<&'a str> {
        match self {
            Self::Direct => None,
            Self::ClaimedModel(id) | Self::Schedule(id) | Self::Workflow { run: id, .. } => {
                Some(id)
            }
        }
    }

    fn kind(self, actor: &str) -> &'static str {
        match self {
            Self::Direct | Self::ClaimedModel(_) if actor == policy::OWNER => "HumanRequest",
            Self::Direct | Self::ClaimedModel(_) => "Service",
            Self::Schedule(_) => "Schedule",
            Self::Workflow { .. } => "WorkflowRun",
        }
    }

    fn authority(self, db: &Db<'_>, actor: &str) -> Result<String> {
        match self {
            Self::Direct | Self::ClaimedModel(_) => Ok(actor.to_owned()),
            _ => db.meta("authority"),
        }
    }
}

pub fn prepare(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    prepare_with_origin(db, actor, decode(p)?, LocalOrigin::Direct)
}
pub fn prepare_claimed_model(db: &Db<'_>, actor: &str, p: &Value, lease: &str) -> Result<Value> {
    let p: Prepare = decode(p)?;
    require(
        p.key == lease,
        "Fenced",
        "tool preparation does not belong to the model lease",
    )?;
    prepare_with_origin(db, actor, p, LocalOrigin::ClaimedModel(lease))
}
pub fn prepare_scheduled(db: &Db<'_>, actor: &str, p: &Value, schedule: &str) -> Result<Value> {
    prepare_with_origin(db, actor, decode(p)?, LocalOrigin::Schedule(schedule))
}
pub fn prepare_workflow(
    db: &Db<'_>,
    actor: &str,
    p: &Value,
    run: &str,
    step_key: &str,
) -> Result<Value> {
    prepare_with_origin(
        db,
        actor,
        decode(p)?,
        LocalOrigin::Workflow { run, step_key },
    )
}
fn prepare_with_origin(
    db: &Db<'_>,
    actor: &str,
    p: Prepare,
    origin: LocalOrigin<'_>,
) -> Result<Value> {
    label(&p.key)?;
    Action::parse(&p.tool, &p.input)?;
    let d = policy::delegation(db, actor, &p.delegation_id, "task.submit")?;
    policy::delegation(db, actor, &d.id, &p.tool)?;
    let (kind, body) = if p.requires.is_empty() {
        ("Input", p.input)
    } else {
        let mut requires = p.requires;
        dependencies::authorize(db, actor, &requires)?;
        requires.sort_unstable_by(|a, b| a.task_id.cmp(&b.task_id));
        (
            dependencies::INPUT_KIND,
            json!({"tool_input":p.input,"requires":requires}),
        )
    };
    let origin_id = origin.id().unwrap_or(&p.key);
    let authority = origin.authority(db, actor)?;
    let mut reference = origin_ref(&authority, origin.kind(actor), origin_id);
    if let LocalOrigin::Workflow { step_key, .. } = origin {
        reference["step_key"] = json!(step_key);
        reference["attempt_generation"] = json!("1");
    }
    let mut s = json!({
        "schema_version": VERSION,
        "task_type": p.tool,
        "origin_ref": reference,
        "idempotency_key": p.key,
        "cancel_scope_id": origin_id,
        "delegation_ref": d.reference(),
        "budget_ref": d.body["budget_ref"]
    });
    if let Some(t) = p.deadline_at {
        s["deadline_at"] = json!(t)
    }
    if !matches!(origin, LocalOrigin::Direct) {
        s["input_ref"] = policy::intern_input_kind(db, actor, kind, &body)?.reference();
        return seal_request(s);
    }
    prepare_direct(db, actor, s, kind, &body, &d)
}

fn prepare_direct(
    db: &Db<'_>,
    actor: &str,
    mut request: Value,
    kind: &str,
    body: &Value,
    delegation: &Doc,
) -> Result<Value> {
    let origin = origin(actor, LocalOrigin::Direct.kind(actor), &request, None)?;
    if let Some(previous) = prior(db, actor, &origin, field(&request, "idempotency_key"))? {
        let accepted = previous
            .receipt
            .as_ref()
            .ok_or_else(|| err("Cancelled", "submission was blocked by origin cancellation"))?;
        let task = db.of(field(accepted, "task_id"), "Task")?;
        let input = db.resolve(&task.spec["input_ref"])?;
        require(
            input.owner == actor && input.kind == kind && input.body == *body,
            "IdempotencyConflict",
            "submission key bound to another request",
        )?;
        request["input_ref"] = input.reference();
        let request = seal_request(request)?;
        check_key(&request, &previous)?;
        return Ok(request);
    }
    request["input_ref"] = preparation::reference(actor, kind, body);
    let request = seal_request(request)?;
    preparation::stage(db, actor, &origin, &request, kind, body, delegation)?;
    Ok(request)
}
pub fn retime_scheduled(template: &Value, trigger_id: &str, deadline_at: &str) -> Result<Value> {
    let mut s = template.clone();
    s["idempotency_key"] = json!(trigger_id);
    s["deadline_at"] = json!(deadline_at);
    seal_request(s)
}
pub(crate) fn bind_workflow_input(
    db: &Db<'_>,
    actor: &str,
    template: &Value,
    input: &Value,
) -> Result<Value> {
    require(
        template["origin_ref"]["origin_type"] == "WorkflowRun",
        "RecoveryRequired",
        "workflow input requires a workflow request template",
    )?;
    Action::parse(field(template, "task_type"), input)?;
    let mut s = template.clone();
    s["input_ref"] = policy::intern_input(db, actor, input)?.reference();
    seal_request(s)
}
fn seal_request(mut s: Value) -> Result<Value> {
    s["request_digest"] = json!(digest("task-submit", "request_digest", &s)?);
    contract("task-submit", &s)?;
    Ok(s)
}
fn origin_ref(authority: &str, kind: &str, id: &str) -> Value {
    json!({"authority_id": authority, "origin_type": kind, "origin_id": id})
}

fn origin(authority: &str, kind: &str, s: &Value, expected_id: Option<&str>) -> Result<String> {
    let o = &s["origin_ref"];
    require(
        o["authority_id"] == authority
            && o["origin_type"] == kind
            && expected_id.is_none_or(|id| o["origin_id"] == id),
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
    let d = delegation(db, actor, s)?;
    let input = db.resolve(&s["input_ref"])?;
    input_owner(actor, &input)?;
    Ok((d, input))
}

fn delegation(db: &Db<'_>, actor: &str, s: &Value) -> Result<Doc> {
    policy::active(db, actor)?;
    let d = db.resolve(&s["delegation_ref"])?;
    policy::delegation(db, actor, &d.id, "task.submit")?;
    policy::delegation(db, actor, &d.id, field(s, "task_type"))?;
    require(
        d.body["budget_ref"] == s["budget_ref"],
        "PermissionDenied",
        "budget outside delegation",
    )?;
    Ok(d)
}

fn input_owner(actor: &str, input: &Doc) -> Result<()> {
    require(
        matches!(input.kind.as_str(), "Input" | dependencies::INPUT_KIND) && input.owner == actor,
        "PermissionDenied",
        "input outside delegation",
    )
}
fn deadline(s: &Value) -> Result<()> {
    if let Some(t) = s["deadline_at"].as_str() {
        require(!expired(t)?, "DeadlineExceeded", "task deadline passed")?
    }
    Ok(())
}
fn validate_submit(db: &Db<'_>, actor: &str, s: &Value, source: LocalOrigin<'_>) -> Result<String> {
    contract("task-submit", s)?;
    if let LocalOrigin::Workflow { step_key, .. } = source {
        require(
            s["origin_ref"]["step_key"] == step_key && s["origin_ref"]["attempt_generation"] == "1",
            "PermissionDenied",
            "workflow task origin does not match its admitted step",
        )?;
    }
    origin(
        &source.authority(db, actor)?,
        source.kind(actor),
        s,
        source.id(),
    )
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
fn plan(db: &Db<'_>, actor: &str, s: &Value, audience: &[Value]) -> Result<Doc> {
    let profile = db.get(policy::RUNTIME_PROFILE)?.reference();
    let tool = field(s, "task_type");
    let target = &audience[0];
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
        "preconditions": audience.iter().map(|r|json!({"resource_ref":r,"kind":"ContentDigest","expected":r["digest"],"enforcement":"ConditionalApply"})).collect::<Vec<_>>(),
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
    submit_with_origin(db, actor, s, retry, LocalOrigin::Direct)
}

// Called only as part of the transaction consuming a provider's authorized
// decision. Keep the existing direct Task origin, keyed to this exact lease.
pub(crate) fn submit_claimed_model_tool(
    db: &Db<'_>,
    session: &Doc,
    lease: &str,
    request: &Value,
) -> Result<Value> {
    require(
        !db.c.is_autocommit(),
        "RecoveryRequired",
        "model decision requires an authority transaction",
    )?;
    let current = db.of(&session.id, "Session")?;
    require(
        current.reference() == session.reference()
            && current.owner == session.owner
            && current.body["lifecycle"] == "ModelInFlight"
            && current.body["current_lease"] == lease
            && request["idempotency_key"] == lease
            && request["delegation_ref"]["resource_id"] == current.spec["delegation_id"]
            && agent::model_tool_lease_live(db, &current)?,
        "Fenced",
        "tool decision no longer belongs to the issued model lease",
    )?;
    submit_with_origin(
        db,
        &current.owner,
        request,
        None,
        LocalOrigin::ClaimedModel(lease),
    )
}

pub fn submit_scheduled(db: &Db<'_>, actor: &str, s: &Value, schedule: &str) -> Result<Value> {
    submit_with_origin(db, actor, s, None, LocalOrigin::Schedule(schedule))
}
pub fn submit_workflow(
    db: &Db<'_>,
    actor: &str,
    s: &Value,
    run: &str,
    step_key: &str,
) -> Result<Value> {
    submit_with_origin(db, actor, s, None, LocalOrigin::Workflow { run, step_key })
}
fn submit_with_origin(
    db: &Db<'_>,
    actor: &str,
    s: &Value,
    retry: Option<&str>,
    source: LocalOrigin<'_>,
) -> Result<Value> {
    let origin = validate_submit(db, actor, s, source)?;
    let d = delegation(db, actor, s)?;
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
    let staged = matches!(source, LocalOrigin::Direct)
        && retry.is_none()
        && preparation::is_prepared(&s["input_ref"]);
    let input = if staged {
        preparation::load(db, actor, &origin, s)?.ok_or_else(|| {
            err(
                "DeadlineExceeded",
                "task preparation is unavailable or expired",
            )
        })?
    } else {
        db.resolve(&s["input_ref"])?
    };
    input_owner(actor, &input)?;
    let a = Action::parse(tool, dependencies::tool_input(&input)?)?;
    let target = a.target(db, actor)?;
    let audience = a.audience(db, actor, &s["delegation_ref"], &target)?;
    let stop_cleanup = if let Action::ContinueOciStop(request) = &a {
        Some(apps::stop::admissible(db, &target, request)?)
    } else {
        None
    };
    let requirements = dependencies::requirements(&input)?;
    let recovery_only =
        !matches!(source, LocalOrigin::ClaimedModel(_)) && work_admission::draining(db)?;
    if !recovery_only {
        db.cfg.check_admission_space()?;
    }
    let recovery = admission::select(
        db,
        actor,
        &a,
        &target,
        !requirements.is_empty(),
        recovery_only,
    )?;
    if recovery_only {
        db.cfg.check_admission_space()?;
    }
    artifact::capacity::check_admission(db, actor, tool)?;
    let budget = field(&s["budget_ref"], "resource_id");
    if recovery.is_some() {
        require(
            concurrency::usage(db, budget)?.is_none_or(|slots| slots.occupied < slots.maximum),
            "QuotaExceeded",
            "recovery task requires an available execution slot in its root budget",
        )?;
    }
    require(
        db.c.execute(
            "UPDATE budgets SET reserved=reserved+1 WHERE id=?1 AND reserved+spent<maximum",
            [budget],
        )? == 1,
        "QuotaExceeded",
        "delegation has no available execution units",
    )?;
    let ready = dependencies::ready(db, actor, &requirements, None)?.is_some();
    if staged {
        preparation::promote(db, &input)?;
    }
    let plan = plan(db, actor, s, &audience)?;
    let task = id("task");
    let deadline_source = match source {
        LocalOrigin::Workflow { run, .. } => Some(run),
        LocalOrigin::Schedule(schedule) => Some(schedule),
        LocalOrigin::Direct | LocalOrigin::ClaimedModel(_) => retry,
    };
    policy::deadline::Clock::sample()?.register_until(
        db,
        &task,
        field(&plan.body, "deadline_at"),
        deadline_source,
    )?;
    let op = id("op");
    let at = now();
    let approval = ready && policy::approval(tool);
    let state = if !ready {
        "WaitingResource"
    } else if approval {
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
    let conditions = if !ready {
        vec![dependencies::waiting()]
    } else if approval {
        vec![approval::request(db, actor, &task, &plan)?]
    } else {
        Vec::new()
    };
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
            "profile_ref": db.get(policy::RUNTIME_PROFILE)?.reference(),
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
    if let Some(operation) = &recovery {
        admission::bind(db, &t, operation)?;
    }
    if let Some(prepared) = &stop_cleanup {
        apps::stop::admit(db, &t, prepared)?;
    }
    artifact::capacity::reserve(db, &t, tool)?;
    apps::artifact_install::acquire(db, &t, &a, &audience)?;
    if let Action::StartAgentSession(blueprint) = &a {
        agent::session_inputs::pin_task(db, &t, blueprint)?;
    }
    dependencies::pin(db, &t, &requirements)?;
    if staged {
        preparation::consume(db, actor, &origin, s)?;
    }
    db.audit(actor, "Task.Accept", Some(&task), "Allowed")?;
    Ok(receipt)
}
pub fn lookup(db: &Db<'_>, actor: &str, s: &Value) -> Result<Value> {
    let origin = validate_submit(db, actor, s, LocalOrigin::Direct)?;
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
    } else if preparation::is_prepared(&s["input_ref"]) {
        if let Some(window) = preparation::window(db, actor, &origin, s)? {
            out["status"] = json!("AbsentWithinWindow");
            out["window_not_after"] = json!(window);
            out["consistency_evidence_ref"] = grant.reference()
        } else {
            out["status"] = json!("Expired");
            out["reason"] = json!("Task preparation is unavailable or expired")
        }
    } else {
        out["status"] = json!("AbsentWithinWindow");
        out["window_not_after"] = d.body["valid_until"].clone();
        out["consistency_evidence_ref"] = grant.reference()
    }
    contract("task-lookup", &out)?;
    Ok(out)
}

pub fn discard_preparation(db: &Db<'_>, actor: &str, s: &Value) -> Result<Value> {
    let origin = validate_submit(db, actor, s, LocalOrigin::Direct)?;
    delegation(db, actor, s)?;
    require(
        preparation::is_prepared(&s["input_ref"]),
        "InvalidArgument",
        "request does not refer to a task preparation",
    )?;
    if let Some(previous) = prior(db, actor, &origin, field(s, "idempotency_key"))? {
        check_key(s, &previous)?;
        return Ok(json!({"status":if previous.receipt.is_some() {
            "AlreadyAccepted"
        } else {
            "AlreadyDiscarded"
        }}));
    }
    require(
        preparation::window(db, actor, &origin, s)?.is_some(),
        "DeadlineExceeded",
        "task preparation is unavailable or expired",
    )?;
    preparation::consume(db, actor, &origin, s)?;
    record_cancellation(db, actor, &origin, s)?;
    db.audit(actor, "Task.DiscardPreparation", None, "Allowed")?;
    Ok(json!({"status":"Discarded"}))
}

fn record_cancellation(db: &Db<'_>, actor: &str, origin: &str, s: &Value) -> Result<()> {
    db.c.execute(
        "INSERT INTO submissions(owner,origin,key,digest,scope,cancelled) VALUES(?1,?2,?3,?4,?5,1)",
        params![
            actor,
            origin,
            field(s, "idempotency_key"),
            field(s, "request_digest"),
            field(s, "cancel_scope_id")
        ],
    )?;
    Ok(())
}

pub fn cancel_origin(db: &Db<'_>, actor: &str, s: &Value) -> Result<Value> {
    contract("cancel-origin", s)?;
    policy::can(db, actor, "task.submit")?;
    let o = origin(actor, LocalOrigin::Direct.kind(actor), s, None)?;
    let key = field(s, "idempotency_key");
    let digest = sha256(&canonical(&json!([actor, s["cancel_request_id"]])));
    let record_id = format!("cancellation-{}", &digest[7..]);
    match db.get(&record_id) {
        Ok(existing) => require(
            existing.body == *s,
            "IdempotencyConflict",
            "cancel request ID already bound",
        )?,
        Err(e) if e.0.code == "NotFound" => {
            db.create("Cancellation", &record_id, actor, json!({}), s.clone())?;
        }
        Err(e) => return Err(e),
    }
    if let Some(old) = prior(db, actor, &o, key)? {
        check_key(s, &old)?;
        if let Some(r) = old.receipt {
            let t = db.get(field(&r, "task_id"))?;
            return Ok(cancel_task(db, &t)?.body);
        }
        require(old.cancelled, "InternalError", "invalid submission record")?;
    } else {
        preparation::cancel(db, actor, &o, key, field(s, "request_digest"))?;
        record_cancellation(db, actor, &o, s)?;
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
    let req: Retry = decode(p)?;
    let t = db.of(&req.id, "Task")?;
    policy::owned(actor, &t)?;
    require(
        t.body["origin_ref"]["origin_type"] != "WorkflowRun",
        "PreconditionFailed",
        "workflow-owned tasks retain their original step attempt",
    )?;
    require(
        agent::session_for_task(db, &t.id)?.is_none(),
        "PreconditionFailed",
        "use Session.RetryTask for a session-owned tool",
    )?;
    retry_in_session(db, actor, p)
}
pub fn retry_in_session(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
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
        t.body["outcome_knowledge"] != "Unknown",
        "OutcomeUnknown",
        "reconcile the original operation before retry",
    )?;
    require(
        matches!(t.body["lifecycle"].as_str(), Some("Failed" | "Cancelled")),
        "Conflict",
        "task is not retryable",
    )?;
    require(
        policy::readonly(field(&t.spec, "task_type"))
            || t.body["outcome_knowledge"] == "NotStarted",
        "PreconditionFailed",
        "write already dispatched; a new effect requires its own approved plan",
    )?;
    let mut s = t.spec.clone();
    require(
        s["idempotency_key"] != p.key,
        "InvalidArgument",
        "retry requires new key",
    )?;
    if s["origin_ref"]["origin_type"] == "Schedule" {
        s["origin_ref"] = origin_ref(actor, LocalOrigin::Direct.kind(actor), &p.key);
        s["cancel_scope_id"] = json!(p.key);
    }
    s["idempotency_key"] = json!(p.key);
    s["request_digest"] = json!(digest("task-submit", "request_digest", &s)?);
    submit(db, actor, &s, Some(&t.id))
}
