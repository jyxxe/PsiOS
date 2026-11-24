use super::{
    Window,
    calendar::{Calendar, CalendarPlan},
    due_ms, instant, remove_pending, timestamp, trigger_id,
};
use crate::{
    Result,
    db::{Db, Doc},
    err, now, require,
};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(tag = "mode", deny_unknown_fields)]
pub(super) enum Missed {
    Skip,
    BoundedCatchUp { limit: u32 },
}

impl Missed {
    pub(super) fn valid(&self, count: u32) -> bool {
        match self {
            Self::Skip => true,
            Self::BoundedCatchUp { limit } => (1..=8.min(count)).contains(limit),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum Overlap {
    Allow,
    Skip,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Repeat {
    interval_seconds: u32,
    count: u32,
    missed: Missed,
    overlap: Overlap,
}

impl Repeat {
    pub(super) fn validate(&self, deadline: DateTime<Utc>) -> Result<DateTime<Utc>> {
        require(
            (1..=86400).contains(&self.interval_seconds)
                && (2..=32).contains(&self.count)
                && self.missed.valid(self.count),
            "InvalidArgument",
            "invalid interval, occurrence count or catch-up limit",
        )?;
        Ok(deadline + self.offset(self.count - 1))
    }

    fn offset(&self, count: u32) -> TimeDelta {
        TimeDelta::seconds(i64::from(self.interval_seconds) * i64::from(count))
    }

    pub(super) fn initial_state(&self, schedule: &str, first: Window) -> Value {
        let mut state = initial_state(schedule, "UTC", first);
        state["repeat"] = json!(self);
        state
    }
}

pub(super) fn initial_state(schedule: &str, timezone: &str, first: Window) -> Value {
    json!({"lifecycle": "Pending", "timezone": timezone, "next": Occurrence::new(schedule, 1, first),
           "trigger_refs": [], "last_task_id": null, "reason": null})
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Occurrence {
    ordinal: u32,
    run_at: String,
    deadline_at: String,
    trigger_id: String,
}

impl Occurrence {
    fn new(schedule: &str, ordinal: u32, window: Window) -> Self {
        let run_at = timestamp(window.run_at);
        Self {
            trigger_id: trigger_id(schedule, &run_at),
            ordinal,
            run_at,
            deadline_at: timestamp(window.deadline_at),
        }
    }
}

pub(super) struct Series {
    missed: Missed,
    overlap: Overlap,
    next: Occurrence,
    upcoming: Vec<Window>,
}

fn persisted<T: DeserializeOwned>(value: &Value) -> Result<T> {
    T::deserialize(value).map_err(|_| err("RecoveryRequired", "invalid stored schedule series"))
}

impl Series {
    pub(super) fn load(schedule: &Doc) -> Result<Option<Self>> {
        let repeat = schedule.spec.get("repeat");
        let calendar = schedule.spec.get("calendar");
        if repeat.is_none() && calendar.is_none() {
            return Ok(None);
        }
        let next: Occurrence = persisted(&schedule.body["next"])?;
        let (missed, overlap, upcoming) = match (repeat, calendar) {
            (Some(repeat), None) => {
                let rules: Repeat = persisted(repeat)?;
                let run = instant(&next.run_at)
                    .map_err(|_| err("RecoveryRequired", "invalid stored trigger time"))?;
                let deadline = instant(&next.deadline_at)
                    .map_err(|_| err("RecoveryRequired", "invalid stored trigger deadline"))?;
                require(
                    (1..=rules.count).contains(&next.ordinal)
                        && rules.validate(deadline).is_ok()
                        && run < deadline,
                    "RecoveryRequired",
                    "invalid pending interval schedule",
                )?;
                let windows = (0..=rules.count - next.ordinal)
                    .map(|index| {
                        let delta = rules.offset(index);
                        Window {
                            run_at: run + delta,
                            deadline_at: deadline + delta,
                        }
                    })
                    .collect();
                (rules.missed, rules.overlap, windows)
            }
            (None, Some(calendar)) => {
                let rules: Calendar = persisted(calendar)?;
                let plan: CalendarPlan = persisted(&schedule.body["calendar_plan"])?;
                rules.validate_plan(&plan)?;
                require(
                    (1..=plan.windows.len() as u32).contains(&next.ordinal),
                    "RecoveryRequired",
                    "calendar trigger ordinal is outside its pinned plan",
                )?;
                let windows = plan.windows[(next.ordinal as usize - 1)..].to_vec();
                (rules.missed, rules.overlap, windows)
            }
            _ => return Err(err("RecoveryRequired", "ambiguous stored schedule series")),
        };
        require(
            schedule.body["lifecycle"] == "Pending"
                && next == Occurrence::new(&schedule.id, next.ordinal, upcoming[0]),
            "RecoveryRequired",
            "pending schedule disagrees with its next trigger",
        )?;
        Ok(Some(Self {
            missed,
            overlap,
            next,
            upcoming,
        }))
    }

    fn occurrence(&self, schedule: &str, offset: usize, window: Window) -> Occurrence {
        Occurrence::new(schedule, self.next.ordinal + offset as u32, window)
    }

    pub(super) fn prepare(&self, request: &Value) -> Result<Value> {
        crate::broker::retime_scheduled(request, &self.next.trigger_id, &self.next.deadline_at)
    }

    pub(super) fn skip_outdated(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        now_ms: i64,
        expired_through: Option<i64>,
    ) -> Result<bool> {
        let expired = self
            .upcoming
            .iter()
            .take_while(|window| due_ms(window.deadline_at) <= expired_through.unwrap_or(now_ms))
            .count();
        let (keep, reason) = match self.missed {
            Missed::Skip => (1, "MissedTrigger"),
            Missed::BoundedCatchUp { limit } => (limit as usize, "CatchUpLimit"),
        };
        let superseded = if expired_through.is_some() {
            0
        } else {
            self.upcoming
                .iter()
                .take_while(|window| due_ms(window.run_at) <= now_ms)
                .count()
                .saturating_sub(keep)
        };
        let count = expired.max(superseded);
        if count == 0 {
            require(
                expired_through.is_none(),
                "RecoveryRequired",
                "schedule deadline index mismatch",
            )?;
            return Ok(false);
        }
        let mut body = schedule.body.clone();
        for (offset, window) in self.upcoming.iter().copied().take(count).enumerate() {
            let (phase, reason) = if offset < expired {
                ("Expired", "DeadlineExceeded")
            } else {
                ("Skipped", reason)
            };
            record(
                db,
                schedule,
                &mut body,
                &self.occurrence(&schedule.id, offset, window),
                phase,
                Some(reason),
                None,
            )?;
        }
        self.advance(db, schedule, body, count, None)?;
        Ok(true)
    }

    pub(super) fn overlapping(&self, db: &Db<'_>, schedule: &Doc) -> Result<bool> {
        if matches!(self.overlap, Overlap::Allow) {
            return Ok(false);
        }
        let Some(id) = schedule.body["last_task_id"].as_str() else {
            return Ok(false);
        };
        let task = db.of(id, "Task")?;
        Ok(!matches!(
            task.body["lifecycle"].as_str(),
            Some("Succeeded" | "Failed" | "Cancelled")
        ) || task.body["outcome_knowledge"] == "Unknown")
    }

    pub(super) fn settle(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        phase: &str,
        reason: Option<&str>,
        receipt: Option<Value>,
    ) -> Result<()> {
        let mut body = schedule.body.clone();
        record(db, schedule, &mut body, &self.next, phase, reason, receipt)?;
        let rejection = if phase == "Rejected" { reason } else { None };
        self.advance(db, schedule, body, 1, rejection)
    }

    fn advance(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        mut body: Value,
        count: usize,
        rejection: Option<&str>,
    ) -> Result<()> {
        if let Some(next) = self
            .upcoming
            .get(count)
            .copied()
            .filter(|_| rejection.is_none())
        {
            let current = self.upcoming[0];
            require(
                db.c.execute(
                    "UPDATE pending_schedules SET run_ms=?2,deadline_ms=?3 WHERE schedule=?1 AND run_ms=?4 AND deadline_ms=?5",
                    params![schedule.id, due_ms(next.run_at), due_ms(next.deadline_at),
                            due_ms(current.run_at), due_ms(current.deadline_at)],
                )? == 1,
                "RecoveryRequired",
                "schedule index disagrees with trigger",
            )?;
            let delta_ms = due_ms(next.deadline_at) - due_ms(current.deadline_at);
            // Move the stored cutoff by the original interval between windows.
            db.c.execute(
                "UPDATE authorization_deadlines SET deadline_boot_ms=deadline_boot_ms+?2 WHERE resource=?1",
                params![schedule.id, delta_ms],
            )?;
            body["next"] = json!(self.occurrence(&schedule.id, count, next));
        } else {
            remove_pending(db, schedule)?;
            body["lifecycle"] = json!(if rejection.is_some() {
                "Rejected"
            } else {
                "Completed"
            });
            body["next"] = Value::Null;
            body["settled_at"] = json!(now());
            body["reason"] = json!(rejection);
        }
        db.status(schedule, body)?;
        Ok(())
    }
}

fn record(
    db: &Db<'_>,
    schedule: &Doc,
    body: &mut Value,
    timing: &Occurrence,
    phase: &str,
    reason: Option<&str>,
    receipt: Option<Value>,
) -> Result<()> {
    if let Some(receipt) = &receipt {
        body["last_task_id"] = receipt["task_id"].clone();
    }
    let trigger = db.create(
        "ScheduleTrigger",
        &timing.trigger_id,
        &schedule.owner,
        json!({"schedule_id": schedule.id, "definition_revision": "r1", "timing": timing}),
        json!({"schedule_id": schedule.id, "ordinal": timing.ordinal, "run_at": timing.run_at,
               "deadline_at": timing.deadline_at, "lifecycle": phase, "reason": reason,
               "task_receipt": receipt, "settled_at": now()}),
    )?;
    body["trigger_refs"]
        .as_array_mut()
        .ok_or_else(|| err("RecoveryRequired", "schedule trigger references missing"))?
        .push(trigger.reference());
    Ok(())
}
