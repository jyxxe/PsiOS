use super::{Manifest, is_task_output, retention};
use crate::{
    Result,
    db::{Db, Doc},
    decode, err, now, policy, require,
};
use psios_api::canonical;
use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const UNAVAILABLE: &str = "ResultUnavailable";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verification {
    id: String,
    expected_resource_version: String,
    max_bytes: Option<usize>,
}

fn verify_bytes(db: &Db<'_>, artifact: &Doc, remaining: &mut usize) -> Result<()> {
    let manifest = Manifest::read(artifact)?;
    let bytes = manifest
        .as_ref()
        .map_or_else(|| canonical(&artifact.body).len(), |manifest| manifest.size);
    *remaining = remaining.checked_sub(bytes).ok_or_else(|| {
        err(
            "QuotaExceeded",
            "task results exceed verification byte limit",
        )
    })?;
    if let Some(manifest) = manifest {
        manifest.verify(db.c)?;
    }
    Ok(())
}

/// Check the pinned output and settle only its availability in the authority transaction.
pub fn verify(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let request: Verification = decode(params)?;
    let mut remaining = request.max_bytes.unwrap_or(DEFAULT_BYTES);
    require(
        (1..=MAX_BYTES).contains(&remaining),
        "InvalidArgument",
        "result verification limit must be between 1 and 67108864 bytes",
    )?;
    let task = db.of(&request.id, "Task")?;
    task.cas(&request.expected_resource_version)?;
    let refs = task.body["result"]["artifact_refs"]
        .as_array()
        .filter(|refs| !refs.is_empty())
        .ok_or_else(|| err("PreconditionFailed", "task has no pinned result artifacts"))?;
    require(
        matches!(
            task.body["lifecycle"].as_str(),
            Some("Succeeded" | "Cancelled")
        ) && task.body["outcome_knowledge"] == "Known",
        "PreconditionFailed",
        "result verification requires a settled Task outcome",
    )?;
    if task.body["result"]["availability"] == "Evicted" {
        require(
            retention::task_receipt(db.c, &task)?.is_some(),
            "RecoveryRequired",
            "evicted result has no authorized retention receipt",
        )?;
        return Ok(task.body);
    }

    let mut availability = "Available";
    for reference in refs {
        let artifact = match db.resolve(reference) {
            Ok(artifact) => artifact,
            Err(error) if error.0.code == "PreconditionFailed" => {
                availability = "Unavailable";
                break;
            }
            Err(error) => return Err(error),
        };
        require(
            is_task_output(&artifact, &task),
            "RecoveryRequired",
            "artifact does not belong to the producing task and run",
        )?;
        retention::require_content(db.c, &artifact)?;
        match verify_bytes(db, &artifact, &mut remaining) {
            Ok(()) => {}
            Err(error) if error.0.code == "RecoveryRequired" => {
                availability = "Unavailable";
                break;
            }
            Err(error) => return Err(error),
        }
    }

    if task.body["result"]["availability"] == availability {
        return Ok(task.body);
    }
    let mut body = task.body.clone();
    body["result"]["availability"] = json!(availability);
    let conditions = body["conditions"]
        .as_array_mut()
        .ok_or_else(|| err("RecoveryRequired", "invalid stored task conditions"))?;
    conditions.retain(|condition| condition["type"] != UNAVAILABLE);
    if availability == "Unavailable" {
        conditions.push(json!({
            "type": UNAVAILABLE, "status": "True", "reason": UNAVAILABLE,
            "message": "a pinned result artifact is missing or failed integrity verification",
            "observed_generation": task.generation.to_string(), "last_transition_at": now()
        }));
    }
    let updated = db.status(&task, body)?;
    db.audit(actor, "Task.VerifyResults", Some(&task.id), availability)?;
    Ok(updated.body)
}
