//! Bounded provider custody and inference outcome projections for maintenance.
use super::{inference, lease::Clock, state};
use crate::{
    Error, Result,
    db::{Db, Doc, row},
    decode, err, now, require,
};
use chrono::{DateTime, Utc};
use psios_api::{BlockerPage, ModelLeaseBlocker, ModelOutcomeBlocker, ResourceRef};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) const SCHEMA: &str = "
CREATE INDEX maintenance_model_lease_boot ON session_turns(boot_id,deadline_boot_ms,lease)
    WHERE receipt IS NULL;
CREATE INDEX maintenance_model_lease_utc ON session_turns(julianday(expires),lease)
    WHERE receipt IS NULL;
CREATE INDEX maintenance_model_unknown ON model_inference_leases(lease) WHERE status='Unknown';
CREATE INDEX maintenance_model_issue ON versions(id,(body->>'current_lease'),rev)
    WHERE kind='Session' AND body->>'lifecycle'='ModelInFlight';";

struct Strict {
    revision: i64,
    model: String,
    time_ns: String,
    clock_matches: bool,
    status: String,
    source: Option<String>,
    receipt: Option<Value>,
}

struct Turn {
    session: String,
    turn: u32,
    expires: String,
    boot_id: Option<String>,
    deadline: Option<i64>,
    time_ns: Option<String>,
    reported: bool,
    strict: Option<Strict>,
}

fn invalid() -> Error {
    err(
        "RecoveryRequired",
        "model blocker disagrees with its durable lease",
    )
}

