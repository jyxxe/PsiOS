//! Durable one-shot admission into the ordinary Task broker.
use crate::{
    ById, Conditional, Error, Result, broker,
    db::{Db, Doc},
    decode, err,
    host::Action,
    label, node, now, policy, require,
};
use chrono::{DateTime, SecondsFormat, Utc};
use psios_api::{canonical, sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub const ACTION: &str = "schedule.manage";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    key: String,
    tool: String,
    input: Value,
    delegation_id: String,
    run_at: String,
    deadline_at: String,
}

fn instant(value: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value).map_err(|_| {
        err(
            "InvalidArgument",
            "schedule time requires an RFC 3339 offset",
        )
    })?;
    require(
        parsed.timestamp_subsec_nanos() < 1_000_000_000,
        "InvalidArgument",
        "schedule time requires a POSIX instant",
    )?;
    Ok(parsed.to_utc())
}

fn due_ms(at: DateTime<Utc>) -> i64 {
    // Round upward so the indexed lookup never fires before the specified instant.
    at.timestamp_millis() + i64::from(!at.timestamp_subsec_nanos().is_multiple_of(1_000_000))
}

fn reference(d: &Doc) -> Value {
    json!({"reference": d.reference(), "state": d.body})
}

pub fn create(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Create = decode(params)?;
    label(&p.key)?;
    policy::active(db, actor)?;
    let run = instant(&p.run_at)?;
    let deadline = instant(&p.deadline_at)?;
    let run_at = run.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let deadline_at = deadline.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let authority = db.meta("authority")?;
    let identity = sha256(&canonical(&json!([
        "psios/schedule/once/1",
        authority,
        actor,
        p.key
    ])));
    let id = format!("schedule-{}", &identity[7..]);
    let creation = sha256(&canonical(&json!([
        &p.tool,
        &p.input,
        &p.delegation_id,
        &run_at,
        &deadline_at,
    ])));
    if let Some(existing) = db.find(&id)? {
        policy::can(db, actor, "task.read")?;
        require(
            existing.kind == "Schedule"
                && existing.owner == actor
                && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "schedule key already bound to another request",
        )?;
        return Ok(reference(&existing));
    }
    let d = policy::delegation(db, actor, &p.delegation_id, ACTION)?;
    let g = db.of(d.body["grant_id"].as_str().unwrap(), "Grant")?;
    let scope_end = instant(d.body["valid_until"].as_str().unwrap())?
        .min(instant(g.spec["valid_until"].as_str().unwrap())?);
    require(
        Utc::now() < run && run < deadline && deadline <= scope_end,
        "InvalidArgument",
        "schedule requires a future run and deadline within its authorization lifetime",
    )?;
    let action = Action::parse(&p.tool, &p.input)?;
    let target = action.target(db, actor)?;
    action.audience(db, actor, &d.reference(), &target)?;
    db.cfg.check_admission_space()?;
    let pending: u32 =
        db.c.query_row("SELECT count(*) FROM pending_schedules", [], |r| r.get(0))?;
    require(
        pending < db.cfg.queue_limit,
        "QueueFull",
        "pending schedule capacity reached",
    )?;
    let trigger = sha256(&canonical(&json!([
        "psios/schedule/trigger/1",
        &id,
        "r1",
        &run_at
    ])));
    let trigger_id = format!("trigger-{}", &trigger[7..]);
    let request = broker::prepare_scheduled(
        db,
        actor,
        &json!({"tool": p.tool, "input": p.input, "key": trigger_id,
                "delegation_id": p.delegation_id, "deadline_at": deadline_at}),
        &id,
    )?;
    let schedule = db.create(
        "Schedule", &id, actor,
        json!({"creation_digest": creation, "request": request}),
        json!({"lifecycle": "Pending", "timezone": "UTC", "run_at": run_at, "deadline_at": deadline_at,
               "trigger_id": trigger_id, "task_receipt": null, "reason": null}),
    )?;
    db.c.execute(
        "INSERT INTO pending_schedules(schedule,run_ms,deadline_ms) VALUES(?1,?2,?3)",
        params![id, due_ms(run), due_ms(deadline)],
    )?;
    Ok(reference(&schedule))
}

