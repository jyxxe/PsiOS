use crate::{
    Result, artifact,
    db::{Db, Doc},
    err, require,
};
use serde_json::Value;

pub(super) fn validate_pointer(pointer: &str) -> Result<()> {
    require(
        (pointer.is_empty() || pointer.starts_with('/'))
            && pointer.len() <= 192
            && !pointer.chars().any(char::is_control)
            && pointer
                .split('~')
                .skip(1)
                .all(|part| part.starts_with(['0', '1'])),
        "InvalidArgument",
        "workflow requires a bounded JSON Pointer",
    )
}

pub(super) fn artifact(db: &Db<'_>, run: &Doc, source: &Value, step: &str) -> Result<(Doc, Value)> {
    let task = db.resolve(&source["task_ref"])?;
    require(
        source["step_id"] == step
            && task.kind == "Task"
            && task.owner == run.owner
            && task.body["lifecycle"] == "Succeeded"
            && task.body["outcome_knowledge"] == "Known"
            && task.body["origin_ref"]["origin_type"] == "WorkflowRun"
            && task.body["origin_ref"]["origin_id"] == run.id
            && task.body["origin_ref"]["step_key"] == source["step_key"],
        "RecoveryRequired",
        "workflow input source is not its settled predecessor",
    )?;
    require(
        task.body["result"]["availability"] != "Evicted",
        "ResultEvicted",
        "workflow predecessor result was evicted",
    )?;
    require(
        task.body["result"]["availability"] != "Unavailable",
        "ResultUnavailable",
        "workflow predecessor result is unavailable",
    )?;
    let reference = task.body["result"]["artifact_refs"]
        .as_array()
        .filter(|refs| refs.len() == 1)
        .and_then(|refs| refs.first())
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "workflow input needs one result artifact",
            )
        })?;
    let artifact = db.resolve(reference).map_err(|error| {
        if error.0.code == "PreconditionFailed" {
            err(
                "ResultUnavailable",
                "workflow predecessor artifact version is unavailable",
            )
        } else {
            error
        }
    })?;
    require(
        artifact.kind == "Artifact"
            && artifact.owner == run.owner
            && artifact.spec["task_id"] == task.id,
        "RecoveryRequired",
        "workflow artifact does not belong to its source Task",
    )?;
    let body = artifact::read(db, &artifact, None).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            err(
                "ResultUnavailable",
                "workflow predecessor artifact failed content verification",
            )
        } else {
            error
        }
    })?;
    Ok((artifact, body))
}
