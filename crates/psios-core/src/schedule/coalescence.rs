//! Witness the selected group of discrete timed occurrences in the broker commit.
use super::{
    Window, due_ms, instant,
    repeat::{Occurrence, Series, persisted},
    timestamp,
};
use crate::{
    Result, broker,
    db::{self, Db, Doc, row},
    err,
    policy::deadline::Clock,
    require,
};
use chrono::DateTime;
use psios_api::canonical;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::storage::restore_publication::CREATE_FORMAT + 1;
pub(super) const COVERED: &str = "Coalesced";
pub(super) const REJECTED: &str = "RepresentativeRejected";

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "timed schedule coalescence proof is inconsistent",
    )
}

fn check(valid: bool) -> Result<()> {
    if valid { Ok(()) } else { Err(invalid()) }
}

pub(super) fn representative(windows: &[Window], at: i64) -> Option<usize> {
    let first = windows.first()?;
    if due_ms(first.deadline_at) <= at {
        return None;
    }
    windows
        .iter()
        .take_while(|window| due_ms(window.run_at) <= at)
        .count()
        .checked_sub(1)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Proof {
    covered_trigger_ids: Vec<String>,
    schedule_ref: Value,
    selected_at: String,
    clock: Sample,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sample {
    boot_id: String,
    time_ns: String,
    elapsed_ms: i64,
    prior_cutoff_ms: Option<i64>,
}

fn cutoff(conn: &Connection, resource: &str, boot: &str) -> Result<Option<(String, i64)>> {
    Ok(conn.query_row(
        "SELECT time_ns,deadline_boot_ms FROM authorization_deadlines WHERE resource=?1 AND boot_id=?2",
        params![resource, boot], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?)
}

impl Proof {
    pub(super) fn capture(
        db: &Db<'_>,
        scheduled: &Doc,
        at: i64,
        clock: &Clock,
        covered_trigger_ids: Vec<String>,
    ) -> Result<Self> {
        let prior = cutoff(db.c, &scheduled.id, &clock.boot.boot_id)?;
        check(
            prior
                .as_ref()
                .is_none_or(|(ns, end)| ns == &clock.time_ns && *end > clock.boot.millis),
        )?;
        Ok(Self {
            covered_trigger_ids,
            schedule_ref: scheduled.reference(),
            selected_at: timestamp(DateTime::from_timestamp_millis(at).ok_or_else(invalid)?),
            clock: Sample {
                boot_id: clock.boot.boot_id.clone(),
                time_ns: clock.time_ns.clone(),
                elapsed_ms: clock.boot.millis,
                prior_cutoff_ms: prior.map(|(_, end)| end),
            },
        })
    }

    fn eligible(&self, windows: &[Window], start: usize, end: usize) -> Result<()> {
        let at = instant(&self.selected_at).map_err(|_| invalid())?;
        check(
            timestamp(at) == self.selected_at
                && at.timestamp_subsec_nanos().is_multiple_of(1_000_000)
                && representative(&windows[start..], at.timestamp_millis()) == Some(end - start),
        )
    }

    fn clock(&self, conn: &Connection, schedule: &str, first: Window, last: Window) -> Result<()> {
        let sample = &self.clock;
        check(
            uuid::Uuid::parse_str(&sample.boot_id)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == sample.boot_id)
                && (sample.time_ns == "unsupported"
                    || sample
                        .time_ns
                        .parse::<u64>()
                        .is_ok_and(|ns| ns != 0 && ns.to_string() == sample.time_ns))
                && sample.elapsed_ms >= 0
                && sample
                    .prior_cutoff_ms
                    .is_none_or(|end| end > sample.elapsed_ms),
        )?;
        let shift = due_ms(last.deadline_at) - due_ms(first.deadline_at);
        let anchored = cutoff(conn, schedule, &sample.boot_id)?;
        check(match (anchored, sample.prior_cutoff_ms) {
            (Some((ns, end)), Some(original)) => {
                ns == sample.time_ns && original.checked_add(shift) == Some(end)
            }
            (None, None) => true,
            _ => false,
        })
    }
}

struct Recorded {
    doc: Doc,
    seq: i64,
    correlation: String,
}

impl Recorded {
    fn revision(conn: &Connection, id: &str, rev: i64) -> Result<Self> {
        conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![id, rev],
            |r| {
                Ok(Self {
                    doc: row(r)?,
                    seq: r.get("seq")?,
                    correlation: r.get("correlation")?,
                })
            },
        )
        .optional()?
        .ok_or_else(invalid)
    }

    fn trigger(
        conn: &Connection,
        schedule: &Doc,
        reference: &Value,
        index: usize,
        window: Window,
    ) -> Result<Self> {
        let timing = Occurrence::new(&schedule.id, index as u32 + 1, window);
        let record = Self::revision(conn, &timing.trigger_id, 1)?;
        let doc = &record.doc;
        check(
            doc.reference() == *reference
                && doc.kind == "ScheduleTrigger"
                && doc.owner == schedule.owner
                && doc.spec
                    == json!({"schedule_id":schedule.id,"definition_revision":"r1","timing":timing})
                && doc.body["schedule_id"] == schedule.id
                && doc.body["ordinal"] == timing.ordinal
                && doc.body["run_at"] == timing.run_at
                && doc.body["deadline_at"] == timing.deadline_at
                && db::find(conn, &timing.trigger_id)?.is_some_and(|current| current.rev == 1),
        )?;
        Ok(record)
    }
}

fn references(schedule: &Doc) -> Result<&[Value]> {
    schedule.body["trigger_refs"]
        .as_array()
        .filter(|refs| refs.len() <= 32)
        .map(Vec::as_slice)
        .ok_or_else(invalid)
}

struct History<'a> {
    schedule: &'a Doc,
    windows: Vec<Window>,
    records: Vec<Recorded>,
    anchor: Window,
    origin: String,
}

