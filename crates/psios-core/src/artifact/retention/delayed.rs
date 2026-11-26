//! A delayed approval can reclaim only the original exact Task results.
use super::{
    CandidateMetadata,
    batch::{self, Plan},
    candidate_metadata,
};
use crate::{
    ById, Conditional, Result,
    db::{Db, Doc, find},
    decode, err, label, now,
    policy::{self, deadline::Clock},
    require,
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use psios_api::{ResourceRef, canonical, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

mod verify;
pub(crate) use verify::{verify, verify_pending};

pub(crate) const FORMAT: u32 = crate::node::network::trial::ROLLBACK_FORMAT + 1;
pub(crate) const KIND: &str = "TaskResultEvictionSchedule";
const MAX_PENDING: i64 = 64;
pub(crate) const SCHEMA: &str = "
    CREATE TABLE pending_result_eviction_schedules(
        schedule TEXT PRIMARY KEY,
        scope_digest TEXT NOT NULL CHECK(length(scope_digest)=71),
        due_boot_ms INTEGER NOT NULL CHECK(due_boot_ms>=0)
    ) STRICT;
    CREATE INDEX result_eviction_schedule_due
        ON pending_result_eviction_schedules(due_boot_ms,schedule);
";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
enum Phase {
    Scheduled,
    Cancelled,
    Suspended,
    Rejected,
    Evicted,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct State {
    lifecycle: Phase,
    plan_digest: String,
    admitted_at: String,
    admitted_boot_ms: i64,
    due_at: String,
    due_boot_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    settled_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduled_ref: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_ref: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    executed_boot_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    schedule: String,
    scope_digest: String,
    due_boot_ms: i64,
}

#[derive(Default)]
pub(crate) struct Cursor {
    due_boot_ms: i64,
    schedule: String,
}

impl Pending {
    fn new(schedule: &str, plan: &Plan, state: &State) -> Self {
        Self {
            schedule: schedule.into(),
            scope_digest: plan_scope(plan),
            due_boot_ms: state.due_boot_ms,
        }
    }

    fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            schedule: row.get(0)?,
            scope_digest: row.get(1)?,
            due_boot_ms: row.get(2)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    plan_token: String,
}

enum Settlement {
    Cancelled,
    Suspended,
    Rejected(&'static str),
    Evicted { batch_ref: ResourceRef, clock: i64 },
}

pub(super) fn schedule_id(plan_digest: &str) -> String {
    format!("result-eviction-schedule-{}", &plan_digest[7..])
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "invalid task result eviction schedule history",
    )
}

fn utc_stamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn stamp(value: &str) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value).ok()?.to_utc();
    (utc_stamp(parsed) == value).then_some(parsed)
}

impl State {
    fn new(plan: &Plan, admitted_at: String, admitted_boot_ms: i64) -> Result<Self> {
        let delay = plan.delay_seconds.ok_or_else(invalid)?;
        let due_at = stamp(&admitted_at)
            .and_then(|at| at.checked_add_signed(TimeDelta::seconds(i64::from(delay))))
            .ok_or_else(invalid)?;
        let due_boot_ms = admitted_boot_ms
            .checked_add(i64::from(delay) * 1000)
            .ok_or_else(invalid)?;
        Ok(Self {
            lifecycle: Phase::Scheduled,
            plan_digest: batch::digest(plan)?,
            admitted_at,
            admitted_boot_ms,
            due_at: utc_stamp(due_at),
            due_boot_ms,
            settled_at: None,
            scheduled_ref: None,
            code: None,
            batch_ref: None,
            executed_boot_ms: None,
        })
    }

    fn settle(&mut self, original: &Doc, settlement: Settlement) -> Result<()> {
        self.lifecycle = match settlement {
            Settlement::Cancelled => Phase::Cancelled,
            Settlement::Suspended => {
                self.code = Some("ScopeChanged".into());
                Phase::Suspended
            }
            Settlement::Rejected(code) => {
                self.code = Some(code.into());
                Phase::Rejected
            }
            Settlement::Evicted { batch_ref, clock } => {
                self.batch_ref = Some(batch_ref);
                self.executed_boot_ms = Some(clock);
                Phase::Evicted
            }
        };
        self.scheduled_ref = Some(decode(&original.reference())?);
        self.settled_at = Some(now());
        Ok(())
    }
}

fn scope_digest(
    authority: &str,
    incarnation: &str,
    epoch: &str,
    owner: &str,
    boot_id: &str,
    time_ns: &str,
) -> String {
    sha256(&canonical(&json!([
        authority,
        incarnation,
        epoch,
        owner,
        boot_id,
        time_ns
    ])))
}

fn plan_scope(plan: &Plan) -> String {
    scope_digest(
        &plan.authority_id,
        &plan.recovery_incarnation,
        &plan.authorization_epoch,
        &plan.owner,
        &plan.boot_id,
        &plan.time_namespace,
    )
}

fn current_scope(db: &Db<'_>, clock: &Clock) -> Result<String> {
    Ok(scope_digest(
        &db.meta("authority")?,
        &db.incarnation,
        &db.meta("epoch")?,
        policy::OWNER,
        &clock.boot.boot_id,
        &clock.time_ns,
    ))
}

fn authorize(db: &Db<'_>, actor: &str) -> Result<()> {
    policy::admin(actor)?;
    policy::active(db, actor)
}

fn verify_record(record: &Doc) -> Result<(Plan, State)> {
    let plan: Plan = decode(&record.spec["plan"]).map_err(|_| invalid())?;
    plan.validate().map_err(|_| invalid())?;
    let state: State = decode(&record.body).map_err(|_| invalid())?;
    let queued = State::new(&plan, state.admitted_at.clone(), state.admitted_boot_ms)?;
    let mut expected = queued;
    if state.lifecycle != Phase::Scheduled {
        expected.lifecycle = state.lifecycle;
        expected.scheduled_ref = state.scheduled_ref.clone();
        expected.settled_at = state.settled_at.clone();
        require(
            expected.scheduled_ref.is_some()
                && expected.settled_at.as_deref().and_then(stamp).is_some(),
            "RecoveryRequired",
            "terminal result eviction schedule lost its admission reference",
        )?;
    }
    match state.lifecycle {
        Phase::Scheduled | Phase::Cancelled => {}
        Phase::Suspended => expected.code = Some("ScopeChanged".into()),
        Phase::Rejected => {
            require(
                matches!(
                    state.code.as_deref(),
                    Some("Conflict" | "PreconditionFailed")
                ),
                "RecoveryRequired",
                "eviction schedule has an invalid rejection reason",
            )?;
            expected.code = state.code.clone();
        }
        Phase::Evicted => {
            require(
                state.batch_ref.is_some()
                    && state
                        .executed_boot_ms
                        .is_some_and(|ms| ms >= state.due_boot_ms),
                "RecoveryRequired",
                "eviction schedule has no due-time batch evidence",
            )?;
            expected.batch_ref = state.batch_ref.clone();
            expected.executed_boot_ms = state.executed_boot_ms;
        }
    }
    require(
        record.kind == KIND
            && record.owner == policy::OWNER
            && record.generation == 1
            && record.rev
                == if state.lifecycle == Phase::Scheduled {
                    1
                } else {
                    2
                }
            && record.id == schedule_id(&expected.plan_digest)
            && record.spec == json!({ "plan": plan })
            && record.body == json!(expected)
            && (plan.issued_boot_ms..plan.deadline_boot_ms).contains(&state.admitted_boot_ms),
        "RecoveryRequired",
        "eviction schedule does not match its immutable admission",
    )?;
    Ok((plan, state))
}

fn view(record: &Doc) -> Value {
    let mut schedule = record.body.clone();
    for field in ["reason", "delay_seconds", "items", "content_bytes"] {
        schedule[field] = record.spec["plan"][field].clone();
    }
    json!({ "reference": record.reference(), "schedule": schedule })
}

fn exclusion(db: &Db<'_>, plan: &Plan) -> Result<Option<&'static str>> {
    for item in &plan.items {
        let task = db.find(&item.task_ref.resource_id)?.ok_or_else(invalid)?;
        if task.reference() != serde_json::to_value(&item.task_ref)? {
            return Ok(Some("Conflict"));
        }
        match candidate_metadata(db, &task)? {
            CandidateMetadata::Pinned => return Ok(Some("Conflict")),
            CandidateMetadata::Ineligible => return Ok(Some("PreconditionFailed")),
            CandidateMetadata::Ready(bytes) => require(
                bytes == u128::from(item.content_bytes),
                "RecoveryRequired",
                "planned result byte count changed without a Task revision",
            )?,
        }
    }
    Ok(None)
}

fn pending_row(conn: &Connection, schedule: &str) -> Result<Option<Pending>> {
    Ok(conn.query_row(
        "SELECT schedule,scope_digest,due_boot_ms FROM pending_result_eviction_schedules WHERE schedule=?1",
        [schedule],
        Pending::row,
    ).optional()?)
}

fn check_projection(
    record: &Doc,
    plan: &Plan,
    state: &State,
    actual: Option<Pending>,
) -> Result<()> {
    let expected =
        (state.lifecycle == Phase::Scheduled).then(|| Pending::new(&record.id, plan, state));
    require(
        actual == expected,
        "RecoveryRequired",
        "eviction schedule queue disagrees with its journal",
    )
}

pub(crate) fn schedule(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor)?;
    let request: Schedule = decode(params)?;
    let plan = batch::open(db, actor, &request.plan_token)?;
    require(
        plan.delay_seconds.is_some(),
        "InvalidArgument",
        "only a delayed result eviction plan can be scheduled",
    )?;
    let id = schedule_id(&batch::digest(&plan)?);
    if let Some(record) = db.find(&id)? {
        let (recorded_plan, state) = verify_record(&record)?;
        require(
            serde_json::to_value(recorded_plan)? == serde_json::to_value(&plan)?,
            "RecoveryRequired",
            "stored eviction schedule does not match its prepared plan",
        )?;
        check_projection(&record, &plan, &state, pending_row(db.c, &id)?)?;
        return Ok(view(&record));
    }
    plan.live(db)?;
    db.cfg.check_admission_space()?;
    let count: i64 = db.c.query_row(
        "SELECT count(*) FROM pending_result_eviction_schedules",
        [],
        |row| row.get(0),
    )?;
    require(
        count < MAX_PENDING,
        "QuotaExceeded",
        "pending result eviction schedule limit reached",
    )?;
    if let Some(code) = exclusion(db, &plan)? {
        return Err(err(
            code,
            "planned Task result is no longer eligible for eviction",
        ));
    }
    plan.live(db)?;
    let clock = Clock::sample()?;
    require(
        current_scope(db, &clock)? == plan_scope(&plan),
        "Conflict",
        "eviction plan authority changed before admission",
    )?;
    require(
        (plan.issued_boot_ms..plan.deadline_boot_ms).contains(&clock.boot.millis),
        "DeadlineExceeded",
        "eviction plan expired before admission",
    )?;
    let state = State::new(&plan, now(), clock.boot.millis)?;
    let pending = Pending::new(&id, &plan, &state);
    let record = db.create(KIND, &id, actor, json!({ "plan": plan }), json!(state))?;
    db.c.execute(
        "INSERT INTO pending_result_eviction_schedules(schedule,scope_digest,due_boot_ms) VALUES(?1,?2,?3)",
        params![pending.schedule, pending.scope_digest, pending.due_boot_ms],
    )?;
    db.audit(
        actor,
        "Task.ScheduleResultEvictionBatch",
        Some(&record.id),
        "Scheduled",
    )?;
    Ok(view(&record))
}

fn get_scoped(db: &Db<'_>, id: &str) -> Result<(Doc, State)> {
    label(id)?;
    let record = db.of(id, KIND)?;
    let (plan, state) = verify_record(&record)?;
    check_projection(&record, &plan, &state, pending_row(db.c, id)?)?;
    Ok((record, state))
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor)?;
    let request: ById = decode(params)?;
    let (record, _) = get_scoped(db, &request.id)?;
    Ok(view(&record))
}

