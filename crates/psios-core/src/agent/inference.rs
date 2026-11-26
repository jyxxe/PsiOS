//! A strict claim is authorization to dispatch exactly one inference attempt.
use super::*;
use crate::db::{find, resolve, row};
use lease::Clock;
use psios_api::{ModelFailureReport, ModelInferenceOutcome};
use rusqlite::Connection;

pub(crate) const FORMAT: u32 = apps::capture::CAPACITY_FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("inference.sql");
const STRICT: &str = "Strict";
const UNKNOWN: &str = "Unknown";
const NOT_STARTED: &str = "NotStarted";
const EXPIRE_BATCH: u32 = 16;

#[derive(Deserialize, Serialize)]
pub(super) enum Policy {
    Strict,
}

pub(super) fn strict(model: &Doc) -> bool {
    model.spec["inference_outcome_policy"] == STRICT
}

pub(super) fn decorate_claim(
    db: &Db<'_>,
    model: &Doc,
    lease: &str,
    reply: &mut Value,
) -> Result<()> {
    if strict(model) {
        reply["inference_outcome_policy"] = json!(STRICT);
        let witnessed: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_provider_authorities WHERE lease=?1)",
            [lease],
            |row| row.get(0),
        )?;
        if witnessed {
            reply["late_decision_observation"] = json!(true);
            reply["decision_rejection_policy"] =
                json!(psios_api::ModelDecisionRejectionPolicy::UndeclaredToolV1);
            reply["model_ref"] = model.reference();
        }
    }
    Ok(())
}

pub(super) fn bind(
    db: &Db<'_>,
    session: &Doc,
    model: &Doc,
    clock: &Clock,
    deadline: i64,
    authority: Option<&provider_authority::Binding>,
) -> Result<()> {
    if let Some(authority) = authority {
        require(
            strict(model),
            "RecoveryRequired",
            "unexpected provider authority on a legacy lease",
        )?;
        db.c.execute(
            "INSERT INTO model_inference_leases(lease,session_revision,model,boot_id,time_ns,deadline_boot_ms)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![session.body["current_lease"].as_str(),session.rev,model.id,clock.boot.boot_id,clock.time_ns,deadline],
        )?;
        authority.persist(db, current_lease(session)?)?;
    } else {
        require(
            !strict(model),
            "RecoveryRequired",
            "strict lease has no issuing provider authority",
        )?;
    }
    Ok(())
}

fn current_lease(session: &Doc) -> Result<&str> {
    session.body["current_lease"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "strict model session has no issued lease",
        )
    })
}

pub(super) fn in_flight(db: &Db<'_>, session: &Doc) -> Result<bool> {
    if state(session) != "ModelInFlight" {
        return Ok(false);
    }
    let model = db.resolve(&session.spec["model_ref"])?;
    if !strict(&model) {
        return Ok(false);
    }
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM model_inference_leases i JOIN session_turns t USING(lease)
         WHERE i.lease=?1 AND i.model=?2 AND t.session=?3 AND i.status='Pending')",
        params![current_lease(session)?, model.id, session.id],
        |r| r.get(0),
    )?;
    require(
        pending,
        "RecoveryRequired",
        "strict model lease has no pending checkpoint",
    )?;
    Ok(true)
}

#[derive(Clone, Copy)]
enum Source {
    Provider,
    LeaseExpired,
    SessionCancelled,
}

impl Source {
    fn text(self) -> &'static str {
        match self {
            Self::Provider => "Provider",
            Self::LeaseExpired => "LeaseExpired",
            Self::SessionCancelled => "SessionCancelled",
        }
    }

    fn method(self) -> &'static str {
        match self {
            Self::Provider => "Model.ReportFailure",
            Self::LeaseExpired => "Model.LeaseExpired",
            Self::SessionCancelled => "Model.LeaseCancelled",
        }
    }
}

