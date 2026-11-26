//! Move execution to another pre-authorized local model without reassigning an old lease.
use super::*;
use crate::db::{resolve, row};
use psios_api::ResourceRef;
use rusqlite::Connection;
use std::collections::BTreeSet;

pub(crate) const ACTION: &str = "session.failover";
pub(crate) const FORMAT: u32 = 47;
pub(crate) const SCHEMA: &str = include_str!("fallback.sql");

pub(super) fn validate(
    primary: &Value,
    alternatives: &[Value],
    context_policy: &str,
) -> Result<()> {
    if alternatives.is_empty() {
        return Ok(());
    }
    require(
        alternatives.len() <= 3 && context_policy == "ReferencesOnly",
        "InvalidArgument",
        "local model fallback requires ReferencesOnly and at most three alternatives",
    )?;
    let primary: ResourceRef = decode(primary)?;
    let mut distinct = BTreeSet::from([primary.resource_id]);
    for reference in alternatives {
        let reference: ResourceRef = decode(reference)?;
        require(
            distinct.insert(reference.resource_id),
            "InvalidArgument",
            "fallback models must have distinct identities",
        )?;
    }
    Ok(())
}

fn authority(db: &Db<'_>, session: &Doc) -> Result<Doc> {
    policy::active(db, &session.owner)?;
    let delegation = field(&session.spec, "delegation_id")?;
    policy::delegation(db, &session.owner, delegation, "session.run")?;
    policy::delegation(db, &session.owner, delegation, ACTION)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "session model handoff state incomplete"))
}

fn compatible(primary: &Doc, alternative: &Doc) -> Result<()> {
    require(
        primary.spec["transport"] == "authenticated-local-peer"
            && alternative.spec["transport"] == primary.spec["transport"]
            && alternative.spec["interface"] == primary.spec["interface"]
            && alternative.spec["modalities"] == primary.spec["modalities"],
        "PreconditionFailed",
        "fallback model is outside the authenticated local interface",
    )
}

fn successor_id(previous: &Doc, model_ref: &Value) -> String {
    registry::stable(
        "session",
        json!([previous.owner, "model-fallback", previous.id, model_ref]),
    )
}

fn successor_checkpoint(previous: &Doc, model_ref: &Value) -> Result<(Value, Value)> {
    let alternatives = previous.spec["fallback_model_refs"]
        .as_array()
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "session model handoff has no original alternates",
            )
        })?;
    let remaining: Vec<_> = alternatives
        .iter()
        .filter(|reference| *reference != model_ref)
        .collect();
    let mut spec = previous.spec.clone();
    spec["model_ref"] = model_ref.clone();
    spec["fallback_model_refs"] = json!(remaining);
    spec["predecessor_ref"] = previous.reference();

    let mut body = previous.body.clone();
    let fields = body.as_object_mut().unwrap();
    for name in ["current_usage_ref", "lease_until", "pause_reason"] {
        fields.remove(name);
    }
    if inference::not_started(previous) {
        fields.remove("model_inference_failure");
        fields.remove("outcome_knowledge");
    }
    body["model_ref"] = model_ref.clone();
    body["fallback_model_refs"] = spec["fallback_model_refs"].clone();
    body["predecessor_ref"] = previous.reference();
    body["lifecycle"] = json!("AwaitingModel");
    body["current_lease"] = Value::Null;
    Ok((spec, body))
}