fn settle(db: &Db<'_>, record: &Doc, mut state: State, settlement: Settlement) -> Result<Doc> {
    let method = if matches!(settlement, Settlement::Cancelled) {
        "Task.CancelResultEvictionSchedule"
    } else {
        "Task.AdvanceResultEvictionSchedule"
    };
    state.settle(record, settlement)?;
    let body = serde_json::to_value(state)?;
    let updated = db.status(record, body)?;
    let deleted = db.c.execute(
        "DELETE FROM pending_result_eviction_schedules WHERE schedule=?1",
        [&record.id],
    )?;
    require(
        deleted == 1,
        "RecoveryRequired",
        "terminal eviction schedule lost its queue entry",
    )?;
    let decision = updated.body["lifecycle"].as_str().ok_or_else(invalid)?;
    db.audit(policy::OWNER, method, Some(&record.id), decision)?;
    Ok(updated)
}

pub(crate) fn cancel(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor)?;
    let request: Conditional = decode(params)?;
    let (record, state) = get_scoped(db, &request.id)?;
    record.cas(&request.expected_resource_version)?;
    require(
        state.lifecycle == Phase::Scheduled,
        "PreconditionFailed",
        "only a pending result eviction schedule can be cancelled",
    )?;
    Ok(view(&settle(db, &record, state, Settlement::Cancelled)?))
}

