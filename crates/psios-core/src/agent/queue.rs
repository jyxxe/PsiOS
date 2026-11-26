use super::*;
use lease::Clock;

const MAX_SESSIONS_PER_MODEL: u32 = 128;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimNext {
    model_ref: Value,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimStatus {
    model_ref: Value,
    key: String,
    lease_id: String,
}

pub(super) fn check_capacity(db: &Db<'_>, model: &str) -> Result<()> {
    let active: u32 = db.c.query_row(
        "SELECT count(*) FROM model_queue WHERE model=?1",
        [model],
        |r| r.get(0),
    )?;
    require(
        active < MAX_SESSIONS_PER_MODEL,
        "QueueFull",
        "active sessions for this model reached the provider queue limit",
    )
}

fn pause(db: &Db<'_>, session: &Doc, reason: &str) -> Result<()> {
    let mut body = session.body.clone();
    body["lifecycle"] = json!("Paused");
    body["pause_reason"] = json!(reason);
    // A model lease without a committed decision did not dispatch a new tool.
    body["outcome_knowledge"] = json!("Known");
    body["current_lease"] = Value::Null;
    db.status(session, body)?;
    Ok(())
}

fn reply(session: &str, mut result: Value) -> Value {
    result["available"] = json!(true);
    result["session_id"] = json!(session);
    result
}

fn bound_model(db: &Db<'_>, actor: &str, reference: &Value) -> Result<Doc> {
    policy::can(db, actor, "model.serve")?;
    let model = db.resolve(reference)?;
    require(
        model.kind == "Model" && model.spec["provider_principal_id"] == actor,
        "PermissionDenied",
        "model is bound to another provider",
    )?;
    policy::can_target(db, actor, "model.serve", &model.id)?;
    Ok(model)
}

fn queue_key(key: &str) -> Result<String> {
    label(key)?;
    // Explicit Session claims cannot use this namespace. The model and the
    // provider-supplied key bind to one turn even after the runnable set changes.
    Ok(format!("queue\n{key}"))
}

pub fn claim_status(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: ClaimStatus = decode(request)?;
    let key = queue_key(&p.key)?;
    label(&p.lease_id)?;
    let model = bound_model(db, actor, &p.model_ref)?;
    let (id, settled): (String, bool) =
        db.c.query_row(
            "SELECT t.session, t.receipt IS NOT NULL OR i.receipt IS NOT NULL FROM session_turns t
         LEFT JOIN model_inference_leases i USING(lease)
         WHERE t.queue_model=?1 AND t.claim_key=?2 AND t.lease=?3",
            params![model.id, key, p.lease_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| err("NotFound", "queued model lease not found"))?;
    // A provider recovering its own delivery learns only the disposition of
    // the exact lease. Task receipts and model context still require live
    // session authorization through the ordinary paths.
    let status = if settled {
        "Settled"
    } else {
        let session = db.of(&id, "Session")?;
        if state(&session) == "ModelInFlight"
            && session.spec["model_ref"] == model.reference()
            && session.body["current_lease"] == p.lease_id
            && Clock::sample()?.current(db, &session)?
            && provider_authority::current(db, &session, &model, &p.lease_id)?
        {
            "Pending"
        } else {
            "Fenced"
        }
    };
    Ok(json!({"status":status}))
}

pub fn claim_next(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: ClaimNext = decode(request)?;
    let key = queue_key(&p.key)?;
    let model = bound_model(db, actor, &p.model_ref)?;
    require(
        model.body["lifecycle"] == "Active" && db.get(&model.id)?.reference() == p.model_ref,
        "PreconditionFailed",
        "execution dependency changed or retired",
    )?;
    let prior: Option<String> =
        db.c.query_row(
            "SELECT session FROM session_turns WHERE queue_model=?1 AND claim_key=?2",
            params![model.id, key],
            |r| r.get(0),
        )
        .optional()?;
    let clock = Clock::sample()?;
    if let Some(id) = prior {
        let session = db.of(&id, "Session")?;
        let (_, skill, model) = provider(db, &session, actor)?;
        let expired = inference::expire_addressed(db, &session, &clock)?;
        let replay = replay_claim(
            db,
            expired.as_ref().unwrap_or(&session),
            &skill,
            &model,
            &clock,
            &key,
            false,
        )?
        .ok_or_else(|| err("RecoveryRequired", "provider claim index has no turn"))?;
        return Ok(reply(&id, replay));
    }

    inference::expire_queued(db, &clock, &model)?;
    if work_admission::draining(db)? {
        return Ok(json!({"available":false}));
    }
    let runnable =
        db.c.prepare(
            "SELECT q.session FROM model_queue q
         LEFT JOIN session_turns t ON t.lease=q.lease
         LEFT JOIN model_lease_times a ON a.lease=t.lease
         WHERE q.model=?1 AND q.ready_seq IS NOT NULL
           AND (q.lease IS NULL OR
                (NOT ?5 AND t.lease IS NOT NULL AND (t.boot_id IS NOT ?2 OR t.deadline_boot_ms<=?3 OR
                    (a.lease IS NOT NULL AND a.time_ns!=?4))))
         ORDER BY q.ready_seq",
        )?
        .query_map(
            params![
                model.id,
                clock.boot.boot_id,
                clock.boot.millis,
                clock.time_ns,
                inference::strict(&model),
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !runnable.is_empty() && model_has_slot(db, &clock, &model)? {
        db.cfg.check_admission_space()?;
        for id in runnable {
            let session = db.of(&id, "Session")?;
            let dependencies = match deps(db, &session) {
                Ok(value) => value,
                Err(e) if e.0.code == "PermissionDenied" => {
                    pause(db, &session, "ModelAuthorizationDenied")?;
                    continue;
                }
                Err(e)
                    if matches!(
                        e.0.code.as_str(),
                        "PreconditionFailed" | "SchemaVersionUnsupported"
                    ) =>
                {
                    pause(db, &session, "ModelDependencyChanged")?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if !has_turn(&session) {
                pause(db, &session, "ModelTurnLimit")?;
                continue;
            }
            let (_, skill, selected) = &dependencies;
            let context = match context(db, &session, skill, selected) {
                Ok(value) => value,
                Err(e) if artifact::is_content_unavailable(&e.0.code) => {
                    pause(db, &session, &e.0.code)?;
                    continue;
                }
                Err(e) if e.0.code == "QuotaExceeded" => {
                    pause(db, &session, "ModelContextLimit")?;
                    continue;
                }
                Err(e) if e.0.code == "PermissionDenied" => {
                    pause(db, &session, "ModelAuthorizationDenied")?;
                    continue;
                }
                Err(e)
                    if tool_context::enabled(&session)
                        && matches!(
                            e.0.code.as_str(),
                            "InvalidArgument" | "PreconditionFailed" | "RecoveryRequired"
                        ) =>
                {
                    pause(db, &session, "ModelContextCheckpointInvalid")?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            match allocate_turn(db, &session, &dependencies, &clock, &key, true, context)? {
                Ok(result) => {
                    db.audit(actor, "Model.ClaimNext", Some(&id), "Allowed")?;
                    return Ok(reply(&id, result));
                }
                Err(refusal) => {
                    if let Some(reason) = refusal.pause_reason() {
                        pause(db, &session, reason)?;
                    }
                }
            }
        }
    }
    Ok(json!({"available":false}))
}