pub(super) fn check_open(
    db: &Db<'_>,
    session: &Doc,
    primary: &Doc,
    candidates: &[Value],
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let delegation = authority(db, session)?;
    policy::tool_scope(db, &delegation.reference(), &primary.id)?;
    for reference in candidates {
        let candidate = registry::pinned(db, reference, "Model")?;
        policy::tool_scope(db, &delegation.reference(), &candidate.id)?;
        compatible(primary, &candidate)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    id: String,
    expected_resource_version: String,
    model_ref: ResourceRef,
}

pub fn handoff(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Handoff = decode(request)?;
    let previous = db.of(&p.id, "Session")?;
    policy::owned(actor, &previous)?;
    let delegation = authority(db, &previous)?;
    let model_ref = json!(p.model_ref);
    policy::tool_scope(db, &delegation.reference(), &p.model_ref.resource_id)?;

    let history_id = format!("model-handoff-{}", previous.id);
    if let Some(history) = db.find(&history_id)? {
        require(
            history.kind == "SessionModelHandoff" && history.spec["request"] == *request,
            "Conflict",
            "session has already transferred to an authorized model",
        )?;
        if history.body.get("not_started_session_ref").is_some()
            || previous.body["pause_reason"] == "ModelNotStarted"
        {
            verify_not_started_handoff(db.c, &history)?;
        }
        return Ok(history.body["receipt"].clone());
    }
    previous.cas(&p.expected_resource_version)?;
    require(
        !inference::in_flight(db, &previous)?,
        "PreconditionFailed",
        "strict model inference has not been settled",
    )?;
    let not_started = inference::not_started(&previous);
    let recoverable_pause = state(&previous) == "Paused"
        && matches!(
            previous.body["pause_reason"].as_str(),
            Some("ModelAuthorizationDenied" | "ModelContextLimit" | "ModelDependencyChanged")
        );
    require(
        previous.body["outcome_knowledge"] != "Unknown"
            && (matches!(state(&previous), "AwaitingModel" | "ModelInFlight")
                || recoverable_pause
                || not_started),
        "PreconditionFailed",
        "session has an unsettled tool or has already closed",
    )?;
    require(
        has_turn(&previous),
        "QuotaExceeded",
        "session turn limit reached",
    )?;
    require(
        previous.spec["context_policy"] == "ReferencesOnly"
            && previous.spec["fallback_model_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&model_ref)),
        "PermissionDenied",
        "model revision was not selected as an alternate for this session",
    )?;
    let model = registry::pinned(db, &model_ref, "Model")?;
    let primary = db.resolve(&previous.spec["model_ref"])?;
    compatible(&primary, &model)?;
    let no_start_ref = if not_started {
        require(
            delegation.body["budget_ref"] == previous.body["budget_ref"],
            "RecoveryRequired",
            "model handoff would replace the original session budget",
        )?;
        let reference = inference::handoff_receipt(db.c, &previous, &primary)?;
        inference::check_model_budget(db, &previous, &model)?;
        Some(reference)
    } else {
        None
    };
    db.cfg.check_admission_space()?;
    if previous.spec["provider_queue"] == true {
        queue::check_capacity(db, &model.id)?;
    }

    let successor_id = successor_id(&previous, &model_ref);
    let (spec, body) = successor_checkpoint(&previous, &model_ref)?;
    let successor = db.create("Session", &successor_id, &previous.owner, spec, body)?;
    let (_, skill, model) = deps(db, &successor)?;
    context(db, &successor, &skill, &model)?;

    if let Some(id) = successor.body["last_task"].as_str() {
        let task = db.of(id, "Task")?;
        require(
            task.owner == previous.owner
                && task.body["lifecycle"] == "Succeeded"
                && task.body["outcome_knowledge"] == "Known",
            "PreconditionFailed",
            "handoff checkpoint has no confirmed tool result",
        )?;
        db.c.execute(
            "INSERT INTO inherited_session_results(session,task) VALUES(?1,?2)",
            params![successor.id, task.id],
        )?;
    }

    let mut closed = previous.body.clone();
    closed["lifecycle"] = json!("Cancelled");
    closed["current_lease"] = Value::Null;
    closed["successor_ref"] = successor.reference();
    closed["handoff_ref"] = json!(history_id);
    let closed = db.status(&previous, closed)?;
    let receipt =
        json!({"previous_session_ref":closed.reference(),"successor":response(&successor)});
    let mut history_body = json!({"original_session_ref":previous.reference(),
        "ended_lease_id":previous.body["current_lease"],"receipt":receipt});
    if let Some(reference) = no_start_ref {
        history_body["not_started_session_ref"] = reference;
    }
    db.create(
        "SessionModelHandoff",
        &history_id,
        &previous.owner,
        json!({"request":request,"authorization_ref":delegation.reference()}),
        history_body,
    )?;
    db.audit(
        actor,
        "Session.UseFallback",
        Some(&previous.id),
        "HandedOff",
    )?;
    Ok(receipt)
}

pub(super) fn retains_result(db: &Db<'_>, task: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM inherited_session_results WHERE task=?1)",
        [task],
        |row| row.get(0),
    )?)
}

