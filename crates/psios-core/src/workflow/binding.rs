use super::{PreparedStep, Step, output};
use crate::{
    Result, broker,
    db::{Db, Doc},
    err, require,
};
use psios_api::{canonical, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Selector {
    step: String,
    pointer: String,
}

pub(super) type Bindings = BTreeMap<String, Selector>;

pub(super) fn validate(step: &Step) -> Result<()> {
    for (field, source) in &step.inputs_from {
        require(
            matches!(
                (step.tool.as_str(), field.as_str()),
                ("volume.write", "content") | ("secret.mac", "message")
            ) && step.input[field].is_string(),
            "InvalidArgument",
            "workflow input mapping requires a supported text argument",
        )?;
        require(
            step.after.contains(&source.step),
            "InvalidArgument",
            "workflow input mapping must read a required predecessor",
        )?;
        output::validate_pointer(&source.pointer)?;
    }
    Ok(())
}

fn mapping_digest(step: &PreparedStep) -> String {
    sha256(&canonical(&json!(step.inputs_from)))
}

pub(super) fn pin(
    db: &Db<'_>,
    run: &Doc,
    step: &PreparedStep,
    statuses: &[Value],
) -> Result<Value> {
    let base = db.resolve(&step.request["input_ref"])?;
    require(
        base.kind == "Input" && base.owner == run.owner,
        "RecoveryRequired",
        "workflow input template is outside the authority",
    )?;
    let mut input = base.body;
    let mut sources = Vec::with_capacity(step.inputs_from.len());
    for (field, source) in &step.inputs_from {
        let predecessor = statuses
            .iter()
            .find(|status| status["step_id"] == source.step)
            .ok_or_else(|| err("RecoveryRequired", "workflow input predecessor is missing"))?;
        let artifact = output::artifact(db, run, predecessor, &source.step)?;
        let value = artifact
            .body
            .pointer(&source.pointer)
            .filter(|value| value.is_string())
            .ok_or_else(|| {
                err(
                    "PreconditionFailed",
                    "workflow input mapping needs a text value",
                )
            })?;
        let argument = input
            .get_mut(field)
            .filter(|value| value.is_string())
            .ok_or_else(|| err("RecoveryRequired", "workflow mapped argument is missing"))?;
        *argument = value.clone();
        sources.push(
            json!({"field":field, "source_task_ref":predecessor["task_ref"],
            "source_artifact_ref":artifact.reference(), "value_digest":sha256(&canonical(value))}),
        );
    }
    let materialized = broker::bind_workflow_input(db, &run.owner, &step.request, &input)?;
    Ok(
        json!({"mapping_digest":mapping_digest(step), "sources":sources,
              "input_ref":materialized["input_ref"], "request_digest":materialized["request_digest"]}),
    )
}

pub(super) fn request(step: &PreparedStep, binding: &Value) -> Result<Value> {
    require(
        binding["mapping_digest"] == mapping_digest(step)
            && binding["input_ref"].is_object()
            && binding["request_digest"].is_string(),
        "RecoveryRequired",
        "pinned workflow input does not match its mapping",
    )?;
    let mut request = step.request.clone();
    request["input_ref"] = binding["input_ref"].clone();
    request["request_digest"] = binding["request_digest"].clone();
    Ok(request)
}