struct Submission {
    task_id: String,
    digest: String,
    scope: String,
    receipt: Value,
}

impl<'a> History<'a> {
    fn load(conn: &Connection, schedule: &'a Doc) -> Result<Self> {
        let initial = Recorded::revision(conn, &schedule.id, 1)?;
        check(
            initial.doc.kind == "Schedule"
                && initial.doc.owner == schedule.owner
                && initial.doc.spec == schedule.spec
                && references(&initial.doc)?.is_empty()
                && initial.doc.body["calendar_plan"] == schedule.body["calendar_plan"]
                && schedule.spec["event"].is_null(),
        )?;
        let windows = Series::load(&initial.doc)?.ok_or_else(invalid)?.upcoming;
        let refs = references(schedule)?;
        check(refs.len() <= windows.len())?;
        let anchor = match schedule.body["lifecycle"].as_str() {
            Some("Pending" | "Cancelled") => refs.len(),
            Some("Completed") if refs.len() == windows.len() => refs.len() - 1,
            Some("Rejected") if !refs.is_empty() => refs.len() - 1,
            _ => return Err(invalid()),
        };
        let anchor_window = *windows.get(anchor).ok_or_else(invalid)?;
        if schedule.body["lifecycle"] == "Pending" {
            let next: Occurrence = persisted(&schedule.body["next"])?;
            check(next == Occurrence::new(&schedule.id, anchor as u32 + 1, anchor_window))?;
            let index: Option<(i64, i64)> = conn
                .query_row(
                    "SELECT run_ms,deadline_ms FROM pending_schedules WHERE schedule=?1",
                    [&schedule.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            check(
                index
                    == Some((
                        due_ms(anchor_window.run_at),
                        due_ms(anchor_window.deadline_at),
                    )),
            )?;
        } else {
            check(schedule.body["next"].is_null())?;
        }
        let records = refs
            .iter()
            .zip(&windows)
            .enumerate()
            .map(|(index, (reference, window))| {
                Recorded::trigger(conn, schedule, reference, index, *window)
            })
            .collect::<Result<Vec<_>>>()?;
        check(records.windows(2).all(|pair| pair[0].seq < pair[1].seq))?;
        let authority: String =
            conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
                r.get(0)
            })?;
        let origin =
            json!({"authority_id":authority,"origin_type":"Schedule","origin_id":schedule.id});
        check(schedule.spec["request"]["origin_ref"] == origin)?;
        Ok(Self {
            schedule,
            windows,
            records,
            anchor: anchor_window,
            origin: String::from_utf8(canonical(&origin)).map_err(|_| invalid())?,
        })
    }