pub(crate) fn verify_not_started(conn: &Connection) -> Result<()> {
    let mut witnesses = conn.prepare(
        "SELECT h.* FROM current h LEFT JOIN versions s
           ON s.id=h.body->'original_session_ref'->>'resource_id'
           AND s.rev=CAST(substr(h.body->'original_session_ref'->>'version',2) AS INTEGER)
         WHERE h.kind='SessionModelHandoff' AND
           (h.body->'not_started_session_ref' IS NOT NULL OR s.body->>'pause_reason'='ModelNotStarted')",
    )?;
    for witness in witnesses.query_map([], row)? {
        verify_not_started_handoff(conn, &witness?).map_err(|_| {
            err(
                "RecoveryRequired",
                "model handoff lost its exact NotStarted lineage",
            )
        })?;
    }
    Ok(())
}

fn verify_not_started_handoff(conn: &Connection, witness: &Doc) -> Result<()> {
    let prior = resolve(conn, &witness.body["original_session_ref"])?;
    let receipt = &witness.body["receipt"];
    let closed = resolve(conn, &receipt["previous_session_ref"])?;
    let successor = resolve(conn, &receipt["successor"]["reference"])?;
    let original_model = resolve(conn, &prior.spec["model_ref"])?;
    let model_ref = &successor.spec["model_ref"];
    let alternate = resolve(conn, model_ref)?;
    let delegation = resolve(conn, &witness.spec["authorization_ref"])?;
    compatible(&original_model, &alternate)?;
    let expected_source = inference::handoff_receipt(conn, &prior, &original_model)?;
    let (successor_spec, successor_body) = successor_checkpoint(&prior, model_ref)?;
    let mut closed_body = prior.body.clone();
    closed_body["lifecycle"] = json!("Cancelled");
    closed_body["current_lease"] = Value::Null;
    closed_body["successor_ref"] = successor.reference();
    closed_body["handoff_ref"] = json!(witness.id);
    require(
        witness.id == format!("model-handoff-{}", prior.id)
            && witness.rev == 1
            && witness.owner == prior.owner
            && witness.body["not_started_session_ref"] == expected_source
            && witness.body["ended_lease_id"].is_null()
            && witness.spec["request"]
                == json!({"id":prior.id,"expected_resource_version":prior.version(),
                "model_ref":model_ref})
            && alternate.kind == "Model"
            && alternate.body["lifecycle"] == "Active"
            && prior.spec["context_policy"] == "ReferencesOnly"
            && prior.spec["fallback_model_refs"]
                .as_array()
                .is_some_and(|models| models.contains(model_ref))
            && delegation.kind == "Delegation"
            && delegation.id == prior.spec["delegation_id"]
            && delegation.owner == prior.owner
            && delegation.body["budget_ref"] == prior.body["budget_ref"]
            && closed.id == prior.id
            && closed.kind == "Session"
            && closed.owner == prior.owner
            && closed.rev == prior.rev + 1
            && closed.spec == prior.spec
            && closed.body == closed_body
            && successor.kind == "Session"
            && successor.id == successor_id(&prior, model_ref)
            && successor.owner == prior.owner
            && successor.rev == 1
            && successor.spec == successor_spec
            && successor.body == successor_body
            && receipt["successor"] == response(&successor),
        "RecoveryRequired",
        "model handoff differs from the original NotStarted session",
    )
}