fn leases_in(db: &Db<'_>, sql: &str, args: impl rusqlite::Params) -> Result<Vec<String>> {
    Ok(db
        .c
        .prepare_cached(sql)?
        .query_map(args, |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn turn(db: &Db<'_>, lease: &str) -> Result<Turn> {
    db.c.prepare_cached(
        "SELECT t.session,t.turn,t.expires,t.boot_id,t.deadline_boot_ms,a.time_ns,t.receipt IS NOT NULL,
                i.session_revision,i.model,i.time_ns,
                i.boot_id IS t.boot_id AND i.deadline_boot_ms IS t.deadline_boot_ms
                  AND (a.lease IS NULL OR a.time_ns=i.time_ns),
                i.status,i.source,i.receipt
         FROM session_turns t LEFT JOIN model_inference_leases i USING(lease)
         LEFT JOIN model_lease_times a USING(lease) WHERE t.lease=?1",
    )?
    .query_row([lease], |r| {
        let strict = if let Some(revision) = r.get(7)? {
            Some(Strict {
                revision,
                model: r.get(8)?,
                time_ns: r.get(9)?,
                clock_matches: r.get(10)?,
                status: r.get(11)?,
                source: r.get(12)?,
                receipt: r.get(13)?,
            })
        } else {
            None
        };
        Ok(Turn {
            session: r.get(0)?,
            turn: r.get(1)?,
            expires: r.get(2)?,
            boot_id: r.get(3)?,
            deadline: r.get(4)?,
            time_ns: r.get(5)?,
            reported: r.get(6)?,
            strict,
        })
    })
    .optional()?
    .ok_or_else(invalid)
}

fn current_session(db: &Db<'_>, turn: &Turn) -> Result<Doc> {
    let session = db.find(&turn.session)?.ok_or_else(invalid)?;
    require(
        session.kind == "Session",
        "RecoveryRequired",
        "model lease lost its Session",
    )?;
    Ok(session)
}

fn pinned(db: &Db<'_>, reference: &Value) -> Result<Doc> {
    db.resolve(reference).map_err(|error| {
        if matches!(
            error.0.code.as_str(),
            "InvalidArgument" | "PreconditionFailed"
        ) {
            invalid()
        } else {
            error
        }
    })
}

fn original_model(
    db: &Db<'_>,
    session: &Doc,
    turn: &Turn,
    lease: &str,
) -> Result<Option<(Doc, ResourceRef)>> {
    let issue = if let Some(strict) = &turn.strict {
        let issue =
            db.c.prepare_cached("SELECT * FROM versions WHERE id=?1 AND rev=?2")?
                .query_row(params![session.id, strict.revision], row)
                .optional()?
                .ok_or_else(invalid)?;
        Some(issue)
    } else {
        // A later Session revision or successor may use another provider.
        // Look up the first checkpoint that actually named this legacy lease.
        db.c.prepare_cached(
            "SELECT * FROM versions INDEXED BY maintenance_model_issue
             WHERE kind='Session' AND body->>'lifecycle'='ModelInFlight'
               AND id=?1 AND body->>'current_lease'=?2 ORDER BY rev LIMIT 1",
        )?
        .query_row(params![session.id, lease], row)
        .optional()?
    };
    let Some(issue) = issue else { return Ok(None) };
    require(
        issue.kind == "Session"
            && issue.owner == session.owner
            && issue.rev <= session.rev
            && state(&issue) == "ModelInFlight"
            && turn.turn > 0
            && issue.body["turn"] == turn.turn
            && issue.body["current_lease"] == lease
            && issue.body["lease_until"] == turn.expires,
        "RecoveryRequired",
        "model lease issue checkpoint disagrees with its original Session",
    )?;
    let reference = &issue.spec["model_ref"];
    if reference.is_null() && turn.strict.is_none() {
        return Ok(None);
    }
    let model = pinned(db, reference)?;
    require(
        model.kind == "Model"
            && turn.strict.as_ref().map_or_else(
                || !inference::strict(&model),
                |strict| {
                    strict.clock_matches && strict.model == model.id && inference::strict(&model)
                },
            ),
        "RecoveryRequired",
        "model lease lost its pinned original Model",
    )?;
    Ok(Some((issue, decode(&model.reference())?)))
}

fn lease_blocker(
    db: &Db<'_>,
    lease: &str,
    clock: &Clock,
    utc: DateTime<Utc>,
) -> Result<Option<ModelLeaseBlocker>> {
    let turn = turn(db, lease)?;
    let strict_status = turn.strict.as_ref().map(|strict| strict.status.as_str());
    if strict_status == Some("NotStarted") {
        return Ok(None);
    }
    require(
        !turn.reported && matches!(strict_status, None | Some("Pending" | "Unknown")),
        "RecoveryRequired",
        "model blocker names an already reported lease",
    )?;
    let expires = DateTime::parse_from_rfc3339(&turn.expires).map_err(|_| invalid())?;
    let local_clock = turn.boot_id.as_deref() == Some(clock.boot.boot_id.as_str())
        && turn.time_ns.as_deref().is_none_or(|ns| ns == clock.time_ns)
        && turn
            .strict
            .as_ref()
            .is_none_or(|strict| strict.time_ns == clock.time_ns);
    // Match provider slot accounting, including its UTC estimate when the
    // issuing Linux boot or time namespace is no longer available.
    let residual = if local_clock {
        turn.deadline.ok_or_else(invalid)? > clock.boot.millis
    } else {
        expires.to_utc() > utc
    };
    let pending = strict_status == Some("Pending");
    if !residual && !pending {
        return Ok(None);
    }
    let session = current_session(db, &turn)?;
    let is_current = state(&session) == "ModelInFlight" && session.body["current_lease"] == lease;
    require(
        !pending || is_current,
        "RecoveryRequired",
        "unreported strict lease no longer owns its Session",
    )?;
    let label = if is_current && clock.current(db, &session)? {
        "Current"
    } else if residual {
        "Residual"
    } else {
        "ElapsedPending"
    };
    let model = original_model(db, &session, &turn, lease)?;
    Ok(Some(ModelLeaseBlocker {
        session_ref: decode(&session.reference())?,
        model_ref: model.map(|(_, reference)| reference),
        lease_id: lease.to_owned(),
        state: label.into(),
        expires_at: turn.expires,
    }))
}

pub(crate) fn leases(db: &Db<'_>, limit: u32) -> Result<BlockerPage<ModelLeaseBlocker>> {
    let clock = Clock::sample()?;
    let utc_sample = now();
    let utc = DateTime::parse_from_rfc3339(&utc_sample)
        .map_err(|_| invalid())?
        .to_utc();
    let cap = i64::from(limit) + 1;
    // Bound indexed candidates before reading any Session/history. The time
    // indexes can overlap and may include settled NotStarted attempts. Mark
    // truncated if a candidate window fills even when some rows are excluded.
    let pending = leases_in(
        db,
        "SELECT lease FROM model_inference_leases INDEXED BY model_inference_pending
         WHERE status='Pending' ORDER BY deadline_boot_ms,lease LIMIT ?1",
        [cap],
    )?;
    let same_boot = leases_in(
        db,
        "SELECT lease FROM session_turns INDEXED BY maintenance_model_lease_boot
         WHERE receipt IS NULL AND boot_id=?1 AND deadline_boot_ms>?2
         ORDER BY deadline_boot_ms,lease LIMIT ?3",
        params![clock.boot.boot_id, clock.boot.millis, cap],
    )?;
    let utc_candidates = leases_in(
        db,
        "SELECT lease FROM session_turns INDEXED BY maintenance_model_lease_utc
         WHERE receipt IS NULL AND julianday(expires)>julianday(?1)
         ORDER BY julianday(expires),lease LIMIT ?2",
        params![utc_sample, cap],
    )?;
    let mut page = BlockerPage {
        items: Vec::new(),
        truncated: [&pending, &same_boot, &utc_candidates]
            .iter()
            .any(|candidates| candidates.len() > limit as usize),
    };
    let mut seen = BTreeSet::new();
    for lease in pending.iter().chain(&same_boot).chain(&utc_candidates) {
        if seen.insert(lease)
            && let Some(blocker) = lease_blocker(db, lease, &clock, utc)?
        {
            if page.items.len() == limit as usize {
                page.truncated = true;
                break;
            }
            page.items.push(blocker);
        }
    }
    Ok(page)
}

pub(crate) fn unknown(db: &Db<'_>, limit: u32) -> Result<BlockerPage<ModelOutcomeBlocker>> {
    let candidates = leases_in(
        db,
        "SELECT lease FROM model_inference_leases INDEXED BY maintenance_model_unknown
         WHERE status='Unknown' ORDER BY lease LIMIT ?1",
        [i64::from(limit) + 1],
    )?;
    let mut page = BlockerPage {
        items: Vec::new(),
        truncated: candidates.len() > limit as usize,
    };
    for lease in candidates.into_iter().take(limit as usize) {
        let turn = turn(db, &lease)?;
        let session = current_session(db, &turn)?;
        let (issued, model_ref) =
            original_model(db, &session, &turn, &lease)?.ok_or_else(invalid)?;
        let strict = turn.strict.as_ref().ok_or_else(invalid)?;
        let source = strict.source.as_ref().ok_or_else(invalid)?;
        let receipt = strict.receipt.as_ref().ok_or_else(invalid)?;
        let settled = pinned(db, &receipt["session_ref"])?;
        let model = &issued.spec["model_ref"];
        let lifecycle = match source.as_str() {
            "SessionCancelled" => "Cancelled",
            "Provider" | "LeaseExpired" => "Paused",
            _ => return Err(invalid()),
        };
        require(
            strict.status == "Unknown"
                && !turn.reported
                && settled.kind == "Session"
                && settled.id == session.id
                && settled.owner == session.owner
                && settled.spec == issued.spec
                && issued.rev < settled.rev
                && settled.rev <= session.rev
                && state(&settled) == lifecycle
                && settled.body["turn"] == turn.turn
                && settled.body["current_lease"].is_null()
                && settled.body["outcome_knowledge"] == "Unknown"
                && settled.body["model_inference_failure"]
                    == json!({"lease_id":lease,"model_ref":model,"outcome":"Unknown","source":source})
                && *receipt
                    == json!({"session_ref":settled.reference(),"model_ref":model,
                    "lease_id":lease,"outcome":"Unknown","lifecycle":lifecycle,"outcome_knowledge":"Unknown"}),
            "RecoveryRequired",
            "unknown model outcome lost its exact lease settlement",
        )?;
        page.items.push(ModelOutcomeBlocker {
            session_ref: decode(&session.reference())?,
            model_ref,
            lease_id: lease,
            source: source.clone(),
        });
    }
    Ok(page)
}