    fn submission(&self, conn: &Connection, trigger: &str) -> Result<Option<Submission>> {
        Ok(conn.query_row(
            "SELECT task,digest,scope,receipt FROM submissions WHERE owner=?1 AND origin=?2 AND key=?3 AND receipt IS NOT NULL",
            params![self.schedule.owner, self.origin, trigger], |r| Ok(Submission {
                task_id: r.get(0)?, digest: r.get(1)?, scope: r.get(2)?, receipt: r.get(3)?,
            }),
        ).optional()?)
    }

    fn verify_group(
        &self,
        conn: &Connection,
        start: usize,
        end: usize,
        proof: Proof,
    ) -> Result<()> {
        let schedule = self.schedule;
        let representative = &self.records[end];
        let outcome = &representative.doc.body["lifecycle"];
        let triggered = outcome == "Triggered";
        check(triggered || outcome == "Rejected")?;
        proof.eligible(&self.windows, start, end)?;
        proof.clock(conn, &schedule.id, self.windows[start], self.anchor)?;
        let bases = db::resolve(conn, &proof.schedule_ref).map_err(|_| invalid())?;
        let before = Recorded::revision(conn, &schedule.id, bases.rev)?;
        let after = Recorded::revision(conn, &schedule.id, bases.rev + 1)?;
        let adjacent = &self.records[start..=end];
        let refs = references(schedule)?;
        let next: Occurrence = persisted(&bases.body["next"])?;
        check(
            bases.id == schedule.id
                && bases.kind == "Schedule"
                && bases.owner == schedule.owner
                && bases.spec == schedule.spec
                && bases.body["lifecycle"] == "Pending"
                && next == Occurrence::new(&schedule.id, start as u32 + 1, self.windows[start])
                && references(&bases)? == &refs[..start]
                && after.doc.kind == "Schedule"
                && after.doc.owner == schedule.owner
                && after.doc.spec == schedule.spec
                && references(&after.doc)? == &refs[..=end]
                && before.seq < adjacent[0].seq
                && representative.seq < after.seq
                && adjacent
                    .iter()
                    .all(|member| member.correlation == after.correlation)
                && proof
                    .covered_trigger_ids
                    .iter()
                    .map(String::as_str)
                    .eq(adjacent
                        .iter()
                        .take(end - start)
                        .map(|member| member.doc.id.as_str())),
        )?;
        let covered_reason = if triggered { COVERED } else { REJECTED };
        for member in &self.records[start..end] {
            let body = &member.doc.body;
            check(
                body["lifecycle"] == "Skipped"
                    && body["reason"] == covered_reason
                    && body["coalesced_into_trigger_id"] == representative.doc.id
                    && body.get("coalescence").is_none()
                    && body.get("overlap_evidence").is_none()
                    && body["task_receipt"].is_null()
                    && self.submission(conn, &member.doc.id)?.is_none(),
            )?;
        }
        check(
            representative
                .doc
                .body
                .get("coalesced_into_trigger_id")
                .is_none()
                && representative.doc.body.get("overlap_evidence").is_none(),
        )?;
        let submitted = self.submission(conn, &representative.doc.id)?;
        if !triggered {
            return check(
                representative.doc.body["task_receipt"].is_null()
                    && submitted.is_none()
                    && end + 1 == self.records.len()
                    && schedule.body["lifecycle"] == "Rejected"
                    && representative.doc.body["reason"] == schedule.body["reason"],
            );
        }
        let Submission {
            task_id,
            digest,
            scope,
            receipt,
        } = submitted.ok_or_else(invalid)?;
        let initial = Recorded::revision(conn, &task_id, 1)?;
        let deadline = timestamp(self.windows[end].deadline_at);
        let request =
            broker::retime_scheduled(&schedule.spec["request"], &representative.doc.id, &deadline)?;
        let sample = &proof.clock;
        let task_cutoff = cutoff(conn, &task_id, &sample.boot_id)?.ok_or_else(invalid)?;
        let delta = due_ms(self.windows[end].deadline_at) - due_ms(self.windows[start].deadline_at);
        check(
            initial.doc.kind == "Task"
                && initial.doc.owner == schedule.owner
                && initial.doc.spec == request
                && before.seq < initial.seq
                && initial.seq < adjacent[0].seq
                && initial.correlation == after.correlation
                && scope == schedule.id
                && receipt == representative.doc.body["task_receipt"]
                && after.doc.body["last_task_id"] == task_id
                && receipt["task_id"] == task_id
                && receipt["task_revision"] == "r1"
                && receipt["deduplicated"] == false
                && receipt["idempotency_key"] == representative.doc.id
                && receipt["origin_ref"] == request["origin_ref"]
                && receipt["request_digest"] == digest
                && receipt["request_digest"] == request["request_digest"]
                && representative.doc.body["reason"].is_null()
                && task_cutoff.0 == sample.time_ns
                && task_cutoff.1 > sample.elapsed_ms
                && sample.prior_cutoff_ms.is_none_or(|base| {
                    base.checked_add(delta)
                        .is_some_and(|end| task_cutoff.1 <= end)
                }),
        )
    }

