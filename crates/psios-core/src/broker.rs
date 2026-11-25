mod approval;
pub(crate) mod concurrency;
pub(crate) mod dependencies;
mod execution;
mod history;
pub(crate) use history::{archive, history};
mod recovery;
pub(crate) mod result_access;
mod submission;

use crate::{
    db::{Db, Doc},
    host::{self, Action, Work},
    policy, *,
};
pub(crate) use approval::decide;
pub(crate) use execution::{claim, dispatch, dispatch_failed, finish, settlement_failed};
use psios_api::{VERSION, canonical, contract, digest, id, sha256};
pub(crate) use recovery::{reconcile, recover, release_unstarted_claim};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use submission::authority;
pub(crate) use submission::{
    bind_workflow_input, cancel_origin, lookup, prepare, prepare_scheduled, prepare_workflow,
    retime_scheduled, retry, retry_in_session, submit, submit_scheduled, submit_workflow,
};

fn field<'v>(v: &'v Value, k: &str) -> &'v str {
    v[k].as_str().expect("validated persisted field")
}
fn condition(code: &str, msg: &str) -> Value {
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
        dependencies::release(db, &t.id)?;
        db.c.execute("DELETE FROM holds WHERE holder=?1", [&op.id])?;
    }
    db.audit(&t.owner, "Task.Terminate", Some(&t.id), state)?;
    let t = db.status(t, task)?;
    agent::task_closed(db, &t)?;
    Ok(t)
}
pub(crate) fn cancel_task(db: &Db<'_>, t: &Doc) -> Result<Doc> {
    match field(&t.body, "lifecycle") {
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
    Ok(cancel_task(db, &t)?.body)
}
