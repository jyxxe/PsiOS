use super::output;
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
        output::validate_pointer(&self.pointer)?;
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
        let (artifact, body) = output::artifact(db, run, source, &self.step)?;
        let value = body
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
            "source_task_ref":source["task_ref"], "source_artifact_ref":artifact.reference(),
            "value_digest":sha256(&canonical(value))}),
        )
    }
}
