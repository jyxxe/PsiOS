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
    admission::check(db, &t.id, &a, &target)?;
    require(
        target.reference() == p.body["allowed_effects"][0]["resource_ref"]
            && audience == plan_audience(&p),
        "ApprovalInvalidated",
        "execution precondition changed",
    )?;
    let outputs = dependencies::ready(
        db,
        &t.owner,
        &dependencies::requirements(&input)?,
        Some(&t.id),
    )?;
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
        WHERE q.phase IN ('Queued','WaitingApproval','WaitingResource')
        UNION ALL
        SELECT q.task,input.read_delegation,0 FROM task_dependencies input JOIN queue q ON q.task=input.task
        WHERE input.read_delegation IS NOT NULL AND q.phase IN ('Queued','WaitingApproval','WaitingResource')
        UNION ALL
        SELECT l.task,d.body->'parent_ref'->>'resource_id',l.depth+1 FROM lineage l JOIN current d ON d.id=l.did
        WHERE l.depth<4 AND d.body->'parent_ref' IS NOT NULL),
        checked AS (
          SELECT q.task,
            julianday(q.valid_until)<=julianday('now')
              OR julianday(d.body->>'valid_until')<=julianday('now')
              OR julianday(g.spec->>'valid_until')<=julianday('now') OR EXISTS(
              SELECT 1 FROM authorization_deadlines elapsed
              WHERE elapsed.resource IN (q.task,g.id,l.did) AND elapsed.boot_id=?1
                AND (elapsed.time_ns!=?2 OR elapsed.deadline_boot_ms<=?3)
            ) AS expired,
            g.body->>'lifecycle'!='Active' OR d.body->>'lifecycle'!='Active'
              OR p.body->>'lifecycle'!='Active' AS revoked
          FROM lineage l JOIN queue q ON q.task=l.task JOIN current d ON d.id=l.did
            JOIN current p ON p.id=d.owner JOIN current g ON g.id=d.body->>'grant_id'
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
    let remaining_staging_bytes = u64::from(apps::capture::STAGING_BYTES)
        .saturating_sub(apps::capture::reserved_staging_bytes(db)?);
    // Use the pinned input revision for eligibility. Normal preflight and the
    // reservation check below still resolve the exact input and its digest.
    // Invalid limits go to normal preflight so an invalid task cannot wait forever.
    let next: Option<String> =
        db.c.query_row(
            "WITH candidates AS (
                SELECT q.task,q.stamp,coalesce(f.turn,0) AS turn,
                       CASE i.kind WHEN 'Input' THEN i.body
                            WHEN 'TaskInputManifest' THEN i.body->'tool_input' END AS capture_input
                FROM queue q JOIN current t ON t.id=q.task
                LEFT JOIN dispatch_fairness f ON f.principal=t.owner
                LEFT JOIN task_concurrency c ON c.budget=q.budget
                LEFT JOIN versions i ON t.spec->>'task_type'=?1
                     AND i.id=t.spec->'input_ref'->>'resource_id'
                     AND ('r'||i.rev)=t.spec->'input_ref'->>'version'
                WHERE q.phase='Queued' AND (c.maximum IS NULL OR
                    (SELECT count(*) FROM occupied_task_slots WHERE budget=q.budget)<c.maximum)
             ), sized AS (
                SELECT task,stamp,turn,
                       CASE WHEN json_type(capture_input)='object' THEN
                           CASE WHEN json_type(capture_input,'$.max_archive_bytes') IS NULL THEN ?2
                                WHEN json_type(capture_input,'$.max_archive_bytes')='integer'
                                THEN capture_input->>'max_archive_bytes' END
                       END AS bytes FROM candidates
             ) SELECT task FROM sized
               WHERE bytes IS NULL OR bytes<1 OR bytes>?3 OR bytes<=?4
               ORDER BY turn,stamp,task LIMIT 1",
            params![
                apps::capture::TOOL,
                storage::oci_archive::DEFAULT_CAPTURE_BYTES,
                db.cfg.oci_capture_max_bytes,
                remaining_staging_bytes as u32
            ],
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
    if t.spec["task_type"] == apps::capture::TOOL
        && u64::from(apps::capture::staging_bytes(db, &t)?) > remaining_staging_bytes
    {
        return Ok(None);
    }
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
    if t.spec["task_type"] == apps::capture::TOOL
        && apps::capture::reserved_staging_bytes(db)? > u64::from(apps::capture::STAGING_BYTES)
    {
        recovery::requeue(
            db,
            &t,
            &op,
            "NotStarted",
            json!([condition(
                "QuotaExceeded",
                "OCI capture staging capacity is occupied"
            )]),
        )?;
        return Ok(None);
    }
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
        "backup.create" | apps::volume_rebind::TOOL => {
            i64::from(db.cfg.backup_capture_timeout_seconds)
        }
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
            "phase": if node::service_control::is_tool(tool) || apps::stop::is_tool(tool) || tool == apps::restart::TOOL || tool == node::network::trial::TOOL || tool == apps::volume_rebind::TOOL { "PermitBound" } else { "EffectStarted" },
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
    // Authority-only effects publish their state and Task receipt together
    // before the worker can cross a host boundary.
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
        Action::PublishSessionSummary(input) => {
            Some(artifact::session_output::publish(db, &work, input)?)
        }
        Action::SubmitWorkItemEvidence(input) => Some(input.submit(db, &work.task.owner)?),
        Action::ListAssignedWork(input) => Some(input.list(db, &work.task.owner)?),
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
    let reason = if matches!(w.action, Action::CaptureOciImage(_)) {
        err(
            "StorageUnavailable",
            "OCI archive publication did not commit",
        )
    } else if matches!(w.action, Action::RestoreReleaseImage(_))
        && apps::restore_image::not_dispatched(db, &w.op)?
    {
        err(
            "StorageUnavailable",
            "OCI image restore stopped before load was checkpointed",
        )
    } else if matches!(w.action, Action::Stop(_) | Action::ContinueOciStop(_))
        && apps::stop::not_dispatched(db, &db.of(&w.op.id, "Operation")?)?
    {
        err(
            "StorageUnavailable",
            "OCI stop or cleanup did not reach its command checkpoint",
        )
    } else if matches!(w.action, Action::RestartApp(_))
        && apps::restart::not_dispatched(db, &db.of(&w.op.id, "Operation")?)?
    {
        err(
            "StorageUnavailable",
            "OCI restart did not reach its invocation checkpoint",
        )
    } else if node::service_control::is_control(&w.action)
        && node::service_control::not_dispatched(db, &db.of(&w.op.id, "Operation")?)?
    {
        err(
            "StorageUnavailable",
            "systemd control stopped before invocation was checkpointed",
        )
    } else if matches!(
        w.action,
        Action::InstallArtifact(_)
            | Action::StageArtifactRelease(_)
            | Action::RetireRelease(_)
            | Action::RebindRestoredVolume(_)
    ) {
        err(
            "StorageUnavailable",
            "application catalog transaction did not commit",
        )
    } else if policy::readonly(field(&w.task.spec, "task_type")) {
        err("StorageUnavailable", "read result could not be committed")
    } else {
        err("OutcomeUnknown", "effect result could not be committed")
    };
    finish(db, w, Err(reason))
}

