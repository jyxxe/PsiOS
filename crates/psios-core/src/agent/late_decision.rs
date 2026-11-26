//! A late observation records a provider statement without settling the lease.
use super::{Decision, Respond, inference, provider_authority, state};
use crate::{
    Error, Result,
    db::{Db, Doc, resolve, row},
    decode, err, label, now, require,
};
use psios_api::{ResourceRef, canonical, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::node::work_admission::BLOCKERS_FORMAT + 1;
pub(crate) const SCHEMA: &str = "
CREATE TABLE model_late_decisions(
 lease TEXT PRIMARY KEY REFERENCES model_inference_leases(lease),
 observation TEXT NOT NULL,
 observation_digest TEXT NOT NULL
) STRICT;";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    lease_id: String,
    model_ref: ResourceRef,
    issued_session_ref: ResourceRef,
    unknown_session_ref: ResourceRef,
    provider_principal_id: String,
    provider_authority_digest: String,
    decision_digest: String,
    output_bytes: u64,
    observed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage_digest: Option<String>,
}

struct Attempt {
    revision: i64,
    model_id: String,
    turn: u32,
    expires: String,
    status: String,
    source: Option<String>,
    failure: Option<Value>,
    unsettled: bool,
    clock_matches: bool,
}

fn unavailable() -> Error {
    err(
        "PermissionDenied",
        "late decision outside original provider scope",
    )
}

fn fenced() -> Error {
    err("Fenced", "lease is outside late decision observation")
}

fn invalid() -> Error {
    err(
        "RecoveryRequired",
        "late decision has inconsistent lease evidence",
    )
}

fn attempt(conn: &Connection, session: &str, lease: &str) -> Result<Option<Attempt>> {
    Ok(conn
        .prepare_cached(
            "SELECT i.session_revision,i.model,t.turn,t.expires,i.status,i.source,i.receipt,
                    t.receipt IS NULL AND t.task IS NULL AND t.decision_digest IS NULL AND a.settled_ms IS NULL,
                    i.boot_id IS t.boot_id AND i.deadline_boot_ms IS t.deadline_boot_ms
                    AND (a.lease IS NULL OR a.time_ns=i.time_ns)
             FROM model_inference_leases i JOIN session_turns t USING(lease)
             LEFT JOIN model_lease_times a USING(lease) WHERE i.lease=?1 AND t.session=?2",
        )?
        .query_row(params![lease, session], |r| {
            Ok(Attempt {
                revision: r.get(0)?,
                model_id: r.get(1)?,
                turn: r.get(2)?,
                expires: r.get(3)?,
                status: r.get(4)?,
                source: r.get(5)?,
                failure: r.get(6)?,
                unsettled: r.get(7)?,
                clock_matches: r.get(8)?,
            })
        })
        .optional()?)
}

impl Attempt {
    fn issued(&self, conn: &Connection, session: &str, lease: &str, model: &Doc) -> Result<Doc> {
        let issued = conn
            .prepare_cached("SELECT * FROM versions WHERE id=?1 AND rev=?2")?
            .query_row(params![session, self.revision], row)
            .optional()?
            .ok_or_else(invalid)?;
        require(
            issued.kind == "Session"
                && model.kind == "Model"
                && model.id == self.model_id
                && inference::strict(model)
                && self.clock_matches
                && issued.spec["model_ref"] == model.reference()
                && state(&issued) == "ModelInFlight"
                && issued.body["current_lease"] == lease
                && self.turn > 0
                && issued.body["turn"] == self.turn
                && issued.body["lease_until"] == self.expires,
            "RecoveryRequired",
            "late decision lost its exact issuing Session",
        )?;
        Ok(issued)
    }

