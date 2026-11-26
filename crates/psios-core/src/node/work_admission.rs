//! Persisted node-wide control of fresh Task and model work.
use crate::{
    Empty, Error, Result,
    db::{Db, Doc, find, row},
    decode, err, now, policy, require,
};
use psios_api::{
    ModelWorkCounts, NodeWorkAdmissionBlockers, TaskWorkCounts, WorkAdmissionMode,
    WorkAdmissionState,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

mod blockers;

pub(crate) const FORMAT: u32 = super::journal::SINCE_FORMAT + 1;
pub(crate) const BLOCKERS_FORMAT: u32 = FORMAT + 1;
const KIND: &str = "NodeWorkAdmission";
const ID: &str = "local-node-work-admission";
const INITIAL_REASON: &str = "Initial local work admission";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    expected_resource_version: String,
    mode: WorkAdmissionMode,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveBlockers {
    expected_resource_version: String,
    limit: Option<u32>,
}

fn valid_reason(reason: &str) -> bool {
    !reason.trim().is_empty() && reason.len() <= 256 && !reason.chars().any(char::is_control)
}

fn validate(record: &Doc, authority: &str) -> Result<WorkAdmissionState> {
    let invalid = || err("RecoveryRequired", "invalid durable node work admission");
    let state: WorkAdmissionState = decode(&record.body).map_err(|_| invalid())?;
    let initial = record.rev == 1;
    require(
        record.id == ID
            && record.kind == KIND
            && record.owner == policy::OWNER
            && record.generation == 1
            && record.rev >= 1
            && record.spec == json!({"authority_id":authority})
            && valid_reason(&state.reason)
            && chrono::DateTime::parse_from_rfc3339(&state.changed_at).is_ok()
            && if initial {
                state.changed_by == policy::SYSTEM
                    && state.reason == INITIAL_REASON
                    && state.mode == WorkAdmissionMode::Open
            } else {
                state.changed_by == policy::OWNER
            },
        "RecoveryRequired",
        "invalid durable node work admission",
    )?;
    Ok(state)
}

fn current(db: &Db<'_>) -> Result<(Doc, WorkAdmissionState)> {
    let record = find(db.c, ID)?
        .ok_or_else(|| err("RecoveryRequired", "local work admission is missing"))?;
    let state = validate(&record, &db.meta("authority")?)?;
    Ok((record, state))
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    let state = WorkAdmissionState {
        mode: WorkAdmissionMode::Open,
        reason: INITIAL_REASON.into(),
        changed_by: policy::SYSTEM.into(),
        changed_at: now(),
    };
    db.create(
        KIND,
        ID,
        policy::OWNER,
        json!({"authority_id":db.meta("authority")?}),
        serde_json::to_value(state)?,
    )?;
    Ok(())
}

pub(crate) fn draining(db: &Db<'_>) -> Result<bool> {
    Ok(current(db)?.1.mode == WorkAdmissionMode::Draining)
}

pub(crate) fn closed() -> Error {
    let mut error = err("PreconditionFailed", "local work admission is draining");
    error.0.retryable = true;
    error.0.retry_condition = "SameRequestAfterBackoff".into();
    error
}

fn view(db: &Db<'_>, record: &Doc, admission: WorkAdmissionState) -> Result<Value> {
    let mut groups = db.c.prepare_cached(
        "SELECT phase,count(*) FROM queue INDEXED BY queue_admitted
         WHERE phase<>'Done' GROUP BY phase",
    )?;
    let phases = groups
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, i64>>>()?;
    let recovery_admissions =
        db.c.query_row("SELECT count(*) FROM recovery_admissions", [], |row| {
            row.get(0)
        })?;
    let task_counts = TaskWorkCounts {
        phases,
        recovery_admissions,
    };
    let model_counts = db.c.query_row(
        "SELECT
            count(*) FILTER (WHERE body->>'lifecycle'='ModelInFlight'),
            count(*) FILTER (WHERE body->>'lifecycle'='Paused' AND body->>'pause_reason'='ModelOutcomeUnknown')
         FROM current WHERE kind='Session'",
        [],
        |row| {
            Ok(ModelWorkCounts {
                current_leases: row.get(0)?,
                paused_outcomes_unknown: row.get(1)?,
            })
        },
    )?;
    Ok(
        json!({"reference":record.reference(), "admission":admission,
        "task_counts":task_counts, "model_counts":model_counts}),
    )
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    policy::active(db, actor)?;
    let _: Empty = decode(params)?;
    let (record, state) = current(db)?;
    view(db, &record, state)
}

pub(crate) fn observe_blockers(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    policy::active(db, actor)?;
    let request: ObserveBlockers = decode(params)?;
    let limit = request.limit.unwrap_or(16);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "work admission blocker limit must be 1 to 64 per group",
    )?;
    let (record, _) = current(db)?;
    record.cas(&request.expected_resource_version)?;
    let response = NodeWorkAdmissionBlockers {
        reference: decode(&record.reference())?,
        observed_at: now(),
        tasks: blockers::tasks(db, limit)?,
        exclusive_holds: blockers::exclusive_holds(db, limit)?,
        volume_read_holds: blockers::volume_read_holds(db, limit)?,
        model_leases: crate::agent::work_admission_status::leases(db, limit)?,
        model_outcomes_unknown: crate::agent::work_admission_status::unknown(db, limit)?,
    };
    Ok(serde_json::to_value(response)?)
}

pub(crate) fn set(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    policy::active(db, actor)?;
    let request: Set = decode(params)?;
    require(
        valid_reason(&request.reason),
        "InvalidArgument",
        "work admission reason must be 1 to 256 bytes without control characters",
    )?;
    let (record, state) = current(db)?;
    record.cas(&request.expected_resource_version)?;
    if request.mode == state.mode {
        return view(db, &record, state);
    }
    let state = WorkAdmissionState {
        mode: request.mode,
        reason: request.reason,
        changed_by: actor.into(),
        changed_at: now(),
    };
    let record = db.status(&record, serde_json::to_value(&state)?)?;
    let decision = match state.mode {
        WorkAdmissionMode::Open => "Open",
        WorkAdmissionMode::Draining => "Draining",
    };
    db.audit(actor, "Node.WorkAdmission.Set", Some(ID), decision)?;
    view(db, &record, state)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
            r.get(0)
        })?;
    let mut statements =
        conn.prepare("SELECT * FROM versions WHERE id=?1 OR kind=?2 ORDER BY rev")?;
    let mut previous: Option<WorkAdmissionMode> = None;
    for (index, rev) in statements.query_map(params![ID, KIND], row)?.enumerate() {
        let rev = rev?;
        let state = validate(&rev, &authority)?;
        require(
            usize::try_from(rev.rev).ok() == index.checked_add(1) && previous != Some(state.mode),
            "RecoveryRequired",
            "invalid node work admission revision sequence",
        )?;
        previous = Some(state.mode);
    }
    require(
        previous.is_some(),
        "RecoveryRequired",
        "local work admission is missing",
    )
}
