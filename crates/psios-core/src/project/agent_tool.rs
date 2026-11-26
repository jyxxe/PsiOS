//! Submit a fixed local result through the Task broker's authority transaction.
use super::{attach_evidence, evidence_target};
use crate::{
    Result,
    db::{Db, Doc},
    label, require,
};
use psios_api::{ResourceRef, SubmitWorkItemEvidence};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "project.submit_evidence";
pub(crate) const FORMAT: u32 = super::MAINTENANCE_FORMAT + 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct Input(SubmitWorkItemEvidence);

fn revision(value: &str) -> bool {
    value
        .strip_prefix('r')
        .and_then(|value| value.parse::<i64>().ok())
        .is_some_and(|number| number > 0 && value == format!("r{number}"))
}

fn fixed(reference: &ResourceRef) -> bool {
    revision(&reference.version)
        && reference
            .digest
            .strip_prefix("sha256:")
            .is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
}

impl Input {
    pub(crate) fn validate(&self) -> Result<()> {
        label(&self.0.id)?;
        label(&self.0.task_ref.resource_id)?;
        label(&self.0.artifact_ref.resource_id)?;
        require(
            revision(&self.0.expected_resource_version)
                && fixed(&self.0.task_ref)
                && fixed(&self.0.artifact_ref),
            "InvalidArgument",
            "work item evidence requires fixed WorkItem, Task and Artifact revisions",
        )
    }

    pub(crate) fn target(&self, db: &Db<'_>, actor: &str) -> Result<Doc> {
        evidence_target(db, actor, &self.0)
    }

    pub(crate) fn submit(&self, db: &Db<'_>, actor: &str) -> Result<Value> {
        let item = attach_evidence(db, actor, &self.0)?;
        Ok(json!({"work_item_ref":item.reference(),"lifecycle":item.body["lifecycle"]}))
    }
}
