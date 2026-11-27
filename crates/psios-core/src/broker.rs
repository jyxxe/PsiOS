pub(crate) mod admission;
mod approval;
pub(crate) mod concurrency;
pub(crate) mod dependencies;
mod execution;
mod history;
mod network_trial;
pub(crate) mod preparation;
pub(crate) use history::{archive, history, redact_late_receipt};
mod recovery;
pub(crate) mod result_access;
mod submission;
pub(crate) mod task_list;

use crate::{
    db::{Db, Doc},
    host::{self, Action, Work},
    policy, *,
};
pub(crate) use approval::{decide, verify_bound_approval};
#[cfg(test)]
pub(crate) use execution::finish;
pub(crate) use execution::{claim, dispatch, dispatch_failed, finish_host, settlement_failed};
pub(crate) use network_trial::{
    authorize_network_trial_confirm, authorize_network_trial_rollback, expire_network_trials,
    finish_network_trial_confirm, finish_network_trial_rollback, network_trial_confirmation,
    prepare_network_trial_confirm, prepare_network_trial_rollback,
};
use psios_api::{VERSION, canonical, contract, digest, id, sha256};
pub(crate) use recovery::{
    ReconcileProbe, ReconcileSample, inspected as inspect_recovery, prepare_reconcile, reconcile,
    recover, release_unstarted_claim,
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use submission::authority;
pub(crate) use submission::{
    bind_workflow_input, cancel_origin, discard_preparation, lookup, prepare,
    prepare_claimed_model, prepare_scheduled, prepare_workflow, retime_scheduled, retry,
    retry_in_session, submit, submit_claimed_model_tool, submit_scheduled, submit_workflow,
};

fn field<'v>(v: &'v Value, k: &str) -> &'v str {
    v[k].as_str().expect("validated persisted field")
}
pub(crate) fn condition(code: &str, msg: &str) -> Value {
    json!({
        "type": code,
        "status": "True",
        "reason": code,
        "message": msg,
        "observed_generation": "1",
        "last_transition_at": now()
    })
}
pub fn get(db: &Db<'_>, actor: &str, p: &Value, kind: &str) -> Result<Value> {
    policy::can(db, actor, "task.read")?;
    let p: ById = decode(p)?;
    let d = db.of(&p.id, kind)?;
    policy::owned(actor, &d)?;
    Ok(d.body)
}
fn operation(db: &Db<'_>, t: &Doc) -> Result<Doc> {
    db.of(
        field(&t.body["operation_refs"][0], "resource_id"),
        "Operation",
    )
}
fn op_update(
    db: &Db<'_>,
    op: &Doc,
    state: &str,
    knowledge: &str,
    effects: Value,
    conditions: Value,
) -> Result<Doc> {
    let mut b = op.body.clone();
    b["lifecycle"] = json!(state);
    b["outcome_knowledge"] = json!(knowledge);
    b["effect_refs"] = effects;
    b["conditions"] = conditions;
    b["updated_at"] = json!(now());
    db.status(op, b)
}
fn close(
    db: &Db<'_>,
    t: &Doc,
    state: &str,
    knowledge: &str,
    reason: Option<&Error>,
    artifact: Option<&Doc>,
) -> Result<Doc> {
    let phase: String =
        db.c.query_row("SELECT phase FROM queue WHERE task=?1", [&t.id], |r| {
            r.get(0)
        })?;
    let conditions = reason
        .map(|e| vec![condition(&e.0.code, &e.0.message)])
        .unwrap_or_default();
    if phase == "WaitingApproval" {
        let request: String = db.c.query_row(
            "SELECT request FROM approval_bindings WHERE task=?1",
            [&t.id],
            |r| r.get(0),
        )?;
        let d = db.get(&request)?;
        if d.body["status"] == "Pending" {
            let mut b = d.body.clone();
            b["status"] = json!(if reason.is_some_and(|e| e.0.code == "DeadlineExceeded") {
                "Expired"
            } else {
                "Cancelled"
            });
            db.status(&d, b)?;
        }
    }
    if matches!(
        phase.as_str(),
        "WaitingResource" | "WaitingApproval" | "Queued" | "Claimed"
    ) {
        let decision:Option<String>=db.c.query_row("SELECT a.decision FROM approval_bindings a JOIN current d ON d.id=a.decision
            WHERE a.task=?1 AND a.operation IS NULL AND d.kind='ApprovalDecision' AND d.body->>'decision'='Approved' ",[&t.id],|r|r.get(0)).optional()?;
        if let Some(decision) = decision {
            let use_ = db.of(&format!("use-{decision}"), "ApprovalUse")?;
            let mut b = use_.body.clone();
            b["validity_status"] = json!(match reason.map(|e| e.0.code.as_str()) {
                Some("DeadlineExceeded") => "Expired",
                Some("PermissionDenied") => "Revoked",
                _ => "Superseded",
            });
            db.status(&use_, b)?;
        }
        db.c.execute("UPDATE budgets SET reserved=reserved-1 WHERE id=(SELECT budget FROM queue WHERE task=?1)",[&t.id])?;
    }
    let unknown = knowledge == "Unknown";
    if unknown
        && matches!(
            t.spec["task_type"].as_str(),
            Some(apps::stop::TOOL | apps::create_recovery::CLEANUP)
        )
    {
        // The exclusive cleanup is still held by its recovery record. Free the
        // reserved queue slot so independent health can prove removal.
        admission::release(db, &t.id)?;
    }
    db.c.execute(
        "UPDATE queue SET phase=?2 WHERE task=?1",
        params![t.id, if unknown { "Unknown" } else { "Done" }],
    )?;
    let op = operation(db, t)?;
    if !unknown
        && state != "Succeeded"
        && matches!(
            t.spec["task_type"].as_str(),
            Some("volume.delete" | "backup.delete")
        )
    {
        storage::retirement::abandon(db, &op)?;
    }
    if !unknown && state != "Succeeded" && t.spec["task_type"] == storage::entry_retirement::TOOL {
        storage::entry_retirement::abandon(db, &op)?;
    }
    if !unknown && state != "Succeeded" && t.spec["task_type"] == storage::directory_create::TOOL {
        storage::directory_create::abandon(db, &op)?;
    }
    let mut effects = op.body["effect_refs"].clone();
    if let Some(r) = effects.as_array_mut().and_then(|a| a.last_mut()) {
        let e = db.get(field(r, "resource_id"))?;
        let mut b = e.body.clone();
        b["outcome_knowledge"] = json!(knowledge);
        if knowledge == "Known" {
            b["phase"] = json!(if phase == "Unknown" {
                "EffectReconciled"
            } else {
                "EffectConfirmed"
            })
        }
        if let Some(a) = artifact {
            let mut refs = vec![a.reference()];
            refs.extend(a.spec.get("reconciliation_evidence_ref").cloned());
            if node::service_control::is_tool(field(&t.spec, "task_type"))
                && let Some(confirmation) = node::service_control::record_settlement(db, &op, a)?
            {
                refs.push(confirmation.reference());
            }
            if t.spec["task_type"] == apps::restart::TOOL
                && let Some(confirmation) = apps::restart::record_settlement(db, &op, a)?
            {
                refs.push(confirmation.reference());
            }
            if t.spec["task_type"] == storage::entry_retirement::TOOL {
                refs.push(storage::entry_retirement::settlement_ref(db, &op)?);
            }
            if t.spec["task_type"] == storage::directory_create::TOOL {
                refs.push(storage::directory_create::settlement_ref(db, &op)?);
            }
            b["evidence_refs"] = json!(refs);
        }
        *r = db.status(&e, b)?.reference();
    }
    let operation = op_update(
        db,
        &op,
        if state == "Paused" { "Failed" } else { state },
        knowledge,
        effects,
        json!(conditions),
    )?;
    let mut task = t.body.clone();
    task["lifecycle"] = json!(state);
    task["outcome_knowledge"] = json!(knowledge);
    task["operation_refs"] = json!([operation.reference()]);
    task["conditions"] = json!(conditions);
    let outputs = json!(artifact.map(Doc::reference).into_iter().collect::<Vec<_>>());
    task["result"] = json!({
        "availability": if artifact.is_some(){"Available"}else{"Unavailable"},
        "artifact_refs": outputs
    });
    if let Some(rid) = task["current_run_id"].as_str() {
        let r = db.of(rid, "Run")?;
        let mut b = r.body.clone();
        b["lifecycle"] = json!(if unknown || phase == "Unknown" {
            "Lost"
        } else {
            state
        });
        b["outcome_knowledge"] = json!(knowledge);
        b["operation_refs"] = task["operation_refs"].clone();
        b["conditions"] = json!(conditions);
        b["output_refs"] = outputs;
        db.status(&r, b)?;
    }
    if !unknown {
        admission::release(db, &t.id)?;
        dependencies::release(db, &t.id)?;
        artifact::capacity::release(db, &t.id)?;
        if t.spec["task_type"] == agent::START_SESSION {
            agent::session_inputs::release_task(db, &t.id)?;
        }
        if matches!(
            t.spec["task_type"].as_str(),
            Some(apps::artifact_install::TOOL | apps::artifact_install::STAGE_TOOL)
        ) {
            apps::artifact_install::release(db, &t.id)?;
        }
        if t.spec["task_type"] == apps::restore_image::TOOL {
            apps::restore_image::release(db, &op.id)?;
        }
        if t.spec["task_type"] == apps::stop::TOOL {
            apps::stop::release(db, &op.id, &t.id)?;
        }
        if t.spec["task_type"] == apps::create_recovery::CLEANUP {
            apps::create_recovery::close(db, &t.id)?;
        }
        if t.spec["task_type"] == apps::create_recovery::CREATE {
            apps::create_recovery::release_original(db, &op.id)?;
        }
        storage::read_holds::release(db, &t.id)?;
        db.c.execute("DELETE FROM holds WHERE holder=?1", [&op.id])?;
    }
    db.audit(&t.owner, "Task.Terminate", Some(&t.id), state)?;
    let t = db.status(t, task)?;
    agent::task_closed(db, &t)?;
    Ok(t)
}

fn cancel_unresolved(db: &Db<'_>, actor: &str, task: &Doc) -> Result<Doc> {
    let missing = || {
        err(
            "RecoveryRequired",
            "unresolved task lost its original operation or run",
        )
    };
    let operation_id: String =
        db.c.query_row(
            "SELECT operation FROM queue WHERE task=?1 AND phase='Unknown'",
            [&task.id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
        .ok_or_else(missing)?;
    let operation = db.find(&operation_id)?.ok_or_else(missing)?;
    let run_id = task.body["current_run_id"].as_str().ok_or_else(missing)?;
    let run = db.find(run_id)?.ok_or_else(missing)?;
    require(
        operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && operation.body["outcome_knowledge"] == "Unknown"
            && task.body["operation_refs"] == json!([operation.reference()])
            && run.kind == "Run"
            && run.owner == task.owner
            && run.body["task_id"] == task.id
            && run.body["lifecycle"] == "Lost"
            && run.body["outcome_knowledge"] == "Unknown"
            && run.body["operation_refs"] == task.body["operation_refs"],
        "RecoveryRequired",
        "unresolved task no longer matches its original operation and run",
    )?;

    // Close the Task lifecycle without moving the effect's recovery boundary.
    let mut body = task.body.clone();
    body["lifecycle"] = json!("Cancelled");
    let cancelled = db.status(task, body)?;
    db.audit(
        actor,
        "Task.RecordCancellation",
        Some(&task.id),
        "PendingReconciliation",
    )?;
    agent::task_closed(db, &cancelled)?;
    Ok(cancelled)
}

pub(crate) fn cancel_task(db: &Db<'_>, actor: &str, t: &Doc) -> Result<Doc> {
    if t.spec["task_type"] == node::network::trial::TOOL
        && let Some(cancelled) = network_trial::cancel(db, t)?
    {
        return Ok(cancelled);
    }
    match field(&t.body, "lifecycle") {
        "Paused" if t.body["outcome_knowledge"] == "Unknown" => cancel_unresolved(db, actor, t),
        "Succeeded" | "Failed" | "Cancelled" | "Paused" | "Cancelling" => Ok(t.clone()),
        "Running" => {
            let phase: String =
                db.c.query_row("SELECT phase FROM queue WHERE task=?1", [&t.id], |r| {
                    r.get(0)
                })?;
            if phase == "Claimed" {
                return close(db, t, "Cancelled", "NotStarted", None, None);
            }
            let mut b = t.body.clone();
            b["lifecycle"] = json!("Cancelling");
            db.status(t, b)
        }
        _ => close(db, t, "Cancelled", "NotStarted", None, None),
    }
}
pub fn cancel(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "task.submit")?;
    let p: Conditional = decode(p)?;
    let t = db.of(&p.id, "Task")?;
    policy::owned(actor, &t)?;
    t.cas(&p.expected_resource_version)?;
    Ok(cancel_task(db, actor, &t)?.body)
}
