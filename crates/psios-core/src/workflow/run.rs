use super::{ACTION, PreparedStep, RUN, admission_delay, binding, check_tool, run_view};
use crate::{
    Conditional, Result, Store, artifact, broker,
    db::{Db, Doc},
    decode, err, node, policy, require,
};
use chrono::Utc;
use psios_api::id;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = 38;
pub(crate) const SCHEMA: &str = include_str!("parallel.sql");

struct Work {
    run: String,
    task: Option<String>,
    expired: bool,
}

struct Rejection {
    run: String,
    version: String,
    step: String,
}

pub fn cancel(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Conditional = decode(params)?;
    policy::can(db, actor, "task.submit")?;
    let run = db.of(&request.id, RUN)?;
    policy::owned(actor, &run)?;
    run.cas(&request.expected_resource_version)?;
    require(
        db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM active_workflows WHERE run=?1 AND stopping=0)",
            [&run.id],
            |row| row.get::<_, bool>(0),
        )?,
        "Conflict",
        "workflow is already stopping or settled",
    )?;
    Ok(run_view(&stop(db, &run, run.body.clone(), "Cancelled")?))
}

pub fn resume(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Conditional = decode(params)?;
    policy::can(db, actor, "task.submit")?;
    let run = db.of(&request.id, RUN)?;
    policy::owned(actor, &run)?;
    run.cas(&request.expected_resource_version)?;
    let reason = run.body["reason"].as_str().unwrap_or_default();
    require(
        run.body["lifecycle"] == "Blocked" && artifact::is_content_unavailable(reason),
        "PreconditionFailed",
        "workflow is not waiting for a predecessor result",
    )?;
    let delegation = run.spec["delegation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow delegation missing"))?;
    policy::delegation(db, &run.owner, delegation, ACTION)?;
    policy::delegation(db, &run.owner, delegation, "task.read")?;
    let mut body = run.body.clone();
    let step = body["steps"]
        .as_array_mut()
        .and_then(|steps| {
            steps
                .iter_mut()
                .find(|step| step["lifecycle"] == "Blocked" && step["reason"] == reason)
        })
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "workflow result wait has no blocked step",
            )
        })?;
    step["lifecycle"] = json!("Pending");
    step["reason"] = Value::Null;
    require(
        db.c.query_row("SELECT EXISTS(SELECT 1 FROM active_workflows WHERE run=?1 AND stopping=0 AND result_blocked=1)",
                       [&run.id], |row| row.get::<_,bool>(0))?,
        "RecoveryRequired",
        "workflow result wait has no active entry",
    )?;
    Ok(run_view(&publish(
        db,
        &run,
        body,
        Utc::now().timestamp_millis(),
    )?))
}

pub fn tick(store: &mut Store) -> Result<()> {
    let correlation = id("workflow-tick");
    let mut rejection = None;
    match store.transaction(&correlation, |db| advance(db, &mut rejection)) {
        Ok(()) => Ok(()),
        Err(error) if error.0.retry_condition == "SameRequestAfterBackoff" => {
            let Some(rejection) = rejection else {
                return Ok(());
            };
            // The admission transaction rolled back without accepting the
            // step. Defer just this run so another ready run can try.
            match store.transaction(&correlation, |db| {
                admission_delay::defer(db, &rejection.run, &rejection.version, &rejection.step)
            }) {
                Err(error)
                    if error.0.code == "StorageUnavailable"
                        && !store.transaction_is_uncertain() =>
                {
                    eprintln!("psiosd workflow admission deferral: {}", error.0.code);
                    Ok(())
                }
                result => result,
            }
        }
        Err(error) => {
            let Some(rejection) = rejection else {
                return Err(error);
            };
            if !matches!(
                error.0.code.as_str(),
                "PermissionDenied"
                    | "QuotaExceeded"
                    | "Conflict"
                    | "PreconditionFailed"
                    | "NotFound"
                    | "UnsupportedCapability"
                    | "DeadlineExceeded"
                    | "ResultEvicted"
                    | "ResultUnavailable"
            ) {
                return Err(error);
            }
            // Admission and the failing attempt have rolled back. The timer
            // still holds the authority lock while it records the refusal.
            store.transaction(&correlation, |db| {
                let run = db.of(&rejection.run, RUN)?;
                run.cas(&rejection.version)?;
                let mut body = run.body.clone();
                let step = progress(&mut body, &rejection.step)?;
                if artifact::is_content_unavailable(&error.0.code) {
                    step["lifecycle"] = json!("Blocked");
                    step["reason"] = json!(error.0.code);
                    publish(db, &run, body, Utc::now().timestamp_millis())?;
                    return Ok(());
                }
                step["lifecycle"] = json!("Failed");
                step["reason"] = json!(error.0.code);
                fail(db, &run, body, &error.0.code)?;
                Ok(())
            })
        }
    }
}

