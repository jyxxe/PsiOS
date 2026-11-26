//! Durable admission into the ordinary Task broker.
pub(crate) mod admission_delay;
mod calendar;
mod repeat;
mod resource_event;
pub(crate) mod workflow_overlap;

use crate::{
    ById, Conditional, Error, Result, Store, broker,
    db::{Db, Doc},
    decode, err,
    host::Action,
    label, node, now, policy, require,
};
use chrono::{DateTime, SecondsFormat, Utc};
use psios_api::{canonical, id, sha256};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const ACTION: &str = "schedule.manage";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    key: String,
    tool: String,
    input: Value,
    delegation_id: String,
    run_at: Option<String>,
    deadline_at: Option<String>,
    repeat: Option<repeat::Repeat>,
    calendar: Option<calendar::Calendar>,
    event: Option<resource_event::Rule>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Window {
    run_at: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
}

enum Timing<'a> {
    Fixed {
        first: Window,
        last_deadline: DateTime<Utc>,
    },
    Calendar(&'a calendar::Calendar),
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

fn timestamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn trigger_id(schedule: &str, run_at: &str) -> String {
    let digest = sha256(&canonical(&json!([
        "psios/schedule/trigger/1",
        schedule,
        "r1",
        run_at
    ])));
    format!("trigger-{}", &digest[7..])
}

fn reference(d: &Doc) -> Value {
    json!({"reference": d.reference(), "state": d.body})
}

fn schedule_id(db: &Db<'_>, actor: &str, key: &str) -> Result<String> {
    let identity = sha256(&canonical(&json!([
        "psios/schedule/once/1",
        db.meta("authority")?,
        actor,
        key
    ])));
    Ok(format!("schedule-{}", &identity[7..]))
}

fn existing(db: &Db<'_>, actor: &str, id: &str, creation: &str) -> Result<Option<Value>> {
    let Some(schedule) = db.find(id)? else {
        return Ok(None);
    };
    policy::can(db, actor, "task.read")?;
    require(
        schedule.kind == "Schedule"
            && schedule.owner == actor
            && schedule.spec["creation_digest"] == creation,
        "IdempotencyConflict",
        "schedule key already bound to another request",
    )?;
    Ok(Some(reference(&schedule)))
}

fn scope_end(db: &Db<'_>, delegation: &Doc) -> Result<DateTime<Utc>> {
    let grant = db.of(delegation.body["grant_id"].as_str().unwrap(), "Grant")?;
    Ok(instant(delegation.body["valid_until"].as_str().unwrap())?
        .min(instant(grant.spec["valid_until"].as_str().unwrap())?))
}

fn prepare_template(
    db: &Db<'_>,
    actor: &str,
    p: &Create,
    delegation: &Doc,
    schedule: &str,
    trigger: &str,
    deadline: &str,
) -> Result<Value> {
    let action = Action::parse(&p.tool, &p.input)?;
    let target = action.target(db, actor)?;
    action.audience(db, actor, &delegation.reference(), &target)?;
    db.cfg.check_admission_space()?;
    let pending: u32 = db.c.query_row(
        "SELECT (SELECT count(*) FROM pending_schedules)
              + (SELECT count(*) FROM pending_event_schedules)",
        [],
        |r| r.get(0),
    )?;
    require(
        pending < db.cfg.queue_limit,
        "QueueFull",
        "pending schedule capacity reached",
    )?;
    broker::prepare_scheduled(
        db,
        actor,
        &json!({"tool":p.tool,"input":p.input,"key":trigger,
                "delegation_id":p.delegation_id,"deadline_at":deadline}),
        schedule,
    )
}

pub fn create(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Create = decode(params)?;
    label(&p.key)?;
    policy::active(db, actor)?;
    let workflow_overlap = p
        .repeat
        .as_ref()
        .is_some_and(|series| series.overlap.workflow())
        || p.calendar
            .as_ref()
            .is_some_and(|series| series.overlap.workflow());
    require(
        !workflow_overlap || p.tool == crate::workflow::START_TOOL,
        "InvalidArgument",
        "SkipWhileWorkflowActive requires a workflow.start schedule",
    )?;
    if let Some(event) = &p.event {
        require(
            p.run_at.is_none()
                && p.deadline_at.is_none()
                && p.repeat.is_none()
                && p.calendar.is_none(),
            "InvalidArgument",
            "event schedules use their own expiration",
        )?;
        return resource_event::create(db, actor, &p, event);
    }
    let mut definition = vec![json!(p.tool), p.input.clone(), json!(p.delegation_id)];
    let timing = match (
        p.run_at.as_deref(),
        p.deadline_at.as_deref(),
        &p.repeat,
        &p.calendar,
    ) {
        (Some(run), Some(deadline), repeat, None) => {
            let first = Window {
                run_at: instant(run)?,
                deadline_at: instant(deadline)?,
            };
            let last_deadline = repeat
                .as_ref()
                .map(|rules| rules.validate(first.deadline_at))
                .transpose()?
                .unwrap_or(first.deadline_at);
            // Keep the persisted digest of existing one-shot and interval requests.
            definition.extend([
                json!(timestamp(first.run_at)),
                json!(timestamp(first.deadline_at)),
            ]);
            if let Some(repeat) = repeat {
                definition.push(json!(repeat));
            }
            Timing::Fixed {
                first,
                last_deadline,
            }
        }
        (None, None, None, Some(calendar)) => {
            definition.push(json!({"calendar": calendar}));
            Timing::Calendar(calendar)
        }
        _ => {
            return Err(err(
                "InvalidArgument",
                "provide run_at and deadline_at with optional repeat, or a calendar",
            ));
        }
    };
    let id = schedule_id(db, actor, &p.key)?;
    let creation = sha256(&canonical(&Value::Array(definition)));
    if let Some(record) = existing(db, actor, &id, &creation)? {
        return Ok(record);
    }
    let d = policy::delegation(db, actor, &p.delegation_id, ACTION)?;
    let scope_end = scope_end(db, &d)?;
    let (first, last_deadline, mut spec, body) = match timing {
        Timing::Fixed {
            first,
            last_deadline,
        } => {
            let mut spec = json!({});
            let body = if let Some(repeat) = &p.repeat {
                spec["repeat"] = json!(repeat);
                repeat.initial_state(&id, first)
            } else {
                let run_at = timestamp(first.run_at);
                json!({"lifecycle": "Pending", "timezone": "UTC", "run_at": run_at,
                       "deadline_at": timestamp(first.deadline_at), "trigger_id": trigger_id(&id, &run_at),
                       "task_receipt": null, "reason": null})
            };
            (first, last_deadline, spec, body)
        }
        Timing::Calendar(calendar) => {
            // Resolve local times once. Existing revisions retain these UTC instants
            // across process restarts and changes to the bundled IANA database.
            let plan = calendar.plan()?;
            let first = plan.windows[0];
            let last_deadline = plan.windows[plan.windows.len() - 1].deadline_at;
            let mut body = repeat::initial_state(&id, &calendar.timezone, first);
            body["calendar"] = json!(calendar);
            body["calendar_plan"] = json!(plan);
            (first, last_deadline, json!({"calendar": calendar}), body)
        }
    };
    require(
        Utc::now() < first.run_at && first.run_at < first.deadline_at && last_deadline <= scope_end,
        "InvalidArgument",
        "schedule requires a future run and deadline within its authorization lifetime",
    )?;
    let trigger_id = trigger_id(&id, &timestamp(first.run_at));
    let request = prepare_template(
        db,
        actor,
        &p,
        &d,
        &id,
        &trigger_id,
        &timestamp(first.deadline_at),
    )?;
    spec["creation_digest"] = json!(creation);
    spec["request"] = request;
    let schedule = db.create("Schedule", &id, actor, spec, body)?;
    policy::deadline::Clock::sample()?.register_until(
        db,
        &id,
        &timestamp(first.deadline_at),
        None,
    )?;
    db.c.execute(
        "INSERT INTO pending_schedules(schedule,run_ms,deadline_ms) VALUES(?1,?2,?3)",
        params![id, due_ms(first.run_at), due_ms(first.deadline_at)],
    )?;
    Ok(reference(&schedule))
}

pub fn get(db: &Db<'_>, actor: &str, p: &Value, kind: &str) -> Result<Value> {
    let p: ById = decode(p)?;
    policy::can(db, actor, "task.read")?;
    let schedule = db.of(&p.id, kind)?;
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
    let mut body = schedule.body.clone();
    resource_event::settle_pending(db, schedule, phase, reason, &mut body)?;
    remove_pending(db, schedule)?;
    body["lifecycle"] = json!(phase);
    body["settled_at"] = json!(now());
    body["reason"] = json!(reason);
    if body.get("next").is_some() {
        body["next"] = Value::Null;
    } else if schedule.spec["event"].is_null() {
        body["task_receipt"] = receipt.unwrap_or(Value::Null);
    }
    db.status(schedule, body)
}

fn remove_pending(db: &Db<'_>, schedule: &Doc) -> Result<()> {
    let sql = if schedule.spec["event"].is_object() {
        "DELETE FROM pending_event_schedules WHERE schedule=?1"
    } else {
        "DELETE FROM pending_schedules WHERE schedule=?1"
    };
    require(
        schedule.body["lifecycle"] == "Pending" && db.c.execute(sql, [&schedule.id])? == 1,
        "RecoveryRequired",
        "schedule state disagrees with its pending entry",
    )?;
    admission_delay::clear(db, &schedule.id)
}

fn deferred(error: &Error) -> bool {
    error.0.code == "QueueFull"
        || (error.0.code == "QuotaExceeded" && error.0.retry_condition == "SameRequestAfterBackoff")
}

fn previous_task_active(db: &Db<'_>, schedule: &Doc) -> Result<bool> {
    let Some(id) = schedule.body["last_task_id"].as_str() else {
        return Ok(false);
    };
    let task = db.of(id, "Task")?;
    Ok(!matches!(
        task.body["lifecycle"].as_str(),
        Some("Succeeded" | "Failed" | "Cancelled")
    ) || task.body["outcome_knowledge"] == "Unknown")
}

struct Attempt {
    id: String,
    version: String,
    run_ms: i64,
    trigger: String,
}

pub fn tick(store: &mut Store) -> Result<()> {
    tick_timed(store)?;
    resource_event::tick(store)
}

fn tick_timed(store: &mut Store) -> Result<()> {
    let correlation = id("schedule-tick");
    let mut attempt = None;
    match store.transaction(&correlation, |db| fire(db, &mut attempt)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let Some(attempt) = attempt else {
                return Err(error);
            };
            // The attempted admission has rolled back. The caller still holds
            // the authority lock while the same attempt is deferred or settled.
            if deferred(&error) {
                return admission_delay::after_refusal(store, &correlation, |db| {
                    let schedule = db.of(&attempt.id, "Schedule")?;
                    schedule.cas(&attempt.version)?;
                    admission_delay::defer(
                        db,
                        &schedule,
                        admission_delay::Kind::Timed,
                        attempt.run_ms,
                        &attempt.trigger,
                    )
                });
            }
            let phase = match error.0.code.as_str() {
                "DeadlineExceeded" => "Expired",
                "PermissionDenied"
                | "PreconditionFailed"
                | "Conflict"
                | "NotFound"
                | "QuotaExceeded"
                | "UnsupportedCapability" => "Rejected",
                _ => return Err(error),
            };
            store.transaction(&correlation, |db| {
                let schedule = db.of(&attempt.id, "Schedule")?;
                schedule.cas(&attempt.version)?;
                if let Some(series) = repeat::Series::load(&schedule)? {
                    series.settle(db, &schedule, phase, Some(&error.0.code), None)?;
                } else {
                    settle(db, &schedule, phase, Some(&error.0.code), None)?;
                }
                Ok(())
            })
        }
    }
}

