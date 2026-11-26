//! Validate resource-event custody separately from the shared Workflow launch proof.
use super::{
    due_ms, instant,
    resource_event::{Rule, pending_body, trigger_spec, verify_source},
    workflow_overlap::{self, Block, MODE},
};
use crate::{
    Result, broker,
    db::{self, Doc, row},
    err, policy,
    workflow::START_TOOL,
};
use psios_api::canonical;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::storage::directory_create::FORMAT + 1;

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "protected Workflow event schedule has inconsistent source custody",
    )
}

fn check(valid: bool) -> Result<()> {
    if valid { Ok(()) } else { Err(invalid()) }
}

struct Version {
    doc: Doc,
    seq: i64,
    correlation: String,
}

impl Version {
    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            doc: row(r)?,
            seq: r.get("seq")?,
            correlation: r.get("correlation")?,
        })
    }

    fn exact(conn: &Connection, id: &str, rev: i64) -> Result<Self> {
        conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![id, rev],
            Self::read,
        )
        .optional()?
        .ok_or_else(invalid)
    }
}

struct Occurrence {
    version: Version,
    event_seq: i64,
    source_rev: i64,
    copied_seq: Option<i64>,
}

impl Occurrence {
    fn doc(&self) -> &Doc {
        &self.version.doc
    }
}

struct Scope<'a> {
    conn: &'a Connection,
    schedule: &'a Doc,
    rule: &'a Rule,
    source: Doc,
}