fn advance(db: &Db<'_>, rejection: &mut Option<Rejection>) -> Result<()> {
    let pending: bool =
        db.c.query_row("SELECT EXISTS(SELECT 1 FROM active_workflows)", [], |row| {
            row.get(0)
        })?;
    if !pending {
        return Ok(());
    }
    let clock = policy::deadline::Clock::sample()?;
    let stamp = Utc::now().timestamp_millis();
    let enabled = node::enabled(db)? && !node::work_admission::draining(db)?;
    let work =
        db.c.query_row(
            "WITH candidates AS (
                 SELECT w.*, delay.boot_id AS deferred_boot_id, delay.time_ns AS deferred_time_ns,
                     delay.deferred_boot_ms, (w.stopping=0 AND (w.deadline_ms<=?1 OR coalesce(
                     elapsed.time_ns!=?3 OR elapsed.deadline_boot_ms<=?4,0))) AS expired,
                     (SELECT a.task FROM active_workflow_tasks a WHERE a.run=w.run
                      AND a.task_rev != coalesce((SELECT rev FROM versions WHERE id=a.task
                          ORDER BY rev DESC LIMIT 1),-1) ORDER BY a.step LIMIT 1) AS changed_task
                 FROM active_workflows w LEFT JOIN authorization_deadlines elapsed
                       ON elapsed.resource=w.run AND elapsed.boot_id=?2
                 LEFT JOIN workflow_admission_deferrals delay ON delay.run=w.run
             )
             SELECT w.run,w.changed_task,w.expired FROM candidates w
             WHERE w.expired OR w.changed_task IS NOT NULL OR
                   (?5 AND w.stopping=0 AND w.result_blocked=0 AND w.ready_ms IS NOT NULL
                    AND (w.deferred_boot_id IS NULL OR w.deferred_boot_id!=?2
                         OR w.deferred_time_ns!=?3 OR w.deferred_boot_ms NOT BETWEEN ?6 AND ?4)
                    AND (SELECT count(*) FROM active_workflow_tasks a WHERE a.run=w.run)<w.max_parallel_steps)
             ORDER BY w.expired DESC,(w.changed_task IS NOT NULL) DESC,w.ready_ms,w.run LIMIT 1",
            params![stamp, clock.boot.boot_id, clock.time_ns, clock.boot.millis, enabled,
                clock.boot.millis - admission_delay::BACKOFF_MS + 1],
            |row| {
                Ok(Work {
                    run: row.get(0)?,
                    task: row.get(1)?,
                    expired: row.get(2)?,
                })
            },
        )
        .optional()?;
    let Some(work) = work else { return Ok(()) };
    let run = db.of(&work.run, RUN)?;
    if work.expired {
        stop(db, &run, run.body.clone(), "DeadlineExceeded")?;
    } else if let Some(task) = work.task {
        let task = db.of(&task, "Task")?;
        observe(db, &run, run.body.clone(), &task)?;
    } else {
        dispatch(db, &run, stamp, rejection)?;
    }
    Ok(())
}

fn progress<'a>(body: &'a mut Value, id: &str) -> Result<&'a mut Value> {
    body["steps"]
        .as_array_mut()
        .and_then(|steps| steps.iter_mut().find(|step| step["step_id"] == id))
        .ok_or_else(|| err("RecoveryRequired", "workflow step projection missing"))
}

