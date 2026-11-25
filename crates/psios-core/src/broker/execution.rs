use super::*;

fn plan_audience(plan: &Doc) -> Vec<Value> {
    plan.body["preconditions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["resource_ref"].clone())
        .collect()
}
pub(super) fn preflight(db: &Db<'_>, t: &Doc) -> Result<(Doc, Doc, Option<Vec<Value>>)> {
    agent::check_dispatch(db, t)?;
    let (_, input) = authority(db, &t.owner, &t.spec)?;
    let op = operation(db, t)?;
    let p = db.resolve(&op.body["plan_ref"])?;
    require(
        !policy::deadline::Clock::sample()?.expired(db, &t.id, field(&p.body, "deadline_at"))?,
        "DeadlineExceeded",
        "operation deadline passed",
    )?;
    let a = Action::parse(
        field(&t.spec, "task_type"),
        dependencies::tool_input(&input)?,
    )?;
    let target = a.target(db, &t.owner)?;
    let audience = a.audience(db, &t.owner, &t.spec["delegation_ref"], &target)?;
    require(
        target.reference() == p.body["allowed_effects"][0]["resource_ref"]
            && audience == plan_audience(&p),
        "ApprovalInvalidated",
        "execution precondition changed",
    )?;
    let outputs = dependencies::ready(db, &t.owner, &dependencies::requirements(&input)?)?;
    Ok((op, p, outputs))
}
pub fn claim(db: &Db<'_>) -> Result<Option<String>> {
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue WHERE phase IN ('Queued','WaitingApproval','WaitingResource'))",
        [],
        |r| r.get(0),
    )?;
    if !pending {
        return Ok(None);
    }
    let clock = policy::deadline::Clock::sample()?;
    let invalid:Option<(String,bool)>=db.c.query_row("WITH RECURSIVE lineage(task,did,depth) AS (
        SELECT q.task,t.spec->'delegation_ref'->>'resource_id',0 FROM queue q JOIN current t ON t.id=q.task
        WHERE q.phase IN ('Queued','WaitingApproval','WaitingResource') UNION ALL
        SELECT l.task,d.body->'parent_ref'->>'resource_id',l.depth+1 FROM lineage l JOIN current d ON d.id=l.did
        WHERE l.depth<4 AND d.body->'parent_ref' IS NOT NULL),
        checked AS (
          SELECT q.task,
            julianday(q.valid_until)<=julianday('now') OR EXISTS(
              SELECT 1 FROM authorization_deadlines elapsed
              WHERE elapsed.resource IN (q.task,q.grant_id,l.did) AND elapsed.boot_id=?1
                AND (elapsed.time_ns!=?2 OR elapsed.deadline_boot_ms<=?3)
            ) AS expired,
            g.body->>'lifecycle'!='Active' OR d.body->>'lifecycle'!='Active'
              OR p.body->>'lifecycle'!='Active' AS revoked
          FROM lineage l JOIN queue q ON q.task=l.task JOIN current d ON d.id=l.did
            JOIN current p ON p.id=d.owner JOIN current g ON g.id=q.grant_id
        )
        SELECT task,expired FROM checked WHERE expired OR revoked LIMIT 1",
        params![clock.boot.boot_id,clock.time_ns,clock.boot.millis],
        |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, expired)) = invalid {
        close(
            db,
            &db.get(&id)?,
            "Failed",
            "NotStarted",
            Some(&err(
                if expired {
                    "DeadlineExceeded"
                } else {
                    "PermissionDenied"
                },
                "queued admission no longer authorized",
            )),
            None,
        )?;
    }
    if !node::enabled(db)? {
        return Ok(None);
    }
    dependencies::advance(db)?;
    let next: Option<String> =
        db.c.query_row(
            "SELECT q.task FROM queue q JOIN current t ON t.id=q.task
             LEFT JOIN dispatch_fairness f ON f.principal=t.owner
             LEFT JOIN task_concurrency c ON c.budget=q.budget
             WHERE q.phase='Queued' AND
               (c.maximum IS NULL OR (SELECT count(*) FROM occupied_task_slots WHERE budget=q.budget)<c.maximum)
             ORDER BY coalesce(f.turn,0),q.stamp,q.task LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(next) = next else { return Ok(None) };
    let t = db.of(&next, "Task")?;
    let (op, plan, outputs) = match preflight(db, &t).and_then(|(op, plan, outputs)| {
        let outputs = outputs.ok_or_else(|| {
            err(
                "ResultUnavailable",
                "required task outputs became unavailable before claim",
            )
        })?;
        Ok((op, plan, outputs))
    }) {
        Ok(v) => v,
        Err(e) => {
            close(db, &t, "Failed", "NotStarted", Some(&e), None)?;
            return Ok(None);
        }
    };
    db.c.execute("INSERT INTO dispatch_fairness(principal,turn) VALUES(?1,(SELECT coalesce(max(turn),0)+1 FROM dispatch_fairness))
        ON CONFLICT(principal) DO UPDATE SET turn=excluded.turn", [&t.owner])?;
    let epoch: u32 = db.c.query_row(
        "UPDATE queue SET attempt=attempt+1,phase='Claimed' WHERE task=?1 RETURNING attempt",
        [&t.id],
        |r| r.get(0),
    )?;
    let run = id("run");
    let r = db.create(
        "Run",
        &run,
        &t.owner,
        json!({"agent_principal_id": t.owner}),
        json!({
            "schema_version": VERSION,
            "run_id": run,
            "task_id": t.id,
            "claim_id": id("claim"),
            "claim_epoch": epoch.to_string(),
            "recovery_incarnation": db.incarnation,
            "lifecycle": "Running",
            "outcome_knowledge": "NotStarted",
            "input_ref": t.spec["input_ref"],
            "execution_snapshot_ref": agent::snapshot(db,&t,&plan,&outputs)?,
            "output_refs": [],
            "operation_refs": [op.reference()],
            "conditions": []
        }),
    )?;
    let mut b = t.body.clone();
    b["lifecycle"] = json!("Running");
    b["current_run_id"] = json!(r.id);
    b["conditions"] = json!([]);
    db.status(&t, b)?;
    Ok(Some(next))
}
pub fn dispatch(db: &Db<'_>, task: &str) -> Result<Option<Work>> {
    let t = db.of(task, "Task")?;
    let phase: String =
        db.c.query_row("SELECT phase FROM queue WHERE task=?1", [task], |r| {
            r.get(0)
        })?;
    if phase != "Claimed" {
        return Ok(None);
    }
    let (op, p, outputs) = preflight(db, &t)?;
    require(
        outputs.is_some(),
        "ResultUnavailable",
        "required task outputs became unavailable before dispatch",
    )?;
    let run = db.get(field(&t.body, "current_run_id"))?;
    let tool = field(&t.spec, "task_type");
    let target = &p.body["allowed_effects"][0]["resource_ref"];
    let decision: Option<String> = if policy::approval(tool) {
        let (d, bound): (Option<String>, Option<String>) = db.c.query_row(
            "SELECT decision,operation FROM approval_bindings WHERE task=?1",
            [task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let did = d.ok_or_else(|| err("ApprovalRequired", "approval missing"))?;
        let d = db.of(&did, "ApprovalDecision")?;
        require(
            d.body["decision"] == "Approved"
                && p.body["plan_digest"] == d.body["plan_digest"]
                && !expired(field(&d.body, "valid_until"))?,
            "ApprovalInvalidated",
            "decision no longer valid",
        )?;
        if let Some(bound) = bound {
            require(
                bound == op.id,
                "ApprovalAlreadyBound",
                "decision bound to different operation",
            )?
        } else {
            db.c.execute(
                "UPDATE approval_bindings SET operation=?1 WHERE task=?2 AND operation IS NULL",
                params![op.id, task],
            )?;
            let unit = db.of(&format!("use-{did}"), "ApprovalUse")?;
            require(
                unit.body["binding_status"] == "Unbound" && unit.body["validity_status"] == "Valid",
                "ApprovalAlreadyBound",
                "approval unit already used",
            )?;
            let mut b = unit.body.clone();
            b["binding_status"] = json!("Bound");
            b["operation_id"] = json!(op.id);
            b["bound_at"] = json!(now());
            db.status(&unit, b)?;
        }
        Some(did)
    } else {
        None
    };
    let eid = id("effect");
    let pid = id("permit");
    let proof = db.record(
        "PermitBinding",
        &t.owner,
        json!({
            "operation_id": op.id,
            "effect_id": eid,
            "permit_id": pid,
            "audience_resource_refs":plan_audience(&p),
            "claim_id": run.body["claim_id"],
            "claim_epoch": run.body["claim_epoch"],
            "plan_digest": p.body["plan_digest"]
        }),
    )?;
    let grant = db.of(
        field(&db.resolve(&t.spec["delegation_ref"])?.body, "grant_id"),
        "Grant",
    )?;
    let effect_seconds = match tool {
        "backup.create" => i64::from(db.cfg.backup_capture_timeout_seconds),
        "backup.restore" => storage::restore_publication::PERMIT_MAX_SECONDS,
        _ => 30,
    };
    let validity = [
        after(effect_seconds),
        field(&grant.spec, "valid_until").into(),
        field(&p.body, "deadline_at").into(),
    ]
    .into_iter()
    .map(|s| {
        chrono::DateTime::parse_from_rfc3339(&s)
            .map_err(|_| err("InvalidArgument", "invalid permit deadline"))
    })
    .collect::<Result<Vec<_>>>()?
    .into_iter()
    .min()
    .unwrap()
    .to_utc()
    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    policy::deadline::Clock::sample()?.register_until(db, &pid, &validity, Some(task))?;
    let mut permit = json!({
        "schema_version": VERSION,
        "permit_id": pid,
        "operation_id": op.id,
        "effect_id": eid,
        "plan_digest": p.body["plan_digest"],
        "audience_resource_refs": plan_audience(&p),
        "principal_chain_digest": sha256(&canonical(&t.spec["delegation_ref"])),
        "authorization_epoch": db.meta("epoch")?,
        "security_recovery_epoch": "1",
        "recovery_incarnation": db.incarnation,
        "authorization_ref": t.spec["delegation_ref"],
        "preconditions": p.body["preconditions"],
        "issued_at": now(),
        "not_after": validity,
        "usage": "SingleLogicalEffect",
        "auth_proof": {
            "mode": "AuthorityLookup",
            "authority_id": db.meta("authority")?,
            "record_ref": proof.reference()
        }
    });
    if let Some(id) = decision {
        permit["decision_ref"] = db.get(&id)?.reference()
    }
    let permit = db.create("ExecutionPermit", &pid, &t.owner, json!({}), permit)?;
    let effect = db.create(
        "Effect",
        &eid,
        &t.owner,
        json!({"permit_ref": permit.reference()}),
        json!({
            "schema_version": VERSION,
            "effect_id": eid,
            "operation_id": op.id,
            "resource_ref": target,
            "action": tool,
            "phase": "EffectStarted",
            "outcome_knowledge": "InFlight",
            "evidence_refs": []
        }),
    )?;
    let mut refs = op.body["effect_refs"].clone();
    refs.as_array_mut().unwrap().push(effect.reference());
    let op = op_update(db, &op, "Running", "InFlight", refs, json!([]))?;
    let mut b = run.body.clone();
    b["outcome_knowledge"] = json!("InFlight");
    b["operation_refs"] = json!([op.reference()]);
    let run = db.status(&run, b)?;
    let mut b = t.body.clone();
    b["outcome_knowledge"] = json!("InFlight");
    b["operation_refs"] = json!([op.reference()]);
    let task = db.status(&t, b)?;
    let work = host::work(db, task, run, op, effect)?;
    db.c.execute("UPDATE budgets SET reserved=reserved-1,spent=spent+1 WHERE id=(SELECT budget FROM queue WHERE task=?1)",[&t.id])?;
    db.c.execute("UPDATE queue SET phase='Dispatched' WHERE task=?1", [&t.id])?;
    db.audit(&t.owner, tool, Some(&work.op.id), "Dispatched")?;
    // Authority-only effects publish the admitted child and the parent Task
    // receipt together, before the worker can cross a host boundary.
    let receipt = match &work.action {
        Action::StartAgentSession(blueprint) => {
            Some(agent::open_for_task(db, &work.task, blueprint)?)
        }
        Action::StartWorkflow(blueprint) => Some(workflow::start_for_task(
            db,
            &work.task,
            blueprint,
            field(&p.body, "deadline_at"),
        )?),
        _ => None,
    };
    if let Some(receipt) = receipt {
        finish(db, &work, Ok(receipt))?;
        return Ok(None);
    }
    Ok(Some(work))
}
pub fn dispatch_failed(db: &Db<'_>, task: &str, e: &Error) -> Result<()> {
    let t = db.of(task, "Task")?;
    if t.body["lifecycle"] == "Running" && t.body["outcome_knowledge"] == "NotStarted" {
        close(db, &t, "Failed", "NotStarted", Some(e), None)?;
    }
    Ok(())
}

pub fn settlement_failed(db: &Db<'_>, w: &Work) -> Result<()> {
    let reason = if policy::readonly(field(&w.task.spec, "task_type")) {
        err("StorageUnavailable", "read result could not be committed")
    } else {
        err("OutcomeUnknown", "effect result could not be committed")
    };
    finish(db, w, Err(reason))
}

pub fn finish(db: &Db<'_>, w: &Work, result: Result<Value>) -> Result<()> {
    let t = db.get(&w.task.id)?;
    if t.body["current_run_id"] != w.run.id
        || t.body["outcome_knowledge"] != "InFlight"
        || w.run.body["recovery_incarnation"] != db.incarnation
    {
        let receipt = match result {
            Ok(value) => json!({"receipt": value}),
            Err(e) => json!({"code": e.0.code}),
        };
        db.record(
            "LateReceipt",
            &t.owner,
            json!({
                "run_ref": w.run.reference(),
                "operation_ref": w.op.reference(),
                "effect_ref": w.effect.reference(),
                "evidence": receipt
            }),
        )?;
        db.audit(&t.owner, "Run.LateReceipt", Some(&w.run.id), "Fenced")?;
        return Ok(());
    }
    let cancelled = t.body["lifecycle"] == "Cancelling";
    if cancelled && matches!(w.action, Action::Activate(_)) {
        close(db, &t, "Cancelled", "Known", None, None)?;
        return Ok(());
    }
    match result {
        Ok(receipt) => {
            let body = host::apply(db, w, &receipt)?;
            let a = artifact::publish(
                db,
                &t.owner,
                json!({
                    "task_id": t.id,
                    "run_id": w.run.id,
                    "operation_id": w.op.id,
                    "effect_id": w.effect.id
                }),
                &body,
            )?;
            close(
                db,
                &t,
                if cancelled { "Cancelled" } else { "Succeeded" },
                "Known",
                None,
                Some(&a),
            )?;
        }
        Err(e) => {
            let unknown = e.0.code == "OutcomeUnknown";
            close(
                db,
                &t,
                if cancelled {
                    "Cancelled"
                } else if unknown {
                    "Paused"
                } else {
                    "Failed"
                },
                if unknown { "Unknown" } else { "Known" },
                Some(&e),
                None,
            )?;
        }
    }
    Ok(())
}