    fn unknown(&self, conn: &Connection, issued: &Doc, lease: &str) -> Result<Doc> {
        let failure = self.failure.as_ref().ok_or_else(invalid)?;
        let unknown = resolve(conn, &failure["session_ref"])?;
        let model = &issued.spec["model_ref"];
        let lifecycle = match self.source.as_deref() {
            Some("SessionCancelled") => "Cancelled",
            Some("Provider" | "LeaseExpired") => "Paused",
            _ => return Err(invalid()),
        };
        require(
            self.status == "Unknown"
                && self.unsettled
                && unknown.kind == "Session"
                && unknown.id == issued.id
                && unknown.owner == issued.owner
                && unknown.spec == issued.spec
                && unknown.rev > issued.rev
                && state(&unknown) == lifecycle
                && unknown.body["turn"] == self.turn
                && unknown.body["current_lease"].is_null()
                && unknown.body["outcome_knowledge"] == "Unknown"
                && unknown.body["model_inference_failure"]
                    == json!({"lease_id":lease,"model_ref":model,"outcome":"Unknown","source":self.source})
                && *failure
                    == json!({"session_ref":unknown.reference(),"model_ref":model,
                    "lease_id":lease,"outcome":"Unknown","lifecycle":lifecycle,"outcome_knowledge":"Unknown"}),
            "RecoveryRequired",
            "late decision lost its original Unknown settlement",
        )?;
        Ok(unknown)
    }
}

fn observation_digest(value: &Value) -> String {
    sha256(&canonical(&json!(["psios-model-late-decision-v1", value])))
}

fn checked_observation(value: &Value, digest: &str) -> Result<Observation> {
    let observation = decode(value).map_err(|_| invalid())?;
    require(
        observation_digest(value) == digest,
        "RecoveryRequired",
        "late model observation metadata digest mismatch",
    )?;
    Ok(observation)
}

pub(crate) fn receipt(conn: &Connection, lease: &str) -> Result<Option<Value>> {
    let record = conn
        .prepare_cached(
            "SELECT observation,observation_digest FROM model_late_decisions WHERE lease=?1",
        )?
        .query_row([lease], |r| {
            Ok((r.get::<_, Value>(0)?, r.get::<_, String>(1)?))
        })
        .optional()?;
    if let Some((value, digest)) = record {
        require(
            checked_observation(&value, &digest)?.lease_id == lease,
            "RecoveryRequired",
            "late model observation names another lease",
        )?;
        return Ok(Some(value));
    }
    Ok(None)
}

pub(crate) fn observe(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Respond = decode(params)?;
    label(&request.session_id)?;
    label(&request.lease_id)?;
    let lease = &request.lease_id;
    let session = db
        .find(&request.session_id)?
        .filter(|doc| doc.kind == "Session")
        .ok_or_else(unavailable)?;
    let model = db.resolve(&session.spec["model_ref"])?;
    require(
        model.kind == "Model" && model.spec["provider_principal_id"] == actor,
        "PermissionDenied",
        "late decision outside original provider scope",
    )?;
    let Some(attempt) = attempt(db.c, &session.id, lease)? else {
        policy_scope_or_fenced(db, &session, &model, lease, actor)?;
        return Err(fenced());
    };
    let issued = attempt.issued(db.c, &session.id, lease, &model)?;
    let Some((binding, chain)) = provider_authority::issued_binding(db.c, &issued, &model, lease)?
    else {
        super::provider_model(db, &session, actor)?;
        return Err(fenced());
    };
    if !provider_authority::binding_current(db, &model, &binding, &chain)? {
        let mut error = unavailable();
        error.0.details = Some(json!({"reason":"OriginalProviderAuthorityUnavailable"}));
        return Err(error);
    }
    match attempt.status.as_str() {
        "Unknown" => {}
        "Pending" => {
            let mut error = err(
                "PreconditionFailed",
                "original lease has not settled Unknown",
            );
            error.0.retryable = true;
            error.0.retry_condition = "SameRequestAfterBackoff".into();
            return Err(error);
        }
        "Responded" | "NotStarted" => return Err(fenced()),
        _ => return Err(invalid()),
    }
    let unknown = attempt.unknown(db.c, &issued, lease)?;
    let maximum: u64 = decode(&model.spec["max_output_bytes"])?;
    let bytes = canonical(&request.decision);
    let output_bytes = bytes.len() as u64;
    require(
        output_bytes <= maximum,
        "QuotaExceeded",
        "provider output exceeds registered bound",
    )?;
    let _: Decision = decode(&request.decision)?;
    let decision_digest = sha256(&bytes);
    let usage_digest = request.usage.map(|usage| sha256(&canonical(&json!(usage))));
    if let Some(previous) = receipt(db.c, lease)? {
        require(
            previous["decision_digest"] == decision_digest
                && previous["usage_digest"] == json!(usage_digest)
                && previous["output_bytes"] == output_bytes,
            "IdempotencyConflict",
            "lease already has a different late decision observation",
        )?;
        return Ok(json!({"status":"Recorded","observation":previous}));
    }
    let observation = serde_json::to_value(Observation {
        lease_id: lease.clone(),
        model_ref: decode(&model.reference())?,
        issued_session_ref: decode(&issued.reference())?,
        unknown_session_ref: decode(&unknown.reference())?,
        provider_principal_id: actor.into(),
        provider_authority_digest: binding.digest(),
        decision_digest,
        output_bytes,
        observed_at: now(),
        usage_digest,
    })?;
    db.c.execute(
        "INSERT INTO model_late_decisions(lease,observation,observation_digest) VALUES(?1,?2,?3)",
        params![lease, observation, observation_digest(&observation)],
    )?;
    db.audit(
        actor,
        "Model.ObserveLateDecision",
        Some(&session.id),
        "Recorded",
    )?;
    Ok(json!({"status":"Recorded","observation":observation}))
}