fn dispatch(
    db: &Db<'_>,
    run: &Doc,
    ready_ms: i64,
    rejection: &mut Option<Rejection>,
) -> Result<()> {
    require(
        matches!(
            run.body["lifecycle"].as_str(),
            Some("Ready" | "Running" | "WaitingApproval")
        ) && run.body["stop_reason"].is_null(),
        "RecoveryRequired",
        "workflow index disagrees with dispatch state",
    )?;
    let steps: Vec<PreparedStep> = decode(&run.spec["steps"])?;
    let version = steps
        .iter()
        .map(PreparedStep::dsl_version)
        .max()
        .unwrap_or(1);
    // Early DSL 1 definitions declared their version, while their runs omitted it.
    let stored_version = run.spec.get("dsl_version").map_or(Some(1), Value::as_u64);
    require(
        stored_version == Some(u64::from(version)),
        "RecoveryRequired",
        "workflow run version disagrees with its steps",
    )?;
    let statuses = run.body["steps"]
        .as_array()
        .ok_or_else(|| err("RecoveryRequired", "workflow steps missing"))?;
    let state = |id: &str| statuses.iter().find(|step| step["step_id"] == id);
    let has_status =
        |id: &str, phase: &str| state(id).is_some_and(|step| step["lifecycle"] == phase);
    for step in steps.iter().filter(|step| has_status(&step.id, "Pending")) {
        let skipped_parent = step.after.iter().find(|dep| has_status(dep, "Skipped"));
        let all_branches_skipped = !step.after_selected.is_empty()
            && step
                .after_selected
                .iter()
                .all(|dep| has_status(dep, "Skipped"));
        if skipped_parent.is_none() && !all_branches_skipped {
            continue;
        }
        let mut body = run.body.clone();
        let skipped = progress(&mut body, &step.id)?;
        skipped["lifecycle"] = json!("Skipped");
        if let Some(dependency) = skipped_parent {
            skipped["reason"] = json!("DependencyNotSelected");
            skipped["skipped_dependency"] = json!(dependency);
        } else {
            skipped["reason"] = json!("NoSelectedPredecessor");
            skipped["joined_predecessors"] = json!([]);
        }
        return persist_selection(db, run, body, ready_ms);
    }
    let step = steps.iter().find(|step| {
        has_status(&step.id, "Pending")
            && step.after.iter().all(|dep| has_status(dep, "Succeeded"))
            && step
                .after_selected
                .iter()
                .all(|dep| has_status(dep, "Succeeded") || has_status(dep, "Skipped"))
    });
    let Some(step) = step else {
        require(
            has_children(db, run)?,
            "RecoveryRequired",
            "active workflow has no ready dependencies",
        )?;
        db.c.execute(
            "UPDATE active_workflows SET ready_ms=NULL WHERE run=?1",
            [&run.id],
        )?;
        admission_delay::clear(db, run)?;
        return Ok(());
    };
    *rejection = Some(Rejection {
        run: run.id.clone(),
        version: run.version(),
        step: step.id.clone(),
    });
    let delegation = run.spec["delegation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow delegation missing"))?;
    let tool = step.request["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow tool missing"))?;
    policy::delegation(db, &run.owner, delegation, ACTION)?;
    check_tool(db, tool, &step.tool_ref)?;
    if step.dsl_version() > 1 {
        policy::delegation(db, &run.owner, delegation, "task.read")?;
    }
    if let Some(predicate) = &step.when {
        let existing = &state(&step.id).unwrap()["condition"];
        if existing.is_null() {
            let source = state(&predicate.step)
                .ok_or_else(|| err("RecoveryRequired", "workflow condition predecessor missing"))?;
            let choice = predicate.evaluate(db, run, source)?;
            let mut body = run.body.clone();
            let selection = progress(&mut body, &step.id)?;
            if choice["selected"] == false {
                selection["lifecycle"] = json!("Skipped");
                selection["reason"] = json!("ConditionFalse");
            }
            selection["condition"] = choice;
            return persist_selection(db, run, body, ready_ms);
        }
        require(
            existing["selected"] == true && existing["predicate_digest"] == predicate.digest(),
            "RecoveryRequired",
            "stored workflow selection disagrees with the definition",
        )?;
    }
    let resolved;
    let request = if step.inputs_from.is_empty() {
        &step.request
    } else {
        let pinned = &state(&step.id).unwrap()["input_binding"];
        if pinned.is_null() {
            let mut body = run.body.clone();
            progress(&mut body, &step.id)?["input_binding"] =
                binding::pin(db, run, step, statuses)?;
            return persist_selection(db, run, body, ready_ms);
        }
        resolved = binding::request(step, pinned)?;
        &resolved
    };
    let receipt = broker::submit_workflow(db, &run.owner, request, &run.id, &step.step_key)?;
    let task_id = receipt["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow task receipt has no ID"))?;
    let task = db.of(task_id, "Task")?;
    let mut body = run.body.clone();
    let projection = progress(&mut body, &step.id)?;
    projection["task_receipt"] = receipt;
    if !step.after_selected.is_empty() {
        let selected: Vec<_> = step
            .after_selected
            .iter()
            .filter_map(|id| state(id).filter(|dep| dep["lifecycle"] == "Succeeded"))
            .map(|dep| json!({"step_id":dep["step_id"],"task_ref":dep["task_ref"]}))
            .collect();
        projection["joined_predecessors"] = json!(selected);
    }
    require(
        db.c.execute("INSERT INTO active_workflow_tasks(run,step,task,task_rev)
                      SELECT w.run,?2,?3,?4 FROM active_workflows w WHERE w.run=?1 AND w.stopping=0
                        AND w.ready_ms IS NOT NULL AND w.result_blocked=0
                        AND (SELECT count(*) FROM active_workflow_tasks a WHERE a.run=w.run)<w.max_parallel_steps",
                     params![run.id, step.id, task.id, task.rev])? == 1,
        "RecoveryRequired",
        "workflow admission did not replace a ready entry",
    )?;
    observe(db, run, body, &task)?;
    Ok(())
}

