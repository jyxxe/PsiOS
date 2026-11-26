//! A completed strict inference whose declared tool did not enter the broker.
use super::{inference, late_decision::sha256_digest, provider_authority, state};
use crate::{
    Result,
    db::{find, resolve, row},
    err, require,
};
use psios_api::{ModelDecisionRejectionPolicy as Policy, ModelTokenUsage, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const CODE: &str = "ToolNotDeclared";
const MARKER: &str = "model_decision_rejection";

pub(super) fn record(
    body: &mut Value,
    policy: Policy,
    lease: &str,
    tool: &str,
    decision_digest: &str,
    output_bytes: usize,
) -> Value {
    body["lifecycle"] = json!("Failed");
    body["outcome_knowledge"] = json!("Known");
    body[MARKER] = json!({"lease_id":lease,"decision_digest":decision_digest,"output_bytes":output_bytes,
        "tool_digest":sha256(tool.as_bytes()),"policy":policy,"code":CODE});
    json!({"policy":policy,"code":CODE})
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "rejected decision does not bind its original strict lease and Skill",
    )
}

struct Attempt {
    turn: u32,
    digest: Option<String>,
    issued_revision: i64,
    status: String,
    elapsed: Option<i64>,
    no_task: bool,
}

pub(super) fn verify_receipt(
    conn: &Connection,
    session: &str,
    lease: &str,
    receipt: &Value,
) -> Result<()> {
    if receipt.get("rejection").is_none() && receipt.get("outcome_knowledge").is_none() {
        return Ok(());
    }
    let prior = conn.query_row(
        "SELECT t.turn,t.decision_digest,i.session_revision,i.status,a.settled_ms,
            t.task IS NULL AND NOT EXISTS(SELECT 1 FROM session_tasks j WHERE j.session=t.session AND j.turn=t.turn)
         FROM session_turns t JOIN model_inference_leases i USING(lease)
         LEFT JOIN model_lease_times a USING(lease) WHERE t.session=?1 AND t.lease=?2",
        params![session,lease],
        |r| Ok(Attempt { turn:r.get(0)?, digest:r.get(1)?, issued_revision:r.get(2)?,
            status:r.get(3)?, elapsed:r.get(4)?, no_task:r.get(5)? }),
    ).optional()?;
    let Attempt {
        turn,
        digest,
        issued_revision,
        status,
        elapsed,
        no_task,
    } = prior.ok_or_else(invalid)?;
    let issued = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![session, issued_revision],
        row,
    )?;
    let model = resolve(conn, &issued.spec["model_ref"])?;
    let skill = resolve(conn, &issued.spec["skill_ref"])?;
    let settled = resolve(conn, &receipt["session_ref"])?;
    let before = inference::predecessor(conn, &settled)?;
    let reserved = resolve(conn, &issued.body["current_usage_ref"])?;
    let current_usage = find(conn, &reserved.id)?.ok_or_else(invalid)?;
    let marker = &settled.body[MARKER];
    let tool_digest = marker["tool_digest"].as_str().ok_or_else(invalid)?;
    let digest = digest
        .as_deref()
        .filter(|value| sha256_digest(value))
        .ok_or_else(invalid)?;
    let output_bytes = receipt["output_bytes"]
        .as_u64()
        .filter(|bytes| {
            *bytes > 0
                && model.spec["max_output_bytes"]
                    .as_u64()
                    .is_some_and(|maximum| *bytes <= maximum)
        })
        .ok_or_else(invalid)?;
    let policy = Policy::UndeclaredToolV1;
    let mut expected_receipt = json!({"session_ref":settled.reference(),"task_id":null,
        "decision_digest":digest,"output_bytes":output_bytes,"lifecycle":"Failed",
        "outcome_knowledge":"Known","rejection":{"policy":policy,"code":CODE}});
    if let Some(reference) = receipt.get("usage_ref") {
        expected_receipt["usage_ref"] = reference.clone();
    }
    let declared = skill.spec["tool_refs"].as_object().ok_or_else(invalid)?;
    require(
        no_task
            && status == "Responded"
            && marker
                == &json!({"lease_id":lease,"decision_digest":digest,"tool_digest":tool_digest,
                "output_bytes":output_bytes,"policy":policy,"code":CODE})
            && sha256_digest(tool_digest)
            && declared
                .keys()
                .all(|tool| sha256(tool.as_bytes()) != tool_digest)
            && receipt == &expected_receipt
            && issued.kind == "Session"
            && state(&issued) == "ModelInFlight"
            && issued.body["current_lease"] == lease
            && issued.body["turn"] == turn
            && model.kind == "Model"
            && inference::strict(&model)
            && provider_authority::issued_binding(conn, &issued, &model, lease)?.is_some()
            && skill.kind == "Skill"
            && settled.kind == "Session"
            && settled.id == issued.id
            && settled.owner == issued.owner
            && settled.spec == issued.spec
            && before.rev >= issued.rev
            && state(&before) == "ModelInFlight"
            && before.body["current_lease"] == lease
            && before.body["current_usage_ref"] == issued.body["current_usage_ref"]
            && settled.body["turn"] == turn
            && settled.body["current_lease"].is_null()
            && state(&settled) == "Failed"
            && settled.body["outcome_knowledge"] == "Known"
            && settled.body["last_task"] == before.body["last_task"]
            && reserved.kind == "ModelUsage"
            && reserved.body["call_id"] == lease
            && reserved.body["session_id"] == session,
        "RecoveryRequired",
        "rejected decision does not bind its original strict lease and Skill",
    )?;
    if let Some(reference) = receipt.get("usage_ref") {
        let usage = resolve(conn, reference)?;
        let tokens = usage.body.get("reported_tokens");
        require(
            usage.kind == "ModelUsage"
                && usage.owner == issued.owner
                && usage.id == reserved.id
                && current_usage.reference() == *reference
                && reserved.rev.checked_add(1) == Some(usage.rev)
                && usage.body["model_ref"] == model.reference()
                && usage.body["session_id"] == session
                && usage.body["call_id"] == lease
                && usage.body["authority_elapsed_lease_ms"] == json!(elapsed)
                && (tokens.is_some() || elapsed.is_some())
                && tokens.is_none_or(|tokens| {
                    serde_json::from_value::<ModelTokenUsage>(tokens.clone()).is_ok()
                        && usage.body["provider_principal_id"]
                            == model.spec["provider_principal_id"]
                        && usage.body["measurement_source"] == "ProviderReported"
                }),
            "RecoveryRequired",
            "rejected decision usage differs from the original model reservation",
        )?;
    } else if elapsed.is_some() || current_usage.reference() != reserved.reference() {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn verify_links(conn: &Connection) -> Result<()> {
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current s WHERE s.kind='Session' AND s.body->'model_decision_rejection' IS NOT NULL
        AND NOT EXISTS(SELECT 1 FROM session_turns t WHERE t.session=s.id AND t.lease=s.body->'model_decision_rejection'->>'lease_id'
        AND t.receipt->'rejection' IS NOT NULL))", [], |r| r.get(0))?;
    require(
        !missing,
        "RecoveryRequired",
        "rejected model Session lost its decision receipt",
    )
}
