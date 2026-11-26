use super::*;
use crate::node::network::trial;

pub(super) fn finish_apply(db: &Db<'_>, work: &Work, result: Result<host::Output>) -> Result<()> {
    let task = db.of(&work.task.id, "Task")?;
    let phase: String =
        db.c.query_row("SELECT phase FROM queue WHERE task=?1", [&task.id], |r| {
            r.get(0)
        })?;
    if phase != "Dispatched" || task.body["current_run_id"] != work.run.id {
        db.audit(&task.owner, "Run.LateReceipt", Some(&work.run.id), "Fenced")?;
        return Ok(());
    }
    let error = match result {
        Ok(host::Output::Receipt(receipt)) => return trial::park(db, work, receipt),
        Ok(_) => err("Fenced", "network trial produced an unexpected receipt"),
        Err(error) => error,
    };
    let op = operation(db, &task)?;
    if trial::not_started(db, &op)? {
        let state = if task.body["lifecycle"] == "Cancelling" {
            "Cancelled"
        } else {
            "Failed"
        };
        close(db, &task, state, "NotStarted", Some(&error), None)?;
        return Ok(());
    }
    unknown(
        db,
        &task,
        "the network trial requires independent recovery evidence",
        false,
    )?;
    Ok(())
}

fn unknown(db: &Db<'_>, task: &Doc, detail: &str, cancelled: bool) -> Result<Doc> {
    let state = if cancelled || task.body["lifecycle"] == "Cancelling" {
        "Cancelled"
    } else {
        "Paused"
    };
    close(
        db,
        task,
        state,
        "Unknown",
        Some(&err("OutcomeUnknown", detail)),
        None,
    )
}

pub(super) fn cancel(db: &Db<'_>, task: &Doc) -> Result<Option<Doc>> {
    if task.body["lifecycle"] != trial::WAITING {
        return Ok(None);
    }
    let op = operation(db, task)?;
    if trial::confirmation_issued(db, &op)? {
        let mut body = task.body.clone();
        body["lifecycle"] = json!("Cancelling");
        return Ok(Some(db.status(task, body)?));
    }
    Ok(Some(unknown(
        db,
        task,
        "cancelled while the NetworkManager rollback was pending",
        true,
    )?))
}

pub(super) fn recover_parked(db: &Db<'_>) -> Result<()> {
    for id in trial::parked_tasks(db)? {
        let task = db.of(&id, "Task")?;
        unknown(
            db,
            &task,
            "authority restarted during the network trial",
            false,
        )?;
    }
    Ok(())
}

pub(crate) fn expire_network_trials(db: &Db<'_>) -> Result<()> {
    for id in trial::parked_tasks(db)? {
        let task = db.of(&id, "Task")?;
        if trial::should_expire(db, &operation(db, &task)?, &task)? {
            unknown(
                db,
                &task,
                "network trial confirmation is no longer authorized",
                false,
            )?;
        }
    }
    Ok(())
}

pub(crate) fn network_trial_confirmation(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
) -> Result<Value> {
    trial::challenge(db, actor, params)
}

pub(crate) fn prepare_network_trial_confirm(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
) -> Result<trial::Confirmation> {
    trial::prepare_confirm(db, actor, params)
}

pub(crate) fn authorize_network_trial_confirm(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    job: &trial::Confirmation,
) -> Result<()> {
    trial::authorize_confirm(db, actor, params, job)
}

pub(crate) fn finish_network_trial_confirm(
    db: &Db<'_>,
    actor: &str,
    job: &trial::Confirmation,
    result: Result<trial::ConfirmationReceipt>,
) -> Result<Value> {
    let (op, result) = trial::confirm_result(db, actor, job, result)?;
    let task = db.of(field(&op.spec, "task_id"), "Task")?;
    if let Some(body) = result {
        let run = db.of(field(&task.body, "current_run_id"), "Run")?;
        artifact::capacity::check_result(db, &task, &body)?;
        let artifact = artifact::publish(
            db,
            &task.owner,
            json!({"task_id":task.id, "operation_id":op.id, "run_id":run.id,
                "effect_id":op.body["effect_refs"][0]["resource_id"]}),
            &body,
        )?;
        artifact::capacity::retain(db, &task, &artifact)?;
        trial::mark_settled(db, &op, true)?;
        let state = if task.body["lifecycle"] == "Cancelling" {
            "Cancelled"
        } else {
            "Succeeded"
        };
        close(db, &task, state, "Known", None, Some(&artifact))?;
        db.audit(actor, "NetworkTrial.Confirm", Some(&op.id), "Committed")?;
    } else {
        unknown(
            db,
            &task,
            "NetworkManager did not confirm the checkpoint destruction",
            false,
        )?;
    }
    Ok(db.of(&op.id, "Operation")?.body)
}