fn settle(
    db: &Db<'_>,
    session: &Doc,
    actor: &str,
    outcome: &str,
    source: Source,
) -> Result<(Doc, Value)> {
    let lease = current_lease(session)?;
    let model = &session.spec["model_ref"];
    let mut body = session.body.clone();
    body["lifecycle"] = json!(if matches!(source, Source::SessionCancelled) {
        "Cancelled"
    } else {
        "Paused"
    });
    body["pause_reason"] = json!(if outcome == UNKNOWN {
        "ModelOutcomeUnknown"
    } else {
        "ModelNotStarted"
    });
    body["outcome_knowledge"] = json!(outcome);
    body["model_inference_failure"] =
        json!({"lease_id":lease,"model_ref":model,"outcome":outcome,"source":source.text()});
    body["current_lease"] = Value::Null;
    body.as_object_mut().unwrap().remove("current_usage_ref");
    body.as_object_mut().unwrap().remove("lease_until");
    let saved = db.status(session, body)?;
    let receipt = json!({"session_ref":saved.reference(),"model_ref":model,"lease_id":lease,
        "outcome":outcome,"lifecycle":state(&saved),"outcome_knowledge":outcome});
    require(
        db.c.execute(
            "UPDATE model_inference_leases SET status=?2,source=?3,receipt=?4 WHERE lease=?1 AND status='Pending'",
            params![lease,outcome,source.text(),receipt],
        )? == 1,
        "RecoveryRequired",
        "strict model failure disagrees with its dispatched lease",
    )?;
    db.audit(actor, source.method(), Some(&session.id), outcome)?;
    Ok((saved, receipt))
}

pub(super) fn expire_addressed(db: &Db<'_>, session: &Doc, clock: &Clock) -> Result<Option<Doc>> {
    if in_flight(db, session)? && !clock.current(db, session)? {
        return Ok(Some(
            settle(db, session, policy::SYSTEM, UNKNOWN, Source::LeaseExpired)?.0,
        ));
    }
    Ok(None)
}