pub(crate) fn advance(db: &Db<'_>, cursor: &mut Cursor) -> Result<bool> {
    require(
        !db.c.is_autocommit(),
        "RecoveryRequired",
        "eviction scheduler requires an authority transaction",
    )?;
    let exists: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_result_eviction_schedules)",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let clock = Clock::sample()?;
    let scope = current_scope(db, &clock)?;
    let pending =
        db.c.query_row(
            "SELECT schedule,scope_digest,due_boot_ms FROM pending_result_eviction_schedules
             WHERE scope_digest<>?1 OR due_boot_ms<=?2
             ORDER BY (due_boot_ms,schedule)<=(?3,?4), due_boot_ms,schedule LIMIT 1",
            params![
                scope,
                clock.boot.millis,
                cursor.due_boot_ms,
                cursor.schedule
            ],
            Pending::row,
        )
        .optional()?;
    let Some(pending) = pending else {
        *cursor = Cursor::default();
        return Ok(false);
    };
    // Advance the process-local scan even on transaction rollback. At most
    // MAX_PENDING rows participate, so a faulty schedule cannot starve its peers.
    cursor.due_boot_ms = pending.due_boot_ms;
    cursor.schedule.clone_from(&pending.schedule);
    let record = find(db.c, &pending.schedule)?.ok_or_else(invalid)?;
    let (plan, state) = verify_record(&record)?;
    check_projection(&record, &plan, &state, Some(pending))?;
    let owner_active = match authorize(db, &plan.owner) {
        Ok(()) => true,
        Err(error) if error.0.code == "PermissionDenied" => false,
        Err(error) => return Err(error),
    };
    let settlement = if !owner_active || scope != plan_scope(&plan) {
        Settlement::Suspended
    } else if let Some(code) = exclusion(db, &plan)? {
        Settlement::Rejected(code)
    } else {
        let candidates = batch::check_candidates(db, &plan)?;
        let clock = Clock::sample()?;
        if current_scope(db, &clock)? != plan_scope(&plan) {
            Settlement::Suspended
        } else {
            require(
                clock.boot.millis >= state.due_boot_ms,
                "RecoveryRequired",
                "eviction schedule clock regressed before execution",
            )?;
            authorize(db, &plan.owner)?;
            let batch = batch::apply_delayed(db, &plan, candidates, &record)?;
            Settlement::Evicted {
                batch_ref: decode(&batch["reference"])?,
                clock: clock.boot.millis,
            }
        }
    };
    settle(db, &record, state, settlement)?;
    Ok(true)
}