fn policy_scope_or_fenced(
    db: &Db<'_>,
    session: &Doc,
    model: &Doc,
    lease: &str,
    actor: &str,
) -> Result<()> {
    let legacy: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_turns WHERE lease=?1 AND session=?2)",
        params![lease, session.id],
        |r| r.get(0),
    )?;
    if !legacy {
        return Err(unavailable());
    }
    require(
        !inference::strict(model),
        "RecoveryRequired",
        "strict model lease lost its checkpoint",
    )?;
    super::provider_model(db, session, actor)?;
    Ok(())
}

fn sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_records(conn).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            error
        } else {
            err(
                "RecoveryRequired",
                format!("late model observation verification: {}", error.0.message),
            )
        }
    })
}

fn verify_records(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT lease,observation,observation_digest FROM model_late_decisions")?;
    let mut rows = statement.query([])?;
    while let Some(record) = rows.next()? {
        let lease: String = record.get(0)?;
        let observation = checked_observation(&record.get(1)?, &record.get::<_, String>(2)?)?;
        let model = resolve(conn, &json!(observation.model_ref))?;
        let attempt = attempt(conn, &observation.issued_session_ref.resource_id, &lease)?
            .ok_or_else(invalid)?;
        let issued = attempt.issued(
            conn,
            &observation.issued_session_ref.resource_id,
            &lease,
            &model,
        )?;
        let unknown = attempt.unknown(conn, &issued, &lease)?;
        let (binding, _) = provider_authority::issued_binding(conn, &issued, &model, &lease)?
            .ok_or_else(invalid)?;
        let maximum: u64 = decode(&model.spec["max_output_bytes"])?;
        require(
            observation.lease_id == lease
                && json!(observation.issued_session_ref) == issued.reference()
                && json!(observation.unknown_session_ref) == unknown.reference()
                && observation.provider_principal_id == model.spec["provider_principal_id"]
                && observation.provider_authority_digest == binding.digest()
                && sha256_digest(&observation.decision_digest)
                && observation
                    .usage_digest
                    .as_deref()
                    .is_none_or(sha256_digest)
                && (1..=maximum).contains(&observation.output_bytes)
                && chrono::DateTime::parse_from_rfc3339(&observation.observed_at).is_ok(),
            "RecoveryRequired",
            "late model observation differs from its original lease",
        )?;
    }
    Ok(())
}
