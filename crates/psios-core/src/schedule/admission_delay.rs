//! Bounded retries that retain the original time window or source event.
use super::{due_ms, instant, resource_event, trigger_id};
use crate::{
    Result, Store,
    db::{Db, Doc, find, resolve, row},
    err, policy, require,
};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const FORMAT: u32 = crate::agent::session_inputs::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("admission_delay.sql");
const BACKOFF_MS: i64 = 1_000;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Timed,
    ResourceEvent,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Timed => "Timed",
            Self::ResourceEvent => "ResourceEvent",
        }
    }
}

pub(super) fn cutoff(boot_ms: i64) -> i64 {
    boot_ms - BACKOFF_MS + 1
}

fn checked_time(value: &serde_json::Value) -> Result<i64> {
    let at = value
        .as_str()
        .and_then(|time| instant(time).ok())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "deferred schedule contains an invalid time",
            )
        })?;
    Ok(due_ms(at))
}

fn check_timed(conn: &Connection, schedule: &Doc, anchor: i64, trigger: &str) -> Result<()> {
    let pending: Option<(i64, i64)> = conn
        .query_row(
            "SELECT run_ms,deadline_ms FROM pending_schedules WHERE schedule=?1",
            [&schedule.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let occurrence = schedule.body.get("next").unwrap_or(&schedule.body);
    let run = checked_time(&occurrence["run_at"])?;
    let deadline = checked_time(&occurrence["deadline_at"])?;
    let run_at = occurrence["run_at"].as_str().unwrap_or_default();
    require(
        pending == Some((run, deadline))
            && run == anchor
            && occurrence["trigger_id"] == trigger
            && trigger_id(&schedule.id, run_at) == trigger,
        "RecoveryRequired",
        "timed admission delay disagrees with the current occurrence",
    )
}

fn check_event(conn: &Connection, schedule: &Doc, anchor: i64, trigger: &str) -> Result<()> {
    let source = conn
        .query_row(
            "SELECT v.* FROM pending_event_schedules p JOIN versions v
                ON v.id=p.source AND v.rev=p.source_rev+1 WHERE p.schedule=?1 AND v.seq=?2",
            params![schedule.id, anchor],
            |r| Ok((row(r)?, r.get::<_, String>("event_id")?)),
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "deferred event does not follow its source cursor",
            )
        })?;
    let (source, event_id) = source;
    require(
        schedule.spec["event"]["source_ref"]["resource_id"] == source.id
            && resource_event::trigger_id(schedule, &event_id) == trigger,
        "RecoveryRequired",
        "deferred event identity differs from its schedule",
    )?;
    if !schedule.body["pending_trigger_ref"].is_null() {
        let pending = resolve(conn, &schedule.body["pending_trigger_ref"])?;
        let source_seq = anchor.to_string();
        require(
            pending.kind == "ScheduleTrigger"
                && pending.owner == schedule.owner
                && pending.id == trigger
                && pending.spec["schedule_id"] == schedule.id
                && pending.spec["creation_digest"] == schedule.spec["creation_digest"]
                && pending.body["lifecycle"] == "Pending"
                && find(conn, &pending.id)?.is_some_and(|current| current.rev == pending.rev)
                && pending.body["task_receipt"].is_null()
                && pending.body["source_event"]["event_id"] == event_id
                && pending.body["source_event"]["source_seq"] == source_seq.as_str()
                && pending.body["source_event"]["resource_ref"] == source.reference(),
            "RecoveryRequired",
            "event admission delay disagrees with its original pending trigger",
        )?;
    }
    Ok(())
}

fn check_anchor(
    conn: &Connection,
    schedule: &Doc,
    kind: Kind,
    anchor: i64,
    trigger: &str,
) -> Result<()> {
    require(
        schedule.kind == "Schedule"
            && schedule.body["lifecycle"] == "Pending"
            && schedule.spec["event"].is_object() == matches!(kind, Kind::ResourceEvent),
        "RecoveryRequired",
        "admission delay has no matching pending schedule",
    )?;
    match kind {
        Kind::Timed => check_timed(conn, schedule, anchor, trigger),
        Kind::ResourceEvent => check_event(conn, schedule, anchor, trigger),
    }
}