pub fn finish(db: &Db<'_>, w: &Work, result: Result<Value>) -> Result<()> {
    finish_host(db, w, result.map(host::Output::Receipt))
}

pub(crate) fn finish_host(db: &Db<'_>, w: &Work, result: Result<host::Output>) -> Result<()> {
    if matches!(w.action, Action::NetworkTrial(_)) {
        return network_trial::finish_apply(db, w, result);
    }
    let atomic_authority = matches!(
        w.action,
        Action::StartAgentSession(_)
            | Action::SubmitWorkItemEvidence(_)
            | Action::ListAssignedWork(_)
    );
    let t = db.get(&w.task.id)?;
    if t.body["current_run_id"] != w.run.id
        || t.body["outcome_knowledge"] != "InFlight"
        || w.run.body["recovery_incarnation"] != db.incarnation
    {
        require(
            !atomic_authority,
            "Fenced",
            "authority task dispatch no longer owns its current run",
        )?;
        let receipt = match result {
            Ok(_)
                if matches!(
                    w.action,
                    Action::InspectProcess(_)
                        | Action::TrackBlockDevice(_)
                        | Action::ObserveBlockDevice(_)
                        | Action::ServiceLogs(_)
                        | Action::ServiceLogsSince(_)
                        | Action::ServiceResources(_)
                        | Action::ListAppReleases(_)
                        | Action::InstallArtifact(_)
                        | Action::StageArtifactRelease(_)
                        | Action::Logs(_)
                        | Action::Stop(_)
                        | Action::ObserveNetworkProbe(_)
                        | Action::InspectNetworkTrial(_)
                ) =>
            {
                json!({"code":"Fenced"})
            }
            Ok(host::Output::Receipt(value)) => json!({"receipt": value}),
            Ok(host::Output::OciImage(_)) => json!({"code":"Fenced"}),
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
    let rebind = matches!(w.action, Action::RebindRestoredVolume(_));
    require(
        !atomic_authority || t.body["lifecycle"] == "Running",
        "Fenced",
        "authority task dispatch is no longer running",
    )?;
    let service_started = if node::service_control::is_control(&w.action) {
        Some(node::service_control::worker_started(db, w, &t)?)
    } else {
        None
    };
    let stop_started = if matches!(w.action, Action::Stop(_) | Action::ContinueOciStop(_)) {
        Some(apps::stop::worker_started(db, w, &t)?)
    } else {
        None
    };
    let restart_started = if matches!(w.action, Action::RestartApp(_)) {
        Some(apps::restart::worker_started(db, w, &t)?)
    } else {
        None
    };
    if cancelled && stop_started == Some(false) {
        close(db, &t, "Cancelled", "NotStarted", None, None)?;
        return Ok(());
    }
    if cancelled && rebind {
        close(db, &t, "Cancelled", "NotStarted", None, None)?;
        return Ok(());
    }
    if cancelled && matches!(w.action, Action::Activate(_) | Action::CaptureOciImage(_)) {
        close(db, &t, "Cancelled", "Known", None, None)?;
        return Ok(());
    }
    let result = result.and_then(|output| {
        require(
            service_started != Some(false),
            "Fenced",
            "systemd result has no committed invocation",
        )?;
        require(restart_started != Some(false), "Fenced", "OCI restart result has no committed invocation")?;
        if stop_started == Some(false) {
            w.check_absent_stop_settlement(db)?;
        }
        require(
            stop_started != Some(false)
                || matches!((&w.action, &output), (Action::Stop(_), host::Output::Receipt(v)) if v["already_absent"] == true),
            "Fenced", "OCI stop result has no witnessed command or verified absence",
        )?;
        w.check_result_publication(db)?;
        match &output {
            host::Output::Receipt(receipt) => {
                if rebind {
                    apps::volume_rebind::validate_observation(db, w, receipt)?;
                }
                artifact::capacity::check_result(db, &t, receipt)?;
            }
            host::Output::OciImage(capture) => apps::capture::check_capacity(db, w, capture)?,
        }
        Ok(output)
    });
    match result {
        Ok(output) => {
            let body = match output {
                host::Output::Receipt(receipt) => host::apply(db, w, &receipt)?,
                host::Output::OciImage(capture) => apps::capture::publish(db, w, *capture)?,
            };
            if matches!(w.action, Action::ObserveNetworkProbe(_)) {
                artifact::capacity::check_result(db, &t, &body)?;
            }
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
            if matches!(w.action, Action::ObserveNetworkProbe(_)) {
                node::network::probe::state::bind_result(db, w, &body, &a)?;
            }
            artifact::capacity::retain(db, &t, &a)?;
            close(
                db,
                &t,
                if cancelled { "Cancelled" } else { "Succeeded" },
                "Known",
                None,
                Some(&a),
            )?;
        }
        // Authority-only tasks publish state and settle in the same transaction.
        Err(e) if atomic_authority => return Err(e),
        Err(e) => {
            let undispatched = service_started == Some(false)
                || stop_started == Some(false)
                || restart_started == Some(false)
                || rebind;
            let e = if stop_started == Some(true) && e.0.code != "OutcomeUnknown" {
                err(
                    "OutcomeUnknown",
                    "unable to settle the checkpointed OCI stop or removal",
                )
            } else if service_started == Some(true) && e.0.code != "OutcomeUnknown" {
                err(
                    "OutcomeUnknown",
                    "unable to settle the checkpointed systemd invocation",
                )
            } else if restart_started == Some(true) && e.0.code != "OutcomeUnknown" {
                err(
                    "OutcomeUnknown",
                    "unable to settle the checkpointed OCI restart",
                )
            } else {
                e
            };
            let unknown = !undispatched && e.0.code == "OutcomeUnknown";
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
                if unknown {
                    "Unknown"
                } else if undispatched {
                    "NotStarted"
                } else {
                    "Known"
                },
                Some(&e),
                None,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store, project};

    #[test]
    fn assigned_work_rechecks_the_project_grant_and_revision_after_claim() {
        let folder = tempfile::tempdir().unwrap();
        let cfg = Config::local(&folder.path().join("state"));
        Store::init(&cfg).unwrap();
        let mut store = Store::open(cfg).unwrap();
        let project = store
            .transaction("create-assigned-work-fixture", |db| {
                project::create_project(
                    db,
                    policy::OWNER,
                    &json!({"key":"assigned-work-fixture","title":"Post-claim authority"}),
                )
            })
            .unwrap();
        let initial = &project["reference"];
        let project_id = initial["resource_id"].as_str().unwrap();
        let project_grant = |db: &Db<'_>| {
            policy::grant(
                db,
                policy::OWNER,
                &json!({
                    "principal_id":policy::OWNER, "tool_targets":[project_id],
                    "actions":["project.read","project.manage","project.contribute"],
                    "units":1,"ttl_seconds":120,
                }),
            )
        };
        let (access, execution) = store
            .transaction("grant-assigned-work-fixture", |db| {
                let access = project_grant(db)?;
                let execution = policy::grant(
                    db,
                    policy::OWNER,
                    &json!({
                        "principal_id":policy::OWNER, "tool_targets":[project_id],
                        "actions":["task.submit","task.read",project::assigned::TOOL],
                        "units":2,"ttl_seconds":120,
                    }),
                )?;
                Ok((access, execution))
            })
            .unwrap();
        let enqueue =
            |store: &mut Store, version: &Value| {
                store.transaction("submit-assigned-work-fixture", |db| {
                let prepared = prepare(db, policy::OWNER, &json!({
                    "tool":project::assigned::TOOL,
                    "input":{"project_id":project_id,"expected_project_version":version},
                    "key":id("assigned-work-fixture"),
                    "delegation_id":execution["delegation_ref"]["resource_id"],
                }))?;
                let submitted = submit(db, policy::OWNER, &prepared, None)?;
                Ok(submitted["task_id"].as_str().unwrap().to_owned())
            }).unwrap()
            };
        let blocked = |store: &mut Store, task: &str, code: &str| {
            let error = store
                .transaction("fence-assigned-work-fixture", |db| dispatch(db, task))
                .err()
                .expect("stale assigned work should fail before effect publication");
            assert_eq!(error.0.code, code);
            store
                .transaction("settle-assigned-work-fixture", |db| {
                    dispatch_failed(db, task, &error)?;
                    let task = db.of(task, "Task")?;
                    assert_eq!(task.body["lifecycle"], "Failed");
                    assert_eq!(task.body["outcome_knowledge"], "NotStarted");
                    assert_eq!(task.body["result"]["artifact_refs"], json!([]));
                    assert_eq!(operation(db, &task)?.body["effect_refs"], json!([]));
                    Ok(())
                })
                .unwrap();
        };

        let task = enqueue(&mut store, &initial["version"]);
        assert_eq!(
            store
                .transaction("claim-assigned-work-fixture", claim)
                .unwrap(),
            Some(task.clone())
        );
        store
            .transaction("revoke-assigned-work-fixture", |db| {
                policy::revoke(
                    db,
                    policy::OWNER,
                    &json!({
                        "id":access["grant_ref"]["resource_id"],
                        "expected_resource_version":access["grant_ref"]["version"],
                    }),
                )?;
                Ok(())
            })
            .unwrap();
        blocked(&mut store, &task, "PermissionDenied");

        store
            .transaction("restore-project-grant-fixture", project_grant)
            .unwrap();
        let task = enqueue(&mut store, &initial["version"]);
        assert_eq!(
            store
                .transaction("claim-assigned-work-fixture", claim)
                .unwrap(),
            Some(task.clone())
        );
        let revised = store
            .transaction("change-project-roster-fixture", |db| {
                let viewer =
                    policy::register(db, policy::OWNER, &json!({"name":"assigned-visitor"}))?;
                project::set_member(
                    db,
                    policy::OWNER,
                    &json!({
                        "id":project_id,"expected_resource_version":initial["version"],
                        "principal_id":viewer["principal_id"],"role":"Viewer",
                    }),
                )
            })
            .unwrap();
        blocked(&mut store, &task, "Conflict");

        let task = enqueue(&mut store, &revised["reference"]["version"]);
        assert_eq!(
            store
                .transaction("claim-assigned-work-fixture", claim)
                .unwrap(),
            Some(task.clone())
        );
        store
            .transaction("dispatch-current-assigned-work-fixture", |db| {
                assert!(dispatch(db, &task)?.is_none());
                let settled = db.of(&task, "Task")?;
                assert_eq!(settled.body["lifecycle"], "Succeeded");
                assert_eq!(settled.body["outcome_knowledge"], "Known");
                let output = db.resolve(&settled.body["result"]["artifact_refs"][0])?;
                assert_eq!(
                    crate::artifact::read(db, &output, Some(64 * 1024))?,
                    json!({"items":[],"next_after_id":null})
                );
                Ok(())
            })
            .unwrap();
    }
}