fn all_steps_complete(body: &Value) -> bool {
    body["steps"].as_array().is_some_and(|steps| {
        steps
            .iter()
            .all(|step| step["lifecycle"] == "Succeeded" || step["lifecycle"] == "Skipped")
    })
}

fn persist_selection(db: &Db<'_>, run: &Doc, body: Value, ready_ms: i64) -> Result<()> {
    publish(db, run, body, ready_ms)?;
    Ok(())
}

fn active_tasks(db: &Db<'_>, run: &Doc) -> Result<Vec<(String, String)>> {
    Ok(db
        .c
        .prepare("SELECT step,task FROM active_workflow_tasks WHERE run=?1 ORDER BY step")?
        .query_map([&run.id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

fn has_children(db: &Db<'_>, run: &Doc) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM active_workflow_tasks WHERE run=?1)",
        [&run.id],
        |row| row.get(0),
    )?)
}

fn record_task(
    db: &Db<'_>,
    run: &Doc,
    body: &mut Value,
    task: &Doc,
) -> Result<Option<&'static str>> {
    let step_id: String =
        db.c.query_row(
            "SELECT step FROM active_workflow_tasks WHERE run=?1 AND task=?2",
            params![run.id, task.id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| err("RecoveryRequired", "workflow task has no active dispatch"))?;
    let step = progress(body, &step_id)?;
    require(
        task.owner == run.owner
            && task.body["origin_ref"]["origin_type"] == "WorkflowRun"
            && task.body["origin_ref"]["origin_id"] == run.id
            && task.body["origin_ref"]["step_key"] == step["step_key"]
            && task.body["origin_ref"] == step["task_receipt"]["origin_ref"]
            && step["task_receipt"]["task_id"] == task.id
            && step["task_receipt"]["request_digest"] == task.spec["request_digest"],
        "RecoveryRequired",
        "workflow task does not match the durable dispatch",
    )?;
    let phase = task.body["lifecycle"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow task lifecycle missing"))?;
    let knowledge = &task.body["outcome_knowledge"];
    require(
        phase != "Succeeded" || knowledge == "Known",
        "RecoveryRequired",
        "successful predecessor must have a known outcome",
    )?;
    step["task_ref"] = task.reference();
    step["outcome_knowledge"] = knowledge.clone();
    step["lifecycle"] = json!(match phase {
        "Queued" => "Ready",
        "Paused" => "Running",
        phase => phase,
    });
    if knowledge == "Unknown" {
        step["reason"] = json!("OutcomeUnknown");
    } else if phase == "Paused" {
        step["reason"] = json!("TaskPaused");
    } else {
        step.as_object_mut()
            .expect("workflow step projection")
            .remove("reason");
    }

    let terminal = knowledge != "Unknown" && matches!(phase, "Succeeded" | "Failed" | "Cancelled");
    let affected = if terminal {
        db.c.execute(
            "DELETE FROM active_workflow_tasks WHERE run=?1 AND task=?2",
            params![run.id, task.id],
        )?
    } else {
        db.c.execute(
            "UPDATE active_workflow_tasks SET task_rev=?3 WHERE run=?1 AND task=?2",
            params![run.id, task.id, task.rev],
        )?
    };
    require(
        affected == 1,
        "RecoveryRequired",
        "workflow observation lacks the original dispatch",
    )?;
    Ok(match (terminal, phase) {
        (true, "Failed") => Some("StepFailed"),
        (true, "Cancelled") => Some("StepCancelled"),
        _ => None,
    })
}

fn observe(db: &Db<'_>, run: &Doc, mut body: Value, task: &Doc) -> Result<Doc> {
    let failure = record_task(db, run, &mut body, task)?;
    if let Some(reason) = failure
        && body["stop_reason"].is_null()
    {
        return fail(db, run, body, reason);
    }
    publish(db, run, body, Utc::now().timestamp_millis())
}

fn stop_phase(reason: &str) -> &'static str {
    if reason == "Cancelled" {
        "Cancelled"
    } else {
        "Failed"
    }
}

fn fail(db: &Db<'_>, run: &Doc, body: Value, reason: &str) -> Result<Doc> {
    if has_children(db, run)? {
        stop(db, run, body, reason)
    } else {
        finish(db, run, body, "Failed", Some(reason))
    }
}

fn stop(db: &Db<'_>, run: &Doc, mut body: Value, reason: &str) -> Result<Doc> {
    body["stop_reason"] = json!(reason);
    for (_, id) in active_tasks(db, run)? {
        let task = broker::cancel_task(db, &db.of(&id, "Task")?)?;
        record_task(db, run, &mut body, &task)?;
    }
    publish(db, run, body, Utc::now().timestamp_millis())
}

fn publish(db: &Db<'_>, run: &Doc, mut body: Value, stamp: i64) -> Result<Doc> {
    admission_delay::clear(db, run)?;
    let active = active_tasks(db, run)?;
    let stopping = body["stop_reason"].as_str().map(str::to_owned);
    if active.is_empty() {
        if let Some(reason) = stopping.as_deref() {
            return finish(db, run, body, stop_phase(reason), Some(reason));
        }
        if all_steps_complete(&body) {
            return finish(db, run, body, "Succeeded", None);
        }
    }
    let steps = body["steps"]
        .as_array()
        .ok_or_else(|| err("RecoveryRequired", "workflow step projections missing"))?;
    let unknown = steps
        .iter()
        .any(|step| step["outcome_knowledge"] == "Unknown");
    let paused = steps.iter().any(|step| step["reason"] == "TaskPaused");
    let dependency = steps
        .iter()
        .filter(|step| step["lifecycle"] == "Blocked")
        .filter_map(|step| step["reason"].as_str())
        .find(|reason| artifact::is_content_unavailable(reason));
    let waiting_approval = !active.is_empty()
        && active.iter().all(|(id, _)| {
            steps
                .iter()
                .any(|step| step["step_id"] == *id && step["lifecycle"] == "WaitingApproval")
        });
    let result_blocked = dependency.is_some();
    let (phase, reason, can_admit) = if unknown {
        ("Blocked", Some("OutcomeUnknown"), false)
    } else if paused {
        ("Blocked", Some("TaskPaused"), false)
    } else if let Some(reason) = stopping.as_deref() {
        ("Cancelling", Some(reason), false)
    } else if let Some(reason) = dependency {
        ("Blocked", Some(reason), false)
    } else if active.is_empty() {
        ("Ready", None, true)
    } else if waiting_approval {
        ("WaitingApproval", None, true)
    } else {
        ("Running", None, true)
    };
    let reason = reason.map(str::to_owned);
    body["lifecycle"] = json!(phase);
    body["reason"] = json!(reason);
    body["active_step"] = json!(active.first().map(|(step, _)| step));
    if run.spec.get("max_parallel_steps").is_some() {
        body["active_steps"] = json!(active.iter().map(|(step, _)| step).collect::<Vec<_>>());
    }
    if unknown {
        body["outcome_knowledge"] = json!("Unknown");
    } else if !active.is_empty() || body["outcome_knowledge"] != "NotStarted" {
        body["outcome_knowledge"] = json!("InFlight");
    }
    require(
        db.c.execute(
            "UPDATE active_workflows SET ready_ms=?2,stopping=?3,result_blocked=?4 WHERE run=?1",
            params![
                run.id,
                can_admit.then_some(stamp),
                stopping.is_some(),
                result_blocked
            ],
        )? == 1,
        "RecoveryRequired",
        "workflow state has no active entry",
    )?;
    db.status(run, body)
}

fn finish(
    db: &Db<'_>,
    run: &Doc,
    mut body: Value,
    phase: &str,
    reason: Option<&str>,
) -> Result<Doc> {
    require(
        db.c.execute(
            "DELETE FROM active_workflows WHERE run=?1 AND NOT EXISTS(
            SELECT 1 FROM active_workflow_tasks WHERE run=?1)",
            [&run.id],
        )? == 1,
        "RecoveryRequired",
        "workflow closed without an active entry",
    )?;
    let steps = body["steps"]
        .as_array_mut()
        .ok_or_else(|| err("RecoveryRequired", "workflow step projections missing"))?;
    for pending in steps
        .iter_mut()
        .filter(|step| step["lifecycle"] == "Pending")
    {
        pending["lifecycle"] = json!("Cancelled");
        pending["reason"] = json!("WorkflowStopped");
    }
    body["lifecycle"] = json!(phase);
    body["reason"] = json!(reason);
    body["active_step"] = Value::Null;
    if run.spec.get("max_parallel_steps").is_some() {
        body["active_steps"] = json!([]);
    }
    if body["outcome_knowledge"] != "NotStarted" || phase == "Succeeded" {
        body["outcome_knowledge"] = json!("Known");
    }
    db.status(run, body)
}