pub(super) fn defer(
    db: &Db<'_>,
    schedule: &Doc,
    kind: Kind,
    anchor: i64,
    trigger: &str,
) -> Result<()> {
    check_anchor(db.c, schedule, kind, anchor, trigger)?;
    let clock = policy::deadline::Clock::sample()?;
    // Timed schedules are ordered by Unix milliseconds and event schedules
    // by authoritative sequence. Requeue after the current cohort without
    // mutating either schedule's original eligibility or trigger identity.
    let current = match kind {
        Kind::Timed => Utc::now().timestamp_millis(),
        Kind::ResourceEvent => {
            db.c.query_row("SELECT max(seq) FROM versions", [], |r| r.get::<_, i64>(0))?
        }
    };
    let previous: Option<i64> = db.c.query_row(
        "SELECT max(ready_order) FROM schedule_admission_deferrals WHERE kind=?1",
        [kind.name()],
        |row| row.get(0),
    )?;
    let order = current
        .max(anchor)
        .max(previous.unwrap_or(anchor))
        .checked_add(1)
        .ok_or_else(|| err("RecoveryRequired", "schedule admission order exhausted"))?;
    db.c.execute(
        "INSERT INTO schedule_admission_deferrals
            (schedule,schedule_rev,kind,anchor,trigger,ready_order,boot_id,time_ns,deferred_boot_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(schedule) DO UPDATE SET schedule_rev=excluded.schedule_rev,kind=excluded.kind,
            anchor=excluded.anchor,trigger=excluded.trigger,ready_order=excluded.ready_order,
            boot_id=excluded.boot_id,time_ns=excluded.time_ns,deferred_boot_ms=excluded.deferred_boot_ms",
        params![schedule.id,schedule.rev,kind.name(),anchor,trigger,order,
                clock.boot.boot_id,clock.time_ns,clock.boot.millis],
    )?;
    Ok(())
}

pub(super) fn clear(db: &Db<'_>, schedule_id: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM schedule_admission_deferrals WHERE schedule=?1",
        [schedule_id],
    )?;
    Ok(())
}

pub(super) fn after_refusal(
    store: &mut Store,
    correlation: &str,
    action: impl FnOnce(&Db<'_>) -> Result<()>,
) -> Result<()> {
    match store.transaction(correlation, action) {
        Err(error) if error.0.code == "StorageUnavailable" && !store.transaction_is_uncertain() => {
            eprintln!("psiosd schedule admission deferral: {}", error.0.code);
            Ok(())
        }
        result => result,
    }
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut query = conn.prepare(
        "SELECT schedule,schedule_rev,kind,anchor,trigger,boot_id,time_ns
           FROM schedule_admission_deferrals",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let rev: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let anchor: i64 = row.get(3)?;
        let trigger: String = row.get(4)?;
        let boot_id: String = row.get(5)?;
        let time_ns: String = row.get(6)?;
        let kind = match kind.as_str() {
            "Timed" => Kind::Timed,
            "ResourceEvent" => Kind::ResourceEvent,
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "unrecognized schedule admission delay",
                ));
            }
        };
        let schedule = find(conn, &id)?
            .ok_or_else(|| err("RecoveryRequired", "deferred schedule no longer exists"))?;
        require(
            schedule.rev == rev
                && uuid::Uuid::parse_str(&boot_id).is_ok_and(|boot| boot.to_string() == boot_id)
                && (time_ns == "unsupported"
                    || time_ns
                        .parse::<u64>()
                        .is_ok_and(|ns| ns != 0 && ns.to_string() == time_ns)),
            "RecoveryRequired",
            "schedule admission delay disagrees with the current revision or clock scope",
        )?;
        check_anchor(conn, &schedule, kind, anchor, &trigger)?;
    }
    Ok(())
}
