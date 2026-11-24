use crate::{
    Result,
    db::{Db, Doc},
    err, require,
};
use psios_api::{canonical, sha256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum Scalar {
    Boolean(bool),
    Choice(String),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Predicate {
    pub(super) step: String,
    pointer: String,
    equals: Scalar,
}

impl Predicate {
    pub(super) fn validate(&self, parents: &[String]) -> Result<()> {
        require(
            parents.contains(&self.step),
            "InvalidArgument",
            "workflow condition must read a direct predecessor",
        )?;
        require(
            (self.pointer.is_empty() || self.pointer.starts_with('/'))
                && self.pointer.len() <= 192
                && !self.pointer.chars().any(char::is_control)
                && self
                    .pointer
                    .split('~')
                    .skip(1)
                    .all(|part| part.starts_with(['0', '1'])),
            "InvalidArgument",
            "workflow condition requires a bounded JSON Pointer",
        )?;
        require(
            match &self.equals {
                Scalar::Boolean(_) => true,
                Scalar::Choice(text) => text.len() <= 256 && !text.chars().any(char::is_control),
            },
            "InvalidArgument",
            "workflow condition choice exceeds its scalar bound",
        )
    }

    pub(super) fn digest(&self) -> String {
        sha256(&canonical(&json!(self)))
    }

    pub(super) fn evaluate(&self, db: &Db<'_>, run: &Doc, source: &Value) -> Result<Value> {
        let task = db.resolve(&source["task_ref"])?;
        require(
            source["step_id"] == self.step
                && task.kind == "Task"
                && task.owner == run.owner
                && task.body["lifecycle"] == "Succeeded"
                && task.body["outcome_knowledge"] == "Known"
                && task.body["origin_ref"]["origin_type"] == "WorkflowRun"
                && task.body["origin_ref"]["origin_id"] == run.id
                && task.body["origin_ref"]["step_key"] == source["step_key"],
            "RecoveryRequired",
            "workflow condition source is not its settled predecessor",
        )?;
        let artifact_ref = task.body["result"]["artifact_refs"]
            .as_array()
            .filter(|refs| refs.len() == 1)
            .and_then(|refs| refs.first())
            .ok_or_else(|| {
                err(
                    "PreconditionFailed",
                    "workflow condition needs one result artifact",
                )
            })?;
        let artifact = db.resolve(artifact_ref)?;
        require(
            artifact.kind == "Artifact" && artifact.owner == run.owner,
            "RecoveryRequired",
            "workflow condition artifact is not owned by the workflow",
        )?;
        let value = artifact
            .body
            .pointer(&self.pointer)
            .ok_or_else(|| err("PreconditionFailed", "workflow condition value is absent"))?;
        let selected = match &self.equals {
            Scalar::Boolean(expected) => value.as_bool().map(|actual| actual == *expected),
            Scalar::Choice(expected) => value.as_str().map(|actual| actual == expected),
        }
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "workflow condition value has the wrong type",
            )
        })?;
        Ok(
            json!({"selected":selected, "predicate_digest": self.digest(),
            "source_task_ref":source["task_ref"], "source_artifact_ref":artifact_ref,
            "value_digest":sha256(&canonical(value))}),
        )
    }
}
