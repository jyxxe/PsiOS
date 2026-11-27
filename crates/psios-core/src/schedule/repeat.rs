use super::{
    Window, admission_delay,
    calendar::{Calendar, CalendarPlan},
    coalescence, due_ms, instant, remove_pending, timestamp, trigger_id, workflow_overlap,
};
use crate::{
    Result,
    db::{Db, Doc},
    err, now, policy, require,
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
    Coalesce,
}

impl Missed {
    pub(super) fn valid(&self, count: u32) -> bool {
        match self {
            Self::Skip | Self::Coalesce => true,
            Self::BoundedCatchUp { limit } => (1..=8.min(count)).contains(limit),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
pub(super) enum Overlap {
    Allow,
    Skip,
    SkipWhileWorkflowActive,
}

impl Overlap {
    pub(super) fn workflow(self) -> bool {
        matches!(self, Self::SkipWhileWorkflowActive)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Repeat {
    interval_seconds: u32,
    pub(super) count: u32,
    missed: Missed,
    pub(super) overlap: Overlap,
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
pub(super) struct Occurrence {
    pub(super) ordinal: u32,
    pub(super) run_at: String,
    pub(super) deadline_at: String,
    pub(super) trigger_id: String,
}

impl Occurrence {
    pub(super) fn new(schedule: &str, ordinal: u32, window: Window) -> Self {
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
    pub(super) upcoming: Vec<Window>,
}

pub(super) struct Selection {
    offset: usize,
    representative: Occurrence,
    pub(super) anchor_trigger: String,
    proof: Option<coalescence::Proof>,
}

pub(super) fn persisted<T: DeserializeOwned>(value: &Value) -> Result<T> {
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

    pub(super) fn select(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        now_ms: i64,
        clock: &policy::deadline::Clock,
    ) -> Result<Selection> {
        let (offset, proof) = if matches!(self.missed, Missed::Coalesce) {
            let offset = coalescence::representative(&self.upcoming, now_ms).ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "coalesced schedule has no eligible occurrence",
                )
            })?;
            let covered = self.upcoming[..offset]
                .iter()
                .map(|window| trigger_id(&schedule.id, &timestamp(window.run_at)))
                .collect();
            (
                offset,
                Some(coalescence::Proof::capture(
                    db, schedule, now_ms, clock, covered,
                )?),
            )
        } else {
            (0, None)
        };
        Ok(Selection {
            offset,
            representative: self.occurrence(&schedule.id, offset, self.upcoming[offset]),
            anchor_trigger: self.next.trigger_id.clone(),
            proof,
        })
    }

    pub(super) fn prepare(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        selection: &Selection,
    ) -> Result<Value> {
        // The broker inherits this elapsed cutoff from the Schedule. A rolled
        // back admission must leave the original head's cutoff and index intact.
        self.move_cutoff(db, schedule, 0, selection.offset)?;
        crate::broker::retime_scheduled(
            &schedule.spec["request"],
            &selection.representative.trigger_id,
            &selection.representative.deadline_at,
        )
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
            Missed::Coalesce => (self.upcoming.len(), "Coalesced"),
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
                Outcome {
                    phase,
                    reason: Some(reason),
                    receipt: None,
                    overlap_evidence: None,
                    coverage: Coverage::None,
                },
            )?;
        }
        self.advance(db, schedule, body, count, 0, None)?;
        Ok(true)
    }

    pub(super) fn skip_overlap(&self, db: &Db<'_>, schedule: &Doc) -> Result<bool> {
        let (reason, evidence) = match self.overlap {
            Overlap::Allow => return Ok(false),
            Overlap::Skip if super::previous_task_active(db, schedule)? => {
                ("PreviousTaskActive", None)
            }
            Overlap::Skip => return Ok(false),
            Overlap::SkipWhileWorkflowActive => {
                let Some(block) = workflow_overlap::blocked(db.c, schedule)? else {
                    return Ok(false);
                };
                (block.reason, Some(block.evidence))
            }
        };
        let mut body = schedule.body.clone();
        record(
            db,
            schedule,
            &mut body,
            &self.next,
            Outcome {
                phase: "Skipped",
                reason: Some(reason),
                receipt: None,
                overlap_evidence: evidence,
                coverage: Coverage::None,
            },
        )?;
        self.advance(db, schedule, body, 1, 0, None)?;
        Ok(true)
    }

    pub(super) fn settle(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        selection: Selection,
        phase: &str,
        reason: Option<&str>,
        receipt: Option<Value>,
    ) -> Result<()> {
        let mut body = schedule.body.clone();
        for (offset, window) in self.upcoming[..selection.offset]
            .iter()
            .copied()
            .enumerate()
        {
            let (phase, reason, coverage) = match phase {
                "Expired" => ("Expired", "DeadlineExceeded", Coverage::None),
                "Rejected" => (
                    "Skipped",
                    coalescence::REJECTED,
                    Coverage::Covered(&selection.representative.trigger_id),
                ),
                _ => (
                    "Skipped",
                    coalescence::COVERED,
                    Coverage::Covered(&selection.representative.trigger_id),
                ),
            };
            record(
                db,
                schedule,
                &mut body,
                &self.occurrence(&schedule.id, offset, window),
                Outcome {
                    phase,
                    reason: Some(reason),
                    receipt: None,
                    overlap_evidence: None,
                    coverage,
                },
            )?;
        }
        record(
            db,
            schedule,
            &mut body,
            &selection.representative,
            Outcome {
                phase,
                reason,
                receipt,
                overlap_evidence: None,
                coverage: selection
                    .proof
                    .filter(|_| phase != "Expired")
                    .map_or(Coverage::None, Coverage::Representative),
            },
        )?;
        let rejection = if phase == "Rejected" { reason } else { None };
        // A refused admission rolled back the in-transaction cutoff shift.
        let cutoff = if phase == "Triggered" {
            selection.offset
        } else {
            0
        };
        self.advance(db, schedule, body, selection.offset + 1, cutoff, rejection)
    }

    fn move_cutoff(&self, db: &Db<'_>, schedule: &Doc, from: usize, to: usize) -> Result<()> {
        if from != to {
            let delta_ms =
                due_ms(self.upcoming[to].deadline_at) - due_ms(self.upcoming[from].deadline_at);
            db.c.execute(
                "UPDATE authorization_deadlines SET deadline_boot_ms=deadline_boot_ms+?2 WHERE resource=?1",
                params![schedule.id, delta_ms],
            )?;
        }
        Ok(())
    }

    fn advance(
        &self,
        db: &Db<'_>,
        schedule: &Doc,
        mut body: Value,
        count: usize,
        cutoff: usize,
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
            // Move the stored cutoff by the original interval between windows.
            self.move_cutoff(db, schedule, cutoff, count)?;
            admission_delay::clear(db, &schedule.id)?;
            body["next"] = json!(self.occurrence(&schedule.id, count, next));
        } else {
            if matches!(self.missed, Missed::Coalesce) {
                self.move_cutoff(db, schedule, cutoff, count - 1)?;
            }
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

enum Coverage<'a> {
    None,
    Covered(&'a str),
    Representative(coalescence::Proof),
}

struct Outcome<'a> {
    phase: &'a str,
    reason: Option<&'a str>,
    receipt: Option<Value>,
    overlap_evidence: Option<Value>,
    coverage: Coverage<'a>,
}

fn record(
    db: &Db<'_>,
    schedule: &Doc,
    body: &mut Value,
    timing: &Occurrence,
    outcome: Outcome<'_>,
) -> Result<()> {
    let Outcome {
        phase,
        reason,
        receipt,
        overlap_evidence,
        coverage,
    } = outcome;
    if let Some(receipt) = &receipt {
        body["last_task_id"] = receipt["task_id"].clone();
    }
    let mut state = json!({"schedule_id": schedule.id, "ordinal": timing.ordinal, "run_at": timing.run_at,
                  "deadline_at": timing.deadline_at, "lifecycle": phase, "reason": reason,
                  "task_receipt": receipt, "settled_at": now()});
    if let Some(evidence) = overlap_evidence {
        state["overlap_evidence"] = evidence;
    }
    match coverage {
        Coverage::None => {}
        Coverage::Covered(representative) => {
            state["coalesced_into_trigger_id"] = json!(representative)
        }
        Coverage::Representative(proof) => state["coalescence"] = json!(proof),
    }
    let trigger = db.create(
        "ScheduleTrigger",
        &timing.trigger_id,
        &schedule.owner,
        json!({"schedule_id": schedule.id, "definition_revision": "r1", "timing": timing}),
        state,
    )?;
    body["trigger_refs"]
        .as_array_mut()
        .ok_or_else(|| err("RecoveryRequired", "schedule trigger references missing"))?
        .push(trigger.reference());
    Ok(())
}