    fn verify(&self, conn: &Connection) -> Result<()> {
        let mut covered = None;
        let mut latest_task = Value::Null;
        for (index, item) in self.records.iter().enumerate() {
            let body = &item.doc.body;
            if body.get("coalesced_into_trigger_id").is_some() {
                covered.get_or_insert(index);
                continue;
            }
            if let Some(proof) = body.get("coalescence") {
                self.verify_group(
                    conn,
                    covered.take().unwrap_or(index),
                    index,
                    persisted(proof)?,
                )?;
                if body["lifecycle"] == "Triggered" {
                    latest_task = body["task_receipt"]["task_id"].clone();
                }
            } else {
                check(
                    covered.is_none()
                        && body["task_receipt"].is_null()
                        && (body["lifecycle"] == "Expired" && body["reason"] == "DeadlineExceeded"
                            || body["lifecycle"] == "Skipped"
                                && matches!(
                                    body["reason"].as_str(),
                                    Some(
                                        "PreviousTaskActive"
                                            | "PreviousWorkflowActive"
                                            | "PreviousWorkflowUnverified"
                                    )
                                ))
                        && self.submission(conn, &item.doc.id)?.is_none(),
                )?;
            }
        }
        check(covered.is_none() && self.schedule.body["last_task_id"] == latest_task)
    }
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current t LEFT JOIN current s ON s.id=t.spec->>'schedule_id'
          WHERE t.kind='ScheduleTrigger' AND (t.body->'coalescence' IS NOT NULL OR t.body->'coalesced_into_trigger_id' IS NOT NULL)
          AND (s.id IS NULL OR s.kind<>'Schedule' OR
               (s.spec->'repeat'->'missed'->>'mode' IS NOT 'Coalesce' AND s.spec->'calendar'->'missed'->>'mode' IS NOT 'Coalesce')
               OR NOT EXISTS(SELECT 1 FROM json_each(s.body->'trigger_refs') WHERE value->>'resource_id'=t.id)))",
        [], |r| r.get(0),
    )?;
    require(
        !orphan,
        "RecoveryRequired",
        "coalescence record lost its original schedule",
    )?;
    let mut schedules = conn.prepare("SELECT * FROM current WHERE kind='Schedule'
        AND (spec->'repeat'->'missed'->>'mode'='Coalesce' OR spec->'calendar'->'missed'->>'mode'='Coalesce')")?;
    for schedule in schedules.query_map([], row)? {
        History::load(conn, &schedule?)?.verify(conn)?;
    }
    Ok(())
}
