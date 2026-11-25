//! Move execution to another pre-authorized local model without reassigning an old lease.
use super::*;
use psios_api::ResourceRef;
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
        return Ok(history.body["receipt"].clone());
    }
    previous.cas(&p.expected_resource_version)?;
    let recoverable_pause = state(&previous) == "Paused"
        && matches!(
            previous.body["pause_reason"].as_str(),
            Some("ModelAuthorizationDenied" | "ModelContextLimit" | "ModelDependencyChanged")
        );
    require(
        previous.body["outcome_knowledge"] != "Unknown"
            && (matches!(state(&previous), "AwaitingModel" | "ModelInFlight") || recoverable_pause),
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
    db.cfg.check_admission_space()?;
    if previous.spec["provider_queue"] == true {
        queue::check_capacity(db, &model.id)?;
    }

    let successor_id = registry::stable(
        "session",
        json!([
            previous.owner,
            "model-fallback",
            previous.id,
            model.reference()
        ]),
    );
    let remaining: Vec<_> = previous.spec["fallback_model_refs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|reference| **reference != model_ref)
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
    body["model_ref"] = model_ref;
    body["fallback_model_refs"] = spec["fallback_model_refs"].clone();
    body["predecessor_ref"] = previous.reference();
    body["lifecycle"] = json!("AwaitingModel");
    body["current_lease"] = Value::Null;
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
    db.create(
        "SessionModelHandoff",
        &history_id,
        &previous.owner,
        json!({"request":request,"authorization_ref":delegation.reference()}),
        json!({"original_session_ref":previous.reference(),"ended_lease_id":previous.body["current_lease"],"receipt":receipt}),
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