impl<'a> Scope<'a> {
    fn load(conn: &'a Connection, schedule: &'a Doc, rule: &'a Rule) -> Result<Self> {
        let source = db::resolve(conn, &rule.source_ref).map_err(|_| invalid())?;
        let origin = &schedule.spec["request"]["origin_ref"];
        let elapsed: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=?1)",
            [&schedule.id],
            |r| r.get(0),
        )?;
        check(
            schedule.kind == "Schedule"
                && rule.protects_workflow()
                && schedule.spec["repeat"].is_null()
                && schedule.spec["calendar"].is_null()
                && schedule.spec["request"]["task_type"] == START_TOOL
                && schedule.spec["request"]["deadline_at"] == rule.expires_at
                && origin["origin_type"] == "Schedule"
                && origin["origin_id"] == schedule.id
                && elapsed
                && schedule.body["overlap"] == MODE
                && schedule.body["max_triggers"] == rule.max_triggers
                && (1..=32).contains(&rule.max_triggers)
                && schedule.body["expires_at"] == rule.expires_at
                && policy::visible(&schedule.owner, &source),
        )?;
        Ok(Self {
            conn,
            schedule,
            rule,
            source,
        })
    }

    fn scheduler_version_after(&self, trigger: &Version) -> Result<Version> {
        let updated = self.conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND seq>?2 AND correlation=?3 ORDER BY rev LIMIT 1",
            params![self.schedule.id, trigger.seq, trigger.correlation], Version::read,
        ).optional()?.ok_or_else(invalid)?;
        check(
            updated.doc.kind == self.schedule.kind
                && updated.doc.owner == self.schedule.owner
                && updated.doc.spec == self.schedule.spec,
        )?;
        Ok(updated)
    }

    fn occurrence(&self, reference: &Value) -> Result<Occurrence> {
        let doc = db::resolve(self.conn, reference).map_err(|_| invalid())?;
        let version = Version::exact(self.conn, &doc.id, doc.rev)?;
        let event = &doc.body["source_event"];
        let (event_seq, source_rev) = verify_source(
            self.conn,
            self.schedule,
            self.rule,
            &self.source,
            &doc.id,
            event,
        )?;
        check(
            doc.kind == "ScheduleTrigger"
                && doc.owner == self.schedule.owner
                && doc.generation == 1
                && doc.spec == trigger_spec(self.schedule)
                && event_seq < version.seq
                && db::find(self.conn, &doc.id)?.is_some_and(|current| current.rev == doc.rev),
        )?;
        let mut expected = pending_body(self.schedule, &doc.id, event);
        let phase = doc.body["lifecycle"].as_str().ok_or_else(invalid)?;
        if phase == "Pending" {
            check(doc.rev == 1 && doc.body == expected)?;
            return Ok(Occurrence {
                version,
                event_seq,
                source_rev,
                copied_seq: None,
            });
        }
        check(
            matches!(
                phase,
                "Triggered" | "Skipped" | "Expired" | "Cancelled" | "Rejected"
            ) && doc.body["settled_at"]
                .as_str()
                .is_some_and(|at| instant(at).is_ok())
                && (phase == "Triggered") == doc.body["reason"].is_null(),
        )?;
        let copied_seq = if doc.rev == 2 {
            let original = Version::exact(self.conn, &doc.id, 1)?;
            let stored = self.scheduler_version_after(&original)?;
            check(
                original.doc.kind == doc.kind
                    && original.doc.owner == doc.owner
                    && original.doc.generation == doc.generation
                    && original.doc.spec == doc.spec
                    && original.doc.body == expected
                    && event_seq < original.seq
                    && stored.seq < version.seq
                    && stored.doc.body["lifecycle"] == "Pending"
                    && stored.doc.body["pending_trigger_ref"] == original.doc.reference(),
            )?;
            Some(stored.seq)
        } else {
            check(doc.rev == 1)?;
            None
        };
        for field in [
            "lifecycle",
            "reason",
            "task_receipt",
            "settled_at",
            "overlap_evidence",
        ] {
            if let Some(value) = doc.body.get(field) {
                expected[field] = value.clone();
            }
        }
        check(
            doc.body == expected
                && (phase == "Skipped") == doc.body.get("overlap_evidence").is_some(),
        )?;
        Ok(Occurrence {
            version,
            event_seq,
            source_rev,
            copied_seq,
        })
    }

    fn references(&self) -> Result<&[Value]> {
        self.schedule.body["trigger_refs"]
            .as_array()
            .filter(|refs| refs.len() <= self.rule.max_triggers as usize)
            .map(Vec::as_slice)
            .ok_or_else(invalid)
    }

    fn history(&self) -> Result<Vec<Occurrence>> {
        let refs = self.references()?;
        let mut records: Vec<Occurrence> = Vec::with_capacity(refs.len());
        let mut source_ref = self.rule.source_ref.clone();
        for (index, reference) in refs.iter().enumerate() {
            let occurrence = self.occurrence(reference)?;
            let doc = occurrence.doc();
            let updated = self.scheduler_version_after(&occurrence.version)?;
            let phase = doc.body["lifecycle"].as_str().ok_or_else(invalid)?;
            let closed_without_advance = matches!(phase, "Expired" | "Cancelled" | "Rejected")
                && updated.doc.body["lifecycle"] == phase;
            if closed_without_advance {
                check(index + 1 == refs.len() && occurrence.copied_seq.is_some())?;
            } else {
                source_ref = doc.body["source_event"]["resource_ref"].clone();
            }
            // The Schedule commits the exact occurrence and source pointer in its settlement transaction.
            check(
                phase != "Pending"
                    && records.last().is_none_or(|previous| {
                        previous.source_rev < occurrence.source_rev
                            && previous.event_seq < occurrence.event_seq
                            && previous.version.seq < occurrence.version.seq
                    })
                    && updated.doc.body["trigger_refs"]
                        .as_array()
                        .is_some_and(|saved| saved == &refs[..=index])
                    && updated.doc.body["pending_trigger_ref"].is_null()
                    && updated.doc.body["source_ref"] == source_ref,
            )?;
            records.push(occurrence);
        }
        check(self.schedule.body["source_ref"] == source_ref)?;
        Ok(records)
    }

    fn cursor(&self) -> Result<Option<i64>> {
        let values: Option<(String, i64, i64)> = self.conn.query_row(
            "SELECT source,source_rev,deadline_ms FROM pending_event_schedules WHERE schedule=?1",
            [&self.schedule.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        if self.schedule.body["lifecycle"] != "Pending" {
            check(values.is_none() && self.schedule.body["pending_trigger_ref"].is_null())?;
            return Ok(None);
        }
        let (id, rev, deadline) = values.ok_or_else(invalid)?;
        let cursor = Version::exact(self.conn, &id, rev)?;
        let last_source =
            db::resolve(self.conn, &self.schedule.body["source_ref"]).map_err(|_| invalid())?;
        let expiry = due_ms(instant(&self.rule.expires_at).map_err(|_| invalid())?);
        check(
            id == self.source.id
                && cursor.doc.owner == self.source.owner
                && cursor.doc.kind == self.source.kind
                && last_source.id == self.source.id
                && rev >= last_source.rev
                && last_source.rev >= self.source.rev
                && deadline == expiry
                && self.references()?.len() < self.rule.max_triggers as usize,
        )?;
        Ok(Some(rev))
    }

    fn check_pending(&self, cursor: i64, reference: &Value) -> Result<Occurrence> {
        let pending = self.occurrence(reference)?;
        let stored = self.scheduler_version_after(&pending.version)?;
        check(
            pending.doc().body["lifecycle"] == "Pending"
                && Some(pending.source_rev) == cursor.checked_add(1)
                && stored.doc.reference() == self.schedule.reference()
                && stored.doc.body["pending_trigger_ref"] == *reference,
        )?;
        Ok(pending)
    }

    fn submission(&self, occurrence: &Occurrence) -> Result<()> {
        let doc = occurrence.doc();
        let template = &self.schedule.spec["request"];
        let origin =
            String::from_utf8(canonical(&template["origin_ref"])).map_err(|_| invalid())?;
        let row: Option<(String, Value)> = self.conn.query_row(
            "SELECT task,receipt FROM submissions WHERE owner=?1 AND origin=?2 AND key=?3 AND receipt IS NOT NULL",
            params![self.schedule.owner, origin, doc.id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        if doc.body["lifecycle"] != "Triggered" {
            return check(row.is_none() && doc.body["task_receipt"].is_null());
        }
        let (task, receipt) = row.ok_or_else(invalid)?;
        let accepted = Version::exact(self.conn, &task, 1)?;
        let expected = broker::retime_scheduled(template, &doc.id, &self.rule.expires_at)?;
        check(
            doc.body["task_receipt"] == receipt
                && receipt["task_id"] == task
                && receipt["idempotency_key"] == doc.id
                && receipt["task_revision"] == "r1"
                && receipt["origin_ref"] == expected["origin_ref"]
                && receipt["request_digest"] == expected["request_digest"]
                && receipt["deduplicated"] == false
                && accepted.doc.kind == "Task"
                && accepted.doc.owner == self.schedule.owner
                && accepted.doc.spec == expected
                && occurrence.copied_seq.unwrap_or(occurrence.event_seq) < accepted.seq
                && accepted.seq < occurrence.version.seq
                && accepted.correlation == occurrence.version.correlation,
        )
    }
}

pub(super) fn verify_pending(
    conn: &Connection,
    schedule: &Doc,
    rule: &Rule,
    pending: &Doc,
) -> Result<()> {
    let scope = Scope::load(conn, schedule, rule)?;
    let cursor = scope.cursor()?.ok_or_else(invalid)?;
    let reference = pending.reference();
    check(schedule.body["pending_trigger_ref"] == reference)?;
    scope.check_pending(cursor, &reference).map(|_| ())
}

pub(super) fn blocked(
    conn: &Connection,
    schedule: &Doc,
    rule: &Rule,
    id: &str,
    event: &Value,
) -> Result<Option<Block>> {
    let scope = Scope::load(conn, schedule, rule)?;
    let cursor = scope.cursor()?.ok_or_else(invalid)?;
    let (seq, rev) = verify_source(conn, schedule, rule, &scope.source, id, event)?;
    check(Some(rev) == cursor.checked_add(1))?;
    let pending_ref = &schedule.body["pending_trigger_ref"];
    if !pending_ref.is_null() {
        let pending = scope.check_pending(cursor, pending_ref)?;
        check(pending.doc().id == id && pending.event_seq == seq)?;
        scope.submission(&pending)?;
    }
    let history = scope.history()?;
    check(
        history
            .last()
            .is_none_or(|last| last.source_rev <= cursor && last.event_seq < seq),
    )?;
    let previous = history
        .iter()
        .rev()
        .find(|record| record.doc().body["lifecycle"] == "Triggered");
    let Some(previous) = previous else {
        check(schedule.body["last_task_id"].is_null())?;
        return Ok(None);
    };
    scope.submission(previous)?;
    check(schedule.body["last_task_id"] == previous.doc().body["task_receipt"]["task_id"])?;
    workflow_overlap::block_at(conn, schedule, previous.doc(), None)
}

/// Inspect only the bounded pending event index, before the shared timer starts.
pub(crate) fn check_active(conn: &Connection) -> Result<()> {
    let bound = i64::from(crate::config::MAX_QUEUE_LIMIT) + 1;
    let mut index =
        conn.prepare("SELECT schedule FROM pending_event_schedules ORDER BY schedule LIMIT ?1")?;
    let ids = index.query_map([bound], |r| r.get::<_, String>(0))?;
    for (offset, id) in ids.enumerate() {
        check(offset < crate::config::MAX_QUEUE_LIMIT as usize)?;
        let Some(schedule) = db::find(conn, &id?)? else {
            continue;
        };
        if schedule.spec["event"]["overlap"] != MODE && schedule.body["overlap"] != MODE {
            continue;
        }
        let rule: Rule = crate::decode(&schedule.spec["event"]).map_err(|_| invalid())?;
        let scope = Scope::load(conn, &schedule, &rule)?;
        let cursor = scope.cursor()?.ok_or_else(invalid)?;
        let pending = &schedule.body["pending_trigger_ref"];
        if !pending.is_null() {
            let pending = scope.check_pending(cursor, pending)?;
            scope.submission(&pending)?;
        }
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut query = conn.prepare(
        "SELECT * FROM current WHERE kind='Schedule'
         AND (spec->'event'->>'overlap'=?1 OR (spec->'event' IS NOT NULL AND body->>'overlap'=?1))",
    )?;
    for schedule in query.query_map([MODE], row)? {
        let schedule = schedule?;
        let rule: Rule = crate::decode(&schedule.spec["event"]).map_err(|_| invalid())?;
        let scope = Scope::load(conn, &schedule, &rule)?;
        let first = Version::exact(conn, &schedule.id, 1)?;
        check(
            first.doc.kind == schedule.kind
                && first.doc.owner == schedule.owner
                && first.doc.spec == schedule.spec
                && first.doc.body["lifecycle"] == "Pending"
                && first.doc.body["source_ref"] == rule.source_ref
                && first.doc.body["trigger_refs"] == json!([])
                && first.doc.body["pending_trigger_ref"].is_null(),
        )?;
        let history = scope.history()?;
        let cursor = scope.cursor()?;
        let pending = &schedule.body["pending_trigger_ref"];
        if !pending.is_null() {
            let pending = scope.check_pending(cursor.ok_or_else(invalid)?, pending)?;
            scope.submission(&pending)?;
        }
        let mut previous = None;
        for entry in &history {
            scope.submission(entry)?;
            let trigger = entry.doc();
            workflow_overlap::verify_disposition(
                conn,
                &schedule,
                trigger,
                entry.version.seq,
                previous,
            )?;
            if trigger.body["lifecycle"] == "Triggered" {
                previous = Some(trigger);
            }
        }
        workflow_overlap::verify_last(conn, &schedule, previous)?;
    }
    Ok(())
}