pub fn get(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: ById = decode(p)?;
    policy::can(db, actor, "task.read")?;
    let schedule = db.of(&p.id, "Schedule")?;
    policy::owned(actor, &schedule)?;
    Ok(reference(&schedule))
}

pub fn cancel(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Conditional = decode(p)?;
    policy::can(db, actor, "task.submit")?;
    let schedule = db.of(&p.id, "Schedule")?;
    policy::owned(actor, &schedule)?;
    schedule.cas(&p.expected_resource_version)?;
    require(
        schedule.body["lifecycle"] == "Pending",
        "Conflict",
        "schedule already settled",
    )?;
    Ok(reference(&settle(db, &schedule, "Cancelled", None, None)?))
}

fn settle(
    db: &Db<'_>,
    schedule: &Doc,
    phase: &str,
    reason: Option<&str>,
    receipt: Option<Value>,
) -> Result<Doc> {
    require(
        db.c.execute(
            "DELETE FROM pending_schedules WHERE schedule=?1",
            [&schedule.id],
        )? == 1
            && schedule.body["lifecycle"] == "Pending",
        "RecoveryRequired",
        "schedule state disagrees with its pending entry",
    )?;
    let mut body = schedule.body.clone();
    body["lifecycle"] = json!(phase);
    body["settled_at"] = json!(now());
    body["reason"] = json!(reason);
    body["task_receipt"] = receipt.unwrap_or(Value::Null);
    db.status(schedule, body)
}

fn deferred(error: &Error) -> bool {
    error.0.code == "QueueFull"
        || (error.0.code == "QuotaExceeded" && error.0.retry_condition == "SameRequestAfterBackoff")
}

pub fn fire(db: &Db<'_>) -> Result<()> {
    let at = Utc::now().timestamp_millis();
    let expired: Option<String> = db.c.query_row(
        "SELECT schedule FROM pending_schedules WHERE deadline_ms<=?1 ORDER BY deadline_ms,schedule LIMIT 1",
        [at], |r| r.get(0),
    ).optional()?;
    let (id, expired) = if let Some(id) = expired {
        (id, true)
    } else {
        let due: Option<String> = db.c.query_row(
            "SELECT schedule FROM pending_schedules WHERE run_ms<=?1 ORDER BY run_ms,schedule LIMIT 1",
            [at], |r| r.get(0),
        ).optional()?;
        let Some(id) = due else { return Ok(()) };
        if !node::enabled(db)? {
            return Ok(());
        }
        (id, false)
    };
    match db.cfg.check_admission_space() {
        Err(e) if deferred(&e) => return Ok(()),
        result => result?,
    }
    let schedule = db.of(&id, "Schedule")?;
    if expired {
        settle(db, &schedule, "Expired", Some("DeadlineExceeded"), None)?;
        return Ok(());
    }
    let request = &schedule.spec["request"];
    let delegation = request["delegation_ref"]["resource_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "schedule delegation reference missing"))?;
    db.c.execute_batch("SAVEPOINT schedule_admission")?;
    let result = policy::delegation(db, &schedule.owner, delegation, ACTION)
        .and_then(|_| broker::submit_scheduled(db, &schedule.owner, request, &schedule.id));
    match result {
        Ok(receipt) => {
            db.c.execute_batch("RELEASE schedule_admission")?;
            settle(db, &schedule, "Triggered", None, Some(receipt))?;
        }
        Err(e) => {
            db.c.execute_batch("ROLLBACK TO schedule_admission; RELEASE schedule_admission")?;
            match e.0.code.as_str() {
                _ if deferred(&e) => return Ok(()),
                "DeadlineExceeded" => settle(db, &schedule, "Expired", Some(&e.0.code), None)?,
                "PermissionDenied"
                | "PreconditionFailed"
                | "Conflict"
                | "NotFound"
                | "QuotaExceeded"
                | "UnsupportedCapability" => {
                    settle(db, &schedule, "Rejected", Some(&e.0.code), None)?
                }
                _ => return Err(e),
            };
        }
    }
    Ok(())
}
