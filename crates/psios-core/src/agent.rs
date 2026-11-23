//! The local runtime coordinates model decisions with (and never independently executes) broker Tasks.
mod lease;
pub mod mail;
mod registry;
use crate::{
    broker,
    db::{Db, Doc},
    policy, *,
};
pub(crate) use lease::pending_for_grant;
use psios_api::{ModelTokenUsage, canonical, id, sha256};
pub use registry::{model, retire, skill};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    key: String,
    goal: String,
    trigger_signal_ref: Option<Value>,
    delegation_id: String,
    skill_ref: Value,
    model_ref: Value,
    context_policy: String,
    max_turns: u32,
}
fn state(d: &Doc) -> &str {
    d.body["lifecycle"].as_str().unwrap_or("")
}
fn response(d: &Doc) -> Value {
    json!({"reference":d.reference(),"session":d.body})
}
fn deps(db: &Db<'_>, s: &Doc) -> Result<(Doc, Doc, Doc)> {
    require(
        s.body["checkpoint_version"] == "1",
        "SchemaVersionUnsupported",
        "unsupported session checkpoint",
    )?;
    policy::active(db, &s.owner)?;
    let did = s.spec["delegation_id"].as_str().unwrap();
    let d = policy::delegation(db, &s.owner, did, "session.run")?;
    for action in ["task.read", "task.submit"] {
        policy::delegation(db, &s.owner, did, action)?;
    }
    let skill = registry::pinned(db, &s.spec["skill_ref"], "Skill")?;
    for (tool, reference) in skill.spec["tool_refs"].as_object().unwrap() {
        policy::delegation(db, &s.owner, did, tool)?;
        require(
            db.of(&format!("tool-{tool}"), "Capability")?.reference() == *reference,
            "PreconditionFailed",
            "tool implementation changed",
        )?;
    }
    let m = registry::pinned(db, &s.spec["model_ref"], "Model")?;
    let provider = m.spec["provider_principal_id"].as_str().unwrap();
    policy::active(db, provider)?;
    policy::can(db, provider, "model.serve")?;
    Ok((d, skill, m))
}
pub fn open(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let o: Open = decode(p)?;
    label(&o.key)?;
    label(&o.delegation_id)?;
    require(
        (1..=8192).contains(&o.goal.len())
            && (1..=32).contains(&o.max_turns)
            && matches!(
                o.context_policy.as_str(),
                "ReferencesOnly" | "LocalToolResults"
            ),
        "InvalidArgument",
        "invalid session context or budget",
    )?;
    let d = policy::delegation(db, actor, &o.delegation_id, "session.run")?;
    let sid = registry::stable("session", json!([actor, o.key]));
    if let Some(existing) = db.find(&sid)? {
        require(
            existing.spec == *p,
            "IdempotencyConflict",
            "session key already bound",
        )?;
        return Ok(response(&existing));
    }
    let count:u32=db.c.query_row("SELECT count(*) FROM current WHERE owner=?1 AND kind='Session' AND body->>'lifecycle' NOT IN ('Failed','Succeeded','Cancelled')",[actor],|r|r.get(0))?;
    require(
        count < 32,
        "QuotaExceeded",
        "active sessions per principal exceeded",
    )?;
    let session=db.create("Session",&sid,actor,p.clone(),json!({"lifecycle":"AwaitingModel","checkpoint_version":"1",
      "turn":0,"goal":o.goal,"skill_ref":o.skill_ref,"model_ref":o.model_ref,"context_policy":o.context_policy,
      "budget_ref":d.body["budget_ref"],"delegation_ref":d.reference(),"max_turns":o.max_turns,"last_task":null,"current_lease":null}))?;
    deps(db, &session)?;
    if let Some(r) = o.trigger_signal_ref {
        mail::consume(db, actor, &r, &session.id)?;
    }
    Ok(response(&session))
}
pub fn get(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: ById = decode(p)?;
    let s = db.of(&p.id, "Session")?;
    policy::owned(actor, &s)?;
    policy::can(db, actor, "task.read")?;
    Ok(response(&s))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Extend {
    id: String,
    expected_resource_version: String,
    max_turns: u32,
}
pub fn extend(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Extend = decode(p)?;
    let s = db.of(&p.id, "Session")?;
    policy::owned(actor, &s)?;
    deps(db, &s)?;
    s.cas(&p.expected_resource_version)?;
    require(
        p.max_turns <= 32 && u64::from(p.max_turns) > s.body["max_turns"].as_u64().unwrap(),
        "InvalidArgument",
        "invalid new turn limit",
    )?;
    require(
        matches!(state(&s), "AwaitingModel" | "ModelInFlight" | "WaitingTask")
            || s.body["pause_reason"] == "ModelTurnLimit",
        "PreconditionFailed",
        "session cannot be extended",
    )?;
    let mut b = s.body.clone();
    b["max_turns"] = json!(p.max_turns);
    if state(&s) == "Paused"
        && s.body["pause_reason"] == "ModelTurnLimit"
        && s.body["outcome_knowledge"] == "Known"
    {
        b["lifecycle"] = json!("AwaitingModel");
        b.as_object_mut().unwrap().remove("pause_reason");
    }
    Ok(response(&db.status(&s, b)?))
}
pub fn history(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: ById = decode(p)?;
    let s = db.of(&p.id, "Session")?;
    policy::owned(actor, &s)?;
    policy::can(db, actor, "task.read")?;
    let turns=db.c.prepare("SELECT turn,lease,expires,task,receipt FROM session_turns WHERE session=?1 ORDER BY turn")?
        .query_map([&s.id],|r| Ok(json!({"turn":r.get::<_,u32>(0)?,"lease_id":r.get::<_,String>(1)?,"expires_at":r.get::<_,String>(2)?,
          "task_id":r.get::<_,Option<String>>(3)?,"decision":r.get::<_,Option<Value>>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let tasks=db.c.prepare("SELECT task,turn,attempt FROM session_tasks WHERE session=?1 ORDER BY turn,attempt")?
       .query_map([&s.id],|r|Ok(json!({"task_id":r.get::<_,String>(0)?,"turn":r.get::<_,u32>(1)?,"attempt":r.get::<_,u32>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"session_ref":s.reference(),"turns":turns,"tasks":tasks}))
}
pub fn cancel(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Conditional = decode(p)?;
    let s = db.of(&p.id, "Session")?;
    policy::owned(actor, &s)?;
    // Closing an owner's cancellation scope remains available after the delegation is withdrawn.
    s.cas(&p.expected_resource_version)?;
    if matches!(state(&s), "Succeeded" | "Failed" | "Cancelled") {
        return Ok(response(&s));
    }
    let mut b = s.body.clone();
    b["lifecycle"] = json!("Cancelled");
    b["current_lease"] = Value::Null;
    let cancelled = db.status(&s, b)?;
    if let Some(task) = s.body["last_task"].as_str() {
        broker::cancel_task(db, &db.of(task, "Task")?)?;
    }
    Ok(response(&db.get(&cancelled.id)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    session_id: String,
    key: String,
}
fn provider(db: &Db<'_>, s: &Doc, actor: &str) -> Result<(Doc, Doc, Doc)> {
    policy::can(db, actor, "model.serve")?;
    let m = db.resolve(&s.spec["model_ref"])?;
    require(
        m.spec["provider_principal_id"] == actor,
        "PermissionDenied",
        "session bound to another provider",
    )?;
    deps(db, s)
}
fn context(db: &Db<'_>, s: &Doc, skill: &Doc, model: &Doc) -> Result<Value> {
    let mut last = Value::Null;
    if let Some(tid) = s.body["last_task"].as_str() {
        let t = db.of(tid, "Task")?;
        require(
            t.owner == s.owner,
            "PermissionDenied",
            "tool result owner changed",
        )?;
        last = json!({"task_id":tid,"tool":t.spec["task_type"],"lifecycle":t.body["lifecycle"],"result":t.body["result"]});
        if s.spec["context_policy"] == "LocalToolResults"
            && let Some(reference) = t.body["result"]["artifact_refs"].get(0)
        {
            let op = db.resolve(&t.body["operation_refs"][0])?;
            let plan = db.resolve(&op.body["plan_ref"])?;
            policy::tool_scope(
                db,
                &t.spec["delegation_ref"],
                plan.body["allowed_effects"][0]["resource_ref"]["resource_id"]
                    .as_str()
                    .unwrap(),
            )?;
            let artifact = db.resolve(reference)?;
            require(
                artifact.owner == s.owner && artifact.kind == "Artifact",
                "PermissionDenied",
                "foreign tool result",
            )?;
            last["local_tool_result"] = artifact.body;
        }
    }
    let v = json!({"session_id":s.id,"goal":s.spec["goal"],"skill_ref":s.spec["skill_ref"],"model_ref":s.spec["model_ref"],
        "instructions":skill.spec["instructions"],"permitted_tools":skill.spec["tools"],"last_tool":last,"context_policy":s.spec["context_policy"]});
    require(
        canonical(&v).len() as u64 <= model.spec["max_context_bytes"].as_u64().unwrap(),
        "QuotaExceeded",
        "model context exceeds pinned provider limit",
    )?;
    Ok(v)
}
pub fn claim(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Claim = decode(p)?;
    label(&p.key)?;
    let s = db.of(&p.session_id, "Session")?;
    let (d, skill, m) = provider(db, &s, actor)?;
    let previous: Option<(String, String, u32, Option<Value>)> =
        db.c.query_row(
            "SELECT lease,expires,turn,receipt FROM session_turns WHERE session=?1 AND claim_key=?2",
            params![s.id, p.key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    if let Some((_, _, turn, Some(receipt))) = &previous {
        return Ok(json!({"turn":turn,"completed":receipt}));
    }
    let clock = lease::BootClock::sample()?;
    let ctx = context(db, &s, &skill, &m)?;
    let lease_info = |lease: &str, expires: &str, turn: u32, recovered: bool| json!({"lease_id":lease,"expires_at":expires,"turn":turn,"context":ctx,"recovered":recovered,"usage_format":"tokens.v1"});
    if let Some((lease, until, turn, _)) = previous {
        require(
            state(&s) == "ModelInFlight"
                && s.body["current_lease"] == lease
                && clock.current(db, &s)?,
            "Fenced",
            "model lease no longer current",
        )?;
        return Ok(lease_info(&lease, &until, turn, true));
    }
    require(
        state(&s) == "AwaitingModel" || (state(&s) == "ModelInFlight" && !clock.current(db, &s)?),
        "Conflict",
        "session already leased, waiting for a task or closed",
    )?;
    db.cfg.check_admission_space()?;
    let active = lease::model_slots(db, &clock, &m.id)?;
    require(
        u64::from(active) < m.spec["max_in_flight"].as_u64().unwrap_or(4),
        "QueueFull",
        "model provider concurrency exhausted",
    )?;
    let turn = s.body["turn"].as_u64().unwrap() as u32 + 1;
    require(
        u64::from(turn) <= s.body["max_turns"].as_u64().unwrap(),
        "QuotaExceeded",
        "session turn limit reached",
    )?;
    require(
        db.c.execute(
            "UPDATE model_budgets SET used=used+1 WHERE budget=?1 AND used<maximum",
            [d.body["budget_ref"]["resource_id"].as_str().unwrap()],
        )? == 1,
        "QuotaExceeded",
        "root model call budget exhausted",
    )?;
    let lease = id("model-call");
    let seconds = m.spec["lease_seconds"].as_i64().unwrap();
    let until = after(seconds);
    db.c.execute(
        "INSERT INTO session_turns(session,turn,lease,claim_key,expires,boot_id,deadline_boot_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            s.id,
            turn,
            lease,
            p.key,
            until,
            clock.boot_id,
            clock.millis + seconds * 1000
        ],
    )?;
    let mut b = s.body.clone();
    b["lifecycle"] = json!("ModelInFlight");
    b["turn"] = json!(turn);
    b["current_lease"] = json!(lease);
    b["lease_until"] = json!(until);
    let usage = db.record("ModelUsage", &s.owner, json!({
        "session_id": s.id, "call_id": lease, "model_ref": m.reference(), "budget_ref": d.body["budget_ref"],
        "input_bytes": canonical(&ctx).len(), "reserved_calls": 1, "issued_at": now()
    }))?;
    b["current_usage_ref"] = usage.reference();
    db.status(&s, b)?;
    Ok(lease_info(&lease, &until, turn, false))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Respond {
    session_id: String,
    lease_id: String,
    decision: Value,
    usage: Option<ModelTokenUsage>,
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Decision {
    Tool { tool: String, input: Value },
    Finish { summary: String },
    Fail { reason: String },
}
// Older format-5 calls already have a reservation, but its reference was not
// included in their session checkpoint. Accept only an unambiguous match.
fn settle_usage(
    db: &Db<'_>,
    s: &Doc,
    model: &Doc,
    actor: &str,
    call: &str,
    tokens: ModelTokenUsage,
) -> Result<Value> {
    let usage = if let Some(reference) = s.body.get("current_usage_ref") {
        db.resolve(reference)?
    } else {
        let ids =
            db.c.prepare(
                "SELECT id FROM current WHERE kind='ModelUsage' AND owner=?1
            AND body->>'session_id'=?2 AND body->>'call_id'=?3 LIMIT 2",
            )?
            .query_map(params![s.owner, s.id, call], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        require(
            ids.len() == 1,
            "RecoveryRequired",
            "original model reservation unavailable",
        )?;
        db.get(&ids[0])?
    };
    require(
        usage.kind == "ModelUsage"
            && usage.owner == s.owner
            && usage.body["call_id"] == call
            && usage.body["reserved_calls"] == 1
            && usage
                .body
                .get("budget_ref")
                .is_none_or(|r| *r == s.body["budget_ref"])
            && usage.body["session_id"] == s.id
            && usage.body["model_ref"] == model.reference()
            && usage.body.get("reported_tokens").is_none(),
        "RecoveryRequired",
        "model reservation mismatch",
    )?;
    let mut body = usage.body.clone();
    body["budget_ref"] = s.body["budget_ref"].clone();
    body["reported_tokens"] = json!(tokens);
    body["measurement_source"] = json!("ProviderReported");
    body["provider_principal_id"] = json!(actor);
    body["reported_at"] = json!(now());
    Ok(db.status(&usage, body)?.reference())
}

pub fn respond(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Respond = decode(p)?;
    let s = db.of(&p.session_id, "Session")?;
    let (_, skill, model) = provider(db, &s, actor)?;
    let bytes = canonical(&p.decision);
    require(
        bytes.len() as u64 <= model.spec["max_output_bytes"].as_u64().unwrap(),
        "QuotaExceeded",
        "provider output exceeds registered bound",
    )?;
    let hash = sha256(&bytes);
    let prior:Option<(String,Value)>=db.c.query_row("SELECT decision_digest,receipt FROM session_turns WHERE lease=?1 AND session=?2 AND receipt IS NOT NULL",
        params![p.lease_id,s.id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((old, receipt)) = prior {
        let previous_usage = receipt
            .get("usage_ref")
            .map(|r| db.resolve(r))
            .transpose()?;
        let prior_tokens = previous_usage
            .as_ref()
            .and_then(|d| d.body.get("reported_tokens"));
        let supplied = p.usage.map(|u| json!(u));
        require(
            old == hash && prior_tokens == supplied.as_ref(),
            "IdempotencyConflict",
            "lease already bound to another decision",
        )?;
        return Ok(receipt);
    }
    require(
        state(&s) == "ModelInFlight"
            && s.body["current_lease"] == p.lease_id
            && lease::BootClock::sample()?.current(db, &s)?,
        "Fenced",
        "model response outside current lease",
    )?;
    let mut b = s.body.clone();
    let mut task = None;
    match decode::<Decision>(&p.decision)? {
        Decision::Tool { tool, input } => {
            require(
                skill.spec["tool_refs"].get(&tool).is_some(),
                "PermissionDenied",
                "tool not declared by pinned skill",
            )?;
            let request = broker::prepare(
                db,
                &s.owner,
                &json!({"tool":tool,"input":input,"key":p.lease_id,"delegation_id":s.spec["delegation_id"]}),
            )?;
            let receipt = broker::submit(db, &s.owner, &request, None)?;
            task = receipt["task_id"].as_str().map(str::to_owned);
            b["last_task"] = receipt["task_id"].clone();
            b["lifecycle"] = json!("WaitingTask");
        }
        Decision::Finish { summary } => {
            require(!summary.is_empty(), "InvalidArgument", "empty final report")?;
            b["lifecycle"] = json!("Succeeded");
            b["summary"] = json!(summary);
        }
        Decision::Fail { reason } => {
            require(
                !reason.is_empty(),
                "InvalidArgument",
                "empty failure reason",
            )?;
            b["lifecycle"] = json!("Failed");
            b["reason"] = json!(reason);
        }
    }
    let usage_ref = p
        .usage
        .map(|tokens| settle_usage(db, &s, &model, actor, &p.lease_id, tokens))
        .transpose()?;
    b["current_lease"] = Value::Null;
    b.as_object_mut().unwrap().remove("current_usage_ref");
    let saved = db.status(&s, b)?;
    let mut receipt = json!({"session_ref":saved.reference(),"task_id":task,"decision_digest":hash,"output_bytes":bytes.len(),"lifecycle":state(&saved)});
    if let Some(reference) = usage_ref {
        receipt["usage_ref"] = reference;
    }
    db.c.execute("UPDATE session_turns SET task=?1,decision_digest=?2,receipt=?3 WHERE session=?4 AND lease=?5",
      params![task,hash,receipt,s.id,p.lease_id])?;
    if let Some(task) = &task {
        db.c.execute(
            "INSERT INTO session_tasks VALUES(?1,?2,?3,1)",
            params![task, s.id, s.body["turn"].as_i64()],
        )?;
    }
    Ok(receipt)
}
pub fn task_closed(db: &Db<'_>, t: &Doc) -> Result<()> {
    if let Some(sid) = session_for_task(db, &t.id)? {
        let s = db.of(&sid, "Session")?;
        if s.body["last_task"] != t.id {
            return Ok(());
        }
        let mut b = s.body.clone();
        b["task_ref"] = t.reference();
        b["result"] = t.body["result"].clone();
        b["outcome_knowledge"] = t.body["outcome_knowledge"].clone();
        if state(&s) != "Cancelled" {
            b["lifecycle"] = json!(if t.body["outcome_knowledge"] == "Unknown" {
                "Paused"
            } else if t.body["lifecycle"] == "Succeeded" && s.body["turn"] == s.body["max_turns"] {
                b["pause_reason"] = json!("ModelTurnLimit");
                "Paused"
            } else if t.body["lifecycle"] == "Succeeded" {
                "AwaitingModel"
            } else if t.body["lifecycle"] == "Cancelled" {
                "Cancelled"
            } else {
                "Failed"
            });
        }
        db.status(&s, b)?;
    }
    Ok(())
}
pub(crate) fn session_for_task(db: &Db<'_>, tid: &str) -> Result<Option<String>> {
    Ok(db
        .c
        .query_row(
            "SELECT session FROM session_tasks WHERE task=?1",
            [tid],
            |r| r.get(0),
        )
        .optional()?)
}
pub fn snapshot(db: &Db<'_>, t: &Doc, plan: &Doc) -> Result<Value> {
    let mut b = json!({"runtime":"psios.local.task.v2","checkpoint_version":"1","input_ref":t.spec["input_ref"],
        "plan_ref":plan.reference(),"plan_digest":plan.body["plan_digest"],"tool_refs":plan.body["runtime_refs"],
        "preconditions":plan.body["preconditions"],"delegation_ref":t.spec["delegation_ref"],"budget_ref":t.spec["budget_ref"],
        "cancel_scope_id":t.spec["cancel_scope_id"],"authorization_epoch":db.meta("epoch")?,"recovery_incarnation":db.incarnation});
    if let Some(sid) = session_for_task(db, &t.id)? {
        let s = db.of(&sid, "Session")?;
        // The broker has already validated this checkpoint in preflight.
        b["session_ref"] = s.reference();
        b["skill_ref"] = s.spec["skill_ref"].clone();
        b["model_ref"] = s.spec["model_ref"].clone();
        b["turn"] = s.body["turn"].clone();
    }
    Ok(db.record("ExecutionSnapshot", &t.owner, b)?.reference())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryTask {
    id: String,
    expected_resource_version: String,
    task_id: String,
    key: String,
}
pub fn retry_task(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let req: RetryTask = decode(p)?;
    label(&req.key)?;
    let s = db.of(&req.id, "Session")?;
    require(
        s.owner == actor,
        "PermissionDenied",
        "only session principal may retry its task",
    )?;
    deps(db, &s)?;
    let previous = db.of(&req.task_id, "Task")?;
    require(
        session_for_task(db, &previous.id)?.as_deref() == Some(&s.id),
        "PermissionDenied",
        "retry target belongs to another session",
    )?;
    // Receipt replay may arrive after the retried child finished. Bind it to this parent and key.
    let prior: Option<String> =
        db.c.query_row(
            "SELECT j.task FROM session_tasks j JOIN current t ON t.id=j.task
        WHERE j.session=?1 AND t.body->>'retry_of_task_id'=?2 AND t.spec->>'idempotency_key'=?3",
            params![s.id, previous.id, req.key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = prior {
        return Ok(json!({"task_id":id,"deduplicated":true}));
    }
    s.cas(&req.expected_resource_version)?;
    require(
        state(&s) == "Failed" && s.body["last_task"] == req.task_id,
        "PreconditionFailed",
        "session has no failed current task",
    )?;
    let (turn, attempt): (u32, u32) = db.c.query_row(
        "SELECT turn,attempt FROM session_tasks WHERE task=?1",
        [&previous.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    require(
        attempt < 3,
        "QuotaExceeded",
        "turn tool retry limit exhausted",
    )?;
    let accepted = broker::retry_in_session(
        db,
        actor,
        &json!({"id":previous.id,"expected_resource_version":previous.version(),"key":req.key}),
    )?;
    let task = accepted["task_id"].as_str().unwrap();
    db.c.execute(
        "INSERT INTO session_tasks VALUES(?1,?2,?3,?4)",
        params![task, s.id, turn, attempt + 1],
    )?;
    let mut b = s.body.clone();
    b["last_task"] = json!(task);
    b["lifecycle"] = json!("WaitingTask");
    b["result"] = Value::Null;
    b["task_ref"] = Value::Null;
    b["outcome_knowledge"] = json!("NotStarted");
    db.status(&s, b)?;
    Ok(json!({"task_id":task,"deduplicated":false}))
}
pub fn check_dispatch(db: &Db<'_>, t: &Doc) -> Result<()> {
    if let Some(sid) = session_for_task(db, &t.id)? {
        let s = db.of(&sid, "Session")?;
        deps(db, &s)?;
        require(
            state(&s) == "WaitingTask" && s.body["last_task"] == t.id,
            "Fenced",
            "session closed before dispatch",
        )?;
    }
    Ok(())
}