fn expire_due(db: &Db<'_>, clock: &Clock, model: Option<&str>) -> Result<()> {
    let sessions =
        db.c.prepare(
            "SELECT t.session FROM model_inference_leases i JOIN session_turns t USING(lease)
         WHERE i.status='Pending' AND (?1 IS NULL OR i.model=?1)
         AND (i.boot_id<>?2 OR i.time_ns<>?3 OR i.deadline_boot_ms<=?4)
         ORDER BY i.deadline_boot_ms,i.lease LIMIT ?5",
        )?
        .query_map(
            params![
                model,
                clock.boot.boot_id,
                clock.time_ns,
                clock.boot.millis,
                EXPIRE_BATCH
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in sessions {
        let session = db.of(&id, "Session")?;
        require(
            expire_addressed(db, &session, clock)?.is_some(),
            "RecoveryRequired",
            "strict model expiry disagrees with its dispatched lease",
        )?;
    }
    Ok(())
}

pub(crate) fn watchdog(db: &Db<'_>) -> Result<()> {
    expire_due(db, &Clock::sample()?, None)
}

pub(super) fn expire_queued(db: &Db<'_>, clock: &Clock, model: &Doc) -> Result<()> {
    if strict(model) {
        expire_due(db, clock, Some(&model.id))?;
    }
    Ok(())
}

pub(super) fn cancel(db: &Db<'_>, session: &Doc, actor: &str) -> Result<Option<Doc>> {
    if in_flight(db, session)? {
        return Ok(Some(
            settle(db, session, actor, UNKNOWN, Source::SessionCancelled)?.0,
        ));
    }
    Ok(None)
}

pub(super) fn failure_receipt(conn: &Connection, lease: &str) -> Result<Option<Value>> {
    Ok(conn
        .query_row(
            "SELECT receipt FROM model_inference_leases WHERE lease=?1 AND receipt IS NOT NULL",
            [lease],
            |r| r.get(0),
        )
        .optional()?)
}

pub(super) fn responded(db: &Db<'_>, model: &Doc, lease: &str) -> Result<()> {
    if strict(model) {
        require(
            db.c.execute("UPDATE model_inference_leases SET status='Responded' WHERE lease=?1 AND status='Pending'", [lease])? == 1,
            "RecoveryRequired", "model decision disagrees with its strict dispatch",
        )?;
    }
    Ok(())
}

pub fn report(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let report: ModelFailureReport = decode(request)?;
    label(&report.lease_id)?;
    let session = db.of(&report.session_id, "Session")?;
    let model = provider_model(db, &session, actor)?;
    require(
        model.reference() == json!(report.model_ref),
        "PreconditionFailed",
        "model report is outside the pinned model revision",
    )?;
    require(
        strict(&model),
        "UnsupportedCapability",
        "model has no strict failure reporting contract",
    )?;
    let outcome = match report.outcome {
        ModelInferenceOutcome::NotStarted => NOT_STARTED,
        ModelInferenceOutcome::Unknown => UNKNOWN,
    };
    let record: Option<(String, Option<String>, Option<Value>)> = db.c.query_row(
        "SELECT i.status,i.source,i.receipt FROM model_inference_leases i JOIN session_turns t USING(lease)
         WHERE i.lease=?1 AND i.model=?2 AND t.session=?3", params![report.lease_id,model.id,session.id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    if let Some((previous, Some(source), Some(receipt))) = &record
        && source == Source::Provider.text()
    {
        require(
            previous == outcome,
            "IdempotencyConflict",
            "lease already has a different model failure report",
        )?;
        return Ok(receipt.clone());
    }
    let clock = Clock::sample()?;
    require(
        record.is_some_and(|(status, _, _)| status == "Pending")
            && state(&session) == "ModelInFlight"
            && session.body["current_lease"] == report.lease_id
            && clock.current(db, &session)?,
        "Fenced",
        "model failure report outside current strict lease",
    )?;
    provider_authority::require_current(db, &session, &model, &report.lease_id)?;
    if outcome == NOT_STARTED {
        let budget = session.body["budget_ref"]["resource_id"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "model budget reference is missing"))?;
        let elapsed = model_time::settle(db, &clock, &report.lease_id, budget)?;
        if elapsed.is_some() {
            settle_usage(db, &session, &model, actor, &report.lease_id, None, elapsed)?;
        }
    }
    Ok(settle(db, &session, actor, outcome, Source::Provider)?.1)
}

pub(super) fn not_started(session: &Doc) -> bool {
    state(session) == "Paused"
        && session.body["pause_reason"] == "ModelNotStarted"
        && session.body["outcome_knowledge"] == NOT_STARTED
}

pub(super) fn check_resume(db: &Db<'_>, session: &Doc, model: &Doc) -> Result<()> {
    require(
        has_turn(session),
        "QuotaExceeded",
        "session turn limit reached",
    )?;
    let lease = session.body["model_inference_failure"]["lease_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "model failure lease is missing"))?;
    let receipt = failure_receipt(db.c, lease)?
        .ok_or_else(|| err("RecoveryRequired", "model failure receipt is missing"))?;
    let paused = db.resolve(&receipt["session_ref"])?;
    require(
        strict(model)
            && paused.id == session.id
            && receipt["model_ref"] == model.reference()
            && receipt["lease_id"] == lease
            && receipt["outcome"] == NOT_STARTED
            && paused.body["model_inference_failure"] == session.body["model_inference_failure"],
        "RecoveryRequired",
        "model failure receipt disagrees with this session",
    )?;
    let budget = session.body["budget_ref"]["resource_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "model budget reference is missing"))?;
    let lease_ms = model.spec["lease_seconds"].as_i64().unwrap() * 1000;
    model_time::window(db, budget, &Clock::sample()?, lease_ms)?
        .map_err(model_time::ClaimRefusal::error)?;
    Ok(())
}

pub(super) fn predecessor(conn: &Connection, version: &Doc) -> Result<Doc> {
    let missing = || {
        err(
            "RecoveryRequired",
            "strict model session transition has no predecessor",
        )
    };
    let prior_revision = version
        .rev
        .checked_sub(1)
        .filter(|rev| *rev > 0)
        .ok_or_else(missing)?;
    conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![version.id, prior_revision],
        row,
    )
    .optional()?
    .filter(|prior| {
        prior.kind == "Session" && prior.owner == version.owner && prior.spec == version.spec
    })
    .ok_or_else(missing)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_turns t JOIN current s ON s.id=t.session
         JOIN versions m ON m.id=s.spec->'model_ref'->>'resource_id' AND m.rev=1 AND m.kind='Model'
         LEFT JOIN model_inference_leases i USING(lease)
         WHERE (m.spec->>'inference_outcome_policy' IS 'Strict')<>(i.lease IS NOT NULL))",
        [],
        |r| r.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "strict model lease index is incomplete",
    )?;
    let mut statement = conn.prepare(
        "SELECT i.lease,t.session,t.turn,i.session_revision,i.model,i.status,i.source,i.receipt,
            i.boot_id=t.boot_id AND i.deadline_boot_ms=t.deadline_boot_ms
                AND (a.lease IS NULL OR i.time_ns=a.time_ns),t.expires,t.receipt IS NOT NULL,a.settled_ms
         FROM model_inference_leases i JOIN session_turns t USING(lease)
         LEFT JOIN model_lease_times a USING(lease)",
    )?;
    let mut rows = statement.query([])?;
    while let Some(record) = rows.next()? {
        let lease: String = record.get(0)?;
        let session: String = record.get(1)?;
        let turn: u32 = record.get(2)?;
        let issued = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![session, record.get::<_, i64>(3)?],
            row,
        )?;
        let before_issue = predecessor(conn, &issued)?;
        let model = resolve(conn, &issued.spec["model_ref"])?;
        let usage = resolve(conn, &issued.body["current_usage_ref"])?;
        let status: String = record.get(5)?;
        let source: Option<String> = record.get(6)?;
        let receipt: Option<Value> = record.get(7)?;
        require(
            issued.kind == "Session"
                && turn > 0
                && state(&before_issue) == "AwaitingModel"
                && before_issue.body["current_lease"].is_null()
                && before_issue.body["turn"].as_u64() == u64::from(turn).checked_sub(1)
                && state(&issued) == "ModelInFlight"
                && issued.body["current_lease"] == lease
                && issued.body["turn"] == turn
                && issued.body["lease_until"] == record.get::<_, String>(9)?
                && model.kind == "Model"
                && strict(&model)
                && model.id == record.get::<_, String>(4)?
                && usage.kind == "ModelUsage"
                && usage.body["session_id"] == session
                && usage.body["call_id"] == lease
                && usage.body["model_ref"] == model.reference()
                && record.get::<_, bool>(8)?
                && (status == "Responded") == record.get::<_, bool>(10)?,
            "RecoveryRequired",
            "strict model dispatch checkpoint disagrees with its lease",
        )?;
        if status == "Pending" {
            let current = find(conn, &session)?
                .ok_or_else(|| err("RecoveryRequired", "strict model session is missing"))?;
            require(
                state(&current) == "ModelInFlight" && current.body["current_lease"] == lease,
                "RecoveryRequired",
                "unreported strict model lease is no longer current",
            )?;
        }
        if let Some(receipt) = receipt {
            let settled = resolve(conn, &receipt["session_ref"])?;
            let before_settlement = predecessor(conn, &settled)?;
            let lifecycle = if source.as_deref() == Some("SessionCancelled") {
                "Cancelled"
            } else {
                "Paused"
            };
            require(
                receipt
                    == json!({"session_ref":settled.reference(),"model_ref":model.reference(),"lease_id":lease,
                    "outcome":status,"lifecycle":lifecycle,"outcome_knowledge":status})
                    && settled.kind == "Session"
                    && settled.id == issued.id
                    && settled.rev > issued.rev
                    && settled.spec == issued.spec
                    && before_settlement.rev >= issued.rev
                    && state(&before_settlement) == "ModelInFlight"
                    && before_settlement.body["current_lease"] == lease
                    && before_settlement.body["turn"] == turn
                    && before_settlement.body["current_usage_ref"]
                        == issued.body["current_usage_ref"]
                    && before_settlement.body["lease_until"] == issued.body["lease_until"]
                    && state(&settled) == lifecycle
                    && settled.body["turn"] == turn
                    && settled.body["current_lease"].is_null()
                    && settled.body["outcome_knowledge"] == status
                    && settled.body["model_inference_failure"]
                        == json!({"lease_id":lease,"model_ref":model.reference(),"outcome":status,"source":source})
                    && (status != UNKNOWN || record.get::<_, Option<i64>>(11)?.is_none()),
                "RecoveryRequired",
                "strict model failure receipt disagrees with its settlement",
            )?;
        }
    }
    super::rejected_decision::verify(conn)
}