fn fire(db: &Db<'_>, attempt: &mut Option<Attempt>) -> Result<()> {
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_schedules)",
        [],
        |row| row.get(0),
    )?;
    if !pending {
        return Ok(());
    }
    let clock = policy::deadline::Clock::sample()?;
    let at = Utc::now().timestamp_millis();
    let expired: Option<(String, i64)> = db.c.query_row(
        "WITH checked AS (
            SELECT p.schedule,p.deadline_ms,
                CASE WHEN elapsed.resource IS NULL THEN ?1
                     WHEN elapsed.time_ns!=?3 THEN ?5
                     ELSE max(?1,p.deadline_ms+(?4-elapsed.deadline_boot_ms))
                END AS through
            FROM pending_schedules p LEFT JOIN authorization_deadlines elapsed
                 ON elapsed.resource=p.schedule AND elapsed.boot_id=?2
        )
        SELECT schedule,through FROM checked WHERE deadline_ms<=through ORDER BY deadline_ms,schedule LIMIT 1",
        params![at,clock.boot.boot_id,clock.time_ns,clock.boot.millis,i64::MAX], |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    let (id, expired_through, run_ms) = if let Some((id, through)) = expired {
        (id, Some(through), None)
    } else {
        if !node::enabled(db)? || node::work_admission::draining(db)? {
            return Ok(());
        }
        let due =
            db.c.query_row(
                "SELECT p.schedule,p.run_ms FROM pending_schedules p
              LEFT JOIN schedule_admission_deferrals d
                ON d.schedule=p.schedule AND d.kind='Timed' AND d.anchor=p.run_ms
             WHERE p.run_ms<=?1 AND (d.boot_id IS NULL OR d.boot_id!=?2 OR d.time_ns!=?3
                OR d.deferred_boot_ms NOT BETWEEN ?4 AND ?5)
             ORDER BY coalesce(d.ready_order,p.run_ms),p.run_ms,p.schedule LIMIT 1",
                params![
                    at,
                    clock.boot.boot_id,
                    clock.time_ns,
                    admission_delay::cutoff(clock.boot.millis),
                    clock.boot.millis
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((id, run_ms)) = due else {
            return Ok(());
        };
        (id, None, Some(run_ms))
    };
    let schedule = db.of(&id, "Schedule")?;
    let series = repeat::Series::load(&schedule)?;
    if let Some(series) = &series {
        if series.skip_outdated(db, &schedule, at, expired_through)? {
            return Ok(());
        }
        if series.skip_overlap(db, &schedule)? {
            return Ok(());
        }
    } else if expired_through.is_some() {
        settle(db, &schedule, "Expired", Some("DeadlineExceeded"), None)?;
        return Ok(());
    }
    let template = &schedule.spec["request"];
    let timed_request;
    let request = if let Some(series) = &series {
        timed_request = series.prepare(template)?;
        &timed_request
    } else {
        template
    };
    let delegation = request["delegation_ref"]["resource_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "schedule delegation reference missing"))?;
    let trigger = request["idempotency_key"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "timed schedule trigger identity missing",
        )
    })?;
    let run_ms =
        run_ms.ok_or_else(|| err("RecoveryRequired", "expired occurrence was dispatched"))?;
    let result = policy::delegation(db, &schedule.owner, delegation, ACTION)
        .and_then(|_| broker::submit_scheduled(db, &schedule.owner, request, &schedule.id));
    let receipt = result.inspect_err(|_| {
        *attempt = Some(Attempt {
            version: schedule.version(),
            id: schedule.id.clone(),
            run_ms,
            trigger: trigger.to_owned(),
        });
    })?;
    if let Some(series) = series {
        series.settle(db, &schedule, "Triggered", None, Some(receipt))
    } else {
        settle(db, &schedule, "Triggered", None, Some(receipt)).map(|_| ())
    }
}
