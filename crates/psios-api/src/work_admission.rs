use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum WorkAdmissionMode {
    Open,
    Draining,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkAdmissionState {
    pub mode: WorkAdmissionMode,
    pub reason: String,
    pub changed_by: String,
    pub changed_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkCounts {
    pub phases: BTreeMap<String, i64>,
    pub recovery_admissions: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelWorkCounts {
    pub current_leases: i64,
    pub paused_outcomes_unknown: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeWorkAdmission {
    pub reference: ResourceRef,
    pub admission: WorkAdmissionState,
    pub task_counts: TaskWorkCounts,
    pub model_counts: ModelWorkCounts,
}

impl Client {
    /// Read local work admission and the current accepted work census.
    pub fn node_work_admission(&self) -> ApiResult<NodeWorkAdmission> {
        serde_json::from_value(self.call("Node.WorkAdmission.Get", json!({}))?)
            .map_err(|_| invalid("invalid node work admission response"))
    }

    /// Update the exact observed admission revision as the local owner.
    pub fn set_node_work_admission(
        &self,
        expected_resource_version: &str,
        mode: WorkAdmissionMode,
        reason: &str,
    ) -> ApiResult<NodeWorkAdmission> {
        let response = self.call(
            "Node.WorkAdmission.Set",
            json!({"expected_resource_version":expected_resource_version,"mode":mode,"reason":reason}),
        )?;
        serde_json::from_value(response)
            .map_err(|_| invalid("invalid node work admission response"))
    }
}
