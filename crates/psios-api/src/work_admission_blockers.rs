use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockerPage<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBlocker {
    pub phase: String,
    pub task_ref: ResourceRef,
    pub operation_ref: ResourceRef,
    pub operation_lifecycle: String,
    pub outcome_knowledge: String,
    pub recovery_admission: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusiveHoldBlocker {
    pub resource_kind: String,
    pub resource_ref: ResourceRef,
    pub holder_kind: String,
    pub holder_ref: ResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_ref: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_ref: Option<ResourceRef>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeReadBlocker {
    pub volume_ref: ResourceRef,
    pub task_ref: ResourceRef,
    pub operation_ref: ResourceRef,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLeaseBlocker {
    pub session_ref: ResourceRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_ref: Option<ResourceRef>,
    pub lease_id: String,
    pub state: String,
    pub expires_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOutcomeBlocker {
    pub session_ref: ResourceRef,
    pub model_ref: ResourceRef,
    pub lease_id: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeWorkAdmissionBlockers {
    pub reference: ResourceRef,
    pub observed_at: String,
    pub tasks: BlockerPage<TaskBlocker>,
    pub exclusive_holds: BlockerPage<ExclusiveHoldBlocker>,
    pub volume_read_holds: BlockerPage<VolumeReadBlocker>,
    pub model_leases: BlockerPage<ModelLeaseBlocker>,
    pub model_outcomes_unknown: BlockerPage<ModelOutcomeBlocker>,
}

impl Client {
    /// Inspect bounded exact blocker references at the observed admission revision.
    pub fn node_work_admission_blockers(
        &self,
        expected_resource_version: &str,
        limit: Option<u32>,
    ) -> ApiResult<NodeWorkAdmissionBlockers> {
        let response = self.call(
            "Node.WorkAdmission.Blockers",
            json!({"expected_resource_version":expected_resource_version,"limit":limit}),
        )?;
        serde_json::from_value(response)
            .map_err(|_| invalid("invalid node work admission blocker snapshot"))
    }
}
