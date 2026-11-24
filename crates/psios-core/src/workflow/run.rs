use super::{ACTION, PreparedStep, RUN, check_tool, run_view};
use crate::{
    Conditional, Result, Store, broker,
    db::{Db, Doc},
    decode, err, node, policy, require,
};
use chrono::Utc;
use psios_api::id;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

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
    let task: Option<String> =
        db.c.query_row(
            "SELECT task FROM active_workflows WHERE run=?1 AND stopping=0",
            [&run.id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| err("Conflict", "workflow is already stopping or settled"))?;
    Ok(run_view(&stop(db, &run, task.as_deref(), "Cancelled")?))
}

pub fn tick(store: &mut Store) -> Result<()> {
    let correlation = id("workflow-tick");
    let mut rejection = None;
    match store.transaction(&correlation, |db| advance(db, &mut rejection)) {
        Ok(()) => Ok(()),
        Err(error) if error.0.retry_condition == "SameRequestAfterBackoff" => Ok(()),
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
                step["lifecycle"] = json!("Failed");
                step["reason"] = json!(error.0.code);
                finish(db, &run, body, "Failed", Some(&error.0.code))?;
                Ok(())
            })
        }
    }
}

fn advance(db: &Db<'_>, rejection: &mut Option<Rejection>) -> Result<()> {
    let stamp = Utc::now().timestamp_millis();
    let work =
        db.c.query_row(
            "SELECT w.run,w.task,(w.stopping=0 AND w.deadline_ms<=?1)
         FROM active_workflows w
         WHERE (w.stopping=0 AND w.deadline_ms<=?1) OR
               w.task IS NULL OR
               (w.task IS NOT NULL AND w.task_rev != COALESCE(
                  (SELECT rev FROM versions WHERE id=w.task ORDER BY rev DESC LIMIT 1),-1))
         ORDER BY CASE WHEN w.stopping=0 AND w.deadline_ms<=?1 THEN 0
                       WHEN w.task IS NOT NULL THEN 1 ELSE 2 END,w.ready_ms,w.run LIMIT 1",
            [stamp],
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
        stop(db, &run, work.task.as_deref(), "DeadlineExceeded")?;
    } else if let Some(task) = work.task {
        let task = db.of(&task, "Task")?;
        observe(db, &run, run.body.clone(), &task)?;
    } else if node::enabled(db)? {
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
        run.body["lifecycle"] == "Ready" && run.body["active_step"].is_null(),
        "RecoveryRequired",
        "workflow index disagrees with dispatch state",
    )?;
    let steps: Vec<PreparedStep> = decode(&run.spec["steps"])?;
    let statuses = run.body["steps"]
        .as_array()
        .ok_or_else(|| err("RecoveryRequired", "workflow steps missing"))?;
    let has_status = |id: &str, phase: &str| {
        statuses
            .iter()
            .any(|step| step["step_id"] == id && step["lifecycle"] == phase)
    };
    let step = steps
        .iter()
        .find(|step| {
            has_status(&step.id, "Pending")
                && step.after.iter().all(|dep| has_status(dep, "Succeeded"))
        })
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "active workflow has no ready dependencies",
            )
        })?;
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
    let receipt = broker::submit_workflow(db, &run.owner, &step.request, &run.id, &step.step_key)?;
    let task_id = receipt["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow task receipt has no ID"))?;
    let task = db.of(task_id, "Task")?;
    let mut body = run.body.clone();
    progress(&mut body, &step.id)?["task_receipt"] = receipt;
    body["active_step"] = json!(step.id);
    require(
        db.c.execute("UPDATE active_workflows SET task=?2,task_rev=?3,ready_ms=?4 WHERE run=?1 AND task IS NULL AND stopping=0",
                     params![run.id, task.id, task.rev, ready_ms])? == 1,
        "RecoveryRequired",
        "workflow admission did not replace a ready entry",
    )?;
    observe(db, run, body, &task)?;
    Ok(())
}

fn observe(db: &Db<'_>, run: &Doc, mut body: Value, task: &Doc) -> Result<Doc> {
    let step_id = body["active_step"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "workflow active step missing"))?
        .to_owned();
    let stopping = body["stop_reason"].as_str().map(str::to_owned);
    let step = progress(&mut body, &step_id)?;
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
    step["task_ref"] = task.reference();
    step["outcome_knowledge"] = knowledge.clone();
    step["lifecycle"] = json!(match phase {
        "Queued" => "Ready",
        "Paused" => "Running",
        phase => phase,
    });
    if knowledge != "Unknown" && matches!(phase, "Succeeded" | "Failed" | "Cancelled") {
        body["active_step"] = Value::Null;
        if let Some(reason) = &stopping {
            return finish(db, run, body, stop_phase(reason), Some(reason));
        }
        if phase != "Succeeded" {
            let reason = if phase == "Failed" {
                "StepFailed"
            } else {
                "StepCancelled"
            };
            return finish(db, run, body, "Failed", Some(reason));
        }
        if body["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|step| step["lifecycle"] == "Succeeded")
        {
            return finish(db, run, body, "Succeeded", None);
        }
        require(
            db.c.execute("UPDATE active_workflows SET task=NULL,task_rev=0,ready_ms=?3 WHERE run=?1 AND task=?2",
                         params![run.id, task.id, Utc::now().timestamp_millis()])? == 1,
            "RecoveryRequired",
            "workflow completion lacks the original dispatch",
        )?;
        body["lifecycle"] = json!("Ready");
        body["outcome_knowledge"] = json!("InFlight");
        body["reason"] = Value::Null;
    } else {
        let (phase, reason, outcome): (&str, Option<&str>, &str) = if knowledge == "Unknown" {
            ("Blocked", Some("OutcomeUnknown"), "Unknown")
        } else if phase == "Paused" {
            ("Blocked", Some("TaskPaused"), "InFlight")
        } else if stopping.is_some() {
            ("Cancelling", stopping.as_deref(), "InFlight")
        } else if phase == "WaitingApproval" {
            ("WaitingApproval", None, "InFlight")
        } else {
            ("Running", None, "InFlight")
        };
        body["lifecycle"] = json!(phase);
        body["reason"] = json!(reason);
        body["outcome_knowledge"] = json!(outcome);
        require(
            db.c.execute(
                "UPDATE active_workflows SET task_rev=?3,stopping=?4 WHERE run=?1 AND task=?2",
                params![run.id, task.id, task.rev, stopping.is_some()],
            )? == 1,
            "RecoveryRequired",
            "workflow observation lacks the original dispatch",
        )?;
    }
    db.status(run, body)
}

fn stop_phase(reason: &str) -> &'static str {
    if reason == "Cancelled" {
        "Cancelled"
    } else {
        "Failed"
    }
}

fn stop(db: &Db<'_>, run: &Doc, task: Option<&str>, reason: &str) -> Result<Doc> {
    let mut body = run.body.clone();
    body["stop_reason"] = json!(reason);
    if let Some(id) = task {
        let task = broker::cancel_task(db, &db.of(id, "Task")?)?;
        observe(db, run, body, &task)
    } else {
        finish(db, run, body, stop_phase(reason), Some(reason))
    }
}

fn finish(
    db: &Db<'_>,
    run: &Doc,
    mut body: Value,
    phase: &str,
    reason: Option<&str>,
) -> Result<Doc> {
    require(
        db.c.execute("DELETE FROM active_workflows WHERE run=?1", [&run.id])? == 1,
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
    if body["outcome_knowledge"] != "NotStarted" || phase == "Succeeded" {
        body["outcome_knowledge"] = json!("Known");
    }
    db.status(run, body)
}
