use super::{due_ms, instant, remove_pending, timestamp, trigger_id};
use crate::{
    Result,
    db::{Db, Doc},
    err, now, require,
};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(tag = "mode", deny_unknown_fields)]
enum Missed {
    Skip,
    BoundedCatchUp { limit: u32 },
}

#[derive(Deserialize, Serialize)]
enum Overlap {
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
                && match self.missed {
                    Missed::Skip => true,
                    Missed::BoundedCatchUp { limit } => (1..=8.min(self.count)).contains(&limit),
                },
            "InvalidArgument",
            "invalid interval, occurrence count or catch-up limit",
        )?;
        Ok(deadline + self.offset(self.count - 1))
    }

    fn offset(&self, count: u32) -> TimeDelta {
        TimeDelta::seconds(i64::from(self.interval_seconds) * i64::from(count))
    }

    pub(super) fn initial_state(
        &self,
        schedule: &str,
        run: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Value {
        json!({"lifecycle": "Pending", "timezone": "UTC", "repeat": self,
               "next": Occurrence::new(schedule, 1, run, deadline),
               "trigger_refs": [], "last_task_id": null, "reason": null})
    }
}

#[derive(Deserialize, Serialize)]
struct Occurrence {
    ordinal: u32,
    run_at: String,
    deadline_at: String,
    trigger_id: String,
}

impl Occurrence {
    fn new(schedule: &str, ordinal: u32, run: DateTime<Utc>, deadline: DateTime<Utc>) -> Self {
        let run_at = timestamp(run);
        Self {
            trigger_id: trigger_id(schedule, &run_at),
            ordinal,
            run_at,
            deadline_at: timestamp(deadline),
        }
    }
}

pub(super) struct Series {
    settings: Repeat,
    next: Occurrence,
    run: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

fn persisted<T: DeserializeOwned>(value: &Value) -> Result<T> {
    T::deserialize(value).map_err(|_| err("RecoveryRequired", "invalid stored interval schedule"))
}

impl Series {
    pub(super) fn load(schedule: &Doc) -> Result<Option<Self>> {
        let Some(settings) = schedule.spec.get("repeat") else {
            return Ok(None);
        };
        let settings: Repeat = persisted(settings)?;
        let next: Occurrence = persisted(&schedule.body["next"])?;
        let run = instant(&next.run_at)
            .map_err(|_| err("RecoveryRequired", "invalid stored trigger time"))?;
        let deadline = instant(&next.deadline_at)
            .map_err(|_| err("RecoveryRequired", "invalid stored trigger deadline"))?;
        require(
            schedule.body["lifecycle"] == "Pending"
                && (1..=settings.count).contains(&next.ordinal)
                && next.trigger_id == trigger_id(&schedule.id, &next.run_at)
                && settings.validate(deadline).is_ok()
                && run < deadline,
            "RecoveryRequired",
            "invalid pending interval schedule",
        )?;
        Ok(Some(Self {
            settings,
            next,
            run,
            deadline,
        }))
    }

    fn at_offset(&self, schedule: &str, count: u32) -> Occurrence {
        let offset = self.settings.offset(count);
        Occurrence::new(
            schedule,
            self.next.ordinal + count,
            self.run + offset,
            self.deadline + offset,
        )
    }

    pub(super) fn prepare(&self, request: &Value) -> Result<Value> {
        crate::broker::retime_scheduled(request, &self.next.trigger_id, &self.next.deadline_at)
    }

    pub(super) fn skip_outdated(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        now_ms: i64,
        expiry_only: bool,
    ) -> Result<bool> {
        let remaining = self.settings.count - self.next.ordinal + 1;
        let interval_ms = i64::from(self.settings.interval_seconds) * 1000;
        let elapsed = |start| {
            if now_ms < start {
                0
            } else {
                (1 + (now_ms - start) / interval_ms).min(i64::from(remaining)) as u32
            }
        };
        let expired = elapsed(due_ms(self.deadline));
        let (keep, reason) = match self.settings.missed {
            Missed::Skip => (1, "MissedTrigger"),
            Missed::BoundedCatchUp { limit } => (limit, "CatchUpLimit"),
        };
        let superseded = if expiry_only {
            0
        } else {
            elapsed(due_ms(self.run)).saturating_sub(keep)
        };
        let count = expired.max(superseded);
        if count == 0 {
            require(
                !expiry_only,
                "RecoveryRequired",
                "interval schedule deadline index mismatch",
            )?;
            return Ok(false);
        }
        let mut body = schedule.body.clone();
        for offset in 0..count {
            let (phase, reason) = if offset < expired {
                ("Expired", "DeadlineExceeded")
            } else {
                ("Skipped", reason)
            };
            record(
                db,
                schedule,
                &mut body,
                &self.at_offset(&schedule.id, offset),
                phase,
                Some(reason),
                None,
            )?;
        }
        self.advance(db, schedule, body, count, None)?;
        Ok(true)
    }

    pub(super) fn overlapping(&self, db: &Db<'_>, schedule: &Doc) -> Result<bool> {
        if matches!(self.settings.overlap, Overlap::Allow) {
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
        count: u32,
        rejection: Option<&str>,
    ) -> Result<()> {
        if self.next.ordinal + count <= self.settings.count && rejection.is_none() {
            let next = self.at_offset(&schedule.id, count);
            let offset = self.settings.offset(count);
            require(
                db.c.execute(
                    "UPDATE pending_schedules SET run_ms=?2,deadline_ms=?3 WHERE schedule=?1 AND run_ms=?4 AND deadline_ms=?5",
                    params![schedule.id, due_ms(self.run + offset), due_ms(self.deadline + offset),
                            due_ms(self.run), due_ms(self.deadline)],
                )? == 1,
                "RecoveryRequired",
                "interval schedule index disagrees with trigger",
            )?;
            body["next"] = json!(next);
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
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "interval schedule trigger references missing",
            )
        })?
        .push(trigger.reference());
    Ok(())
}
