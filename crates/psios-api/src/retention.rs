use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareResultEvictionBatch {
    pub task_refs: Vec<ResourceRef>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_content_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_seconds: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionPlanItem {
    pub task_ref: ResourceRef,
    pub content_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionCandidatesRequest {
    pub updated_before: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionCandidateSkips {
    pub pinned: u32,
    pub ineligible: u32,
    pub changed: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionCandidates {
    pub items: Vec<ResultEvictionPlanItem>,
    pub next_cursor: Option<String>,
    pub scanned: u32,
    pub skipped: ResultEvictionCandidateSkips,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionPlan {
    pub plan_token: String,
    pub items: Vec<ResultEvictionPlanItem>,
    pub content_bytes: u64,
    pub valid_until: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_seconds: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionBatch {
    pub reference: ResourceRef,
    pub batch: ResultEvictionBatchReceipt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionBatchReceipt {
    pub plan_digest: String,
    pub reason: String,
    pub eviction_refs: Vec<ResourceRef>,
    pub content_bytes_removed: String,
    pub evicted_by: String,
    pub evicted_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule_ref: Option<ResourceRef>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum ResultEvictionScheduleLifecycle {
    Scheduled,
    Evicted,
    Suspended,
    Rejected,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionSchedule {
    pub reference: ResourceRef,
    pub schedule: ResultEvictionScheduleStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultEvictionScheduleStatus {
    pub lifecycle: ResultEvictionScheduleLifecycle,
    pub plan_digest: String,
    pub admitted_at: String,
    pub admitted_boot_ms: i64,
    pub due_at: String,
    pub due_boot_ms: i64,
    pub delay_seconds: u32,
    pub reason: String,
    pub content_bytes: u64,
    pub items: Vec<ResultEvictionPlanItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_ref: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_ref: Option<ResourceRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_boot_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Client {
    /// Find fixed Task result revisions that are currently eligible for planning.
    pub fn result_eviction_candidates(
        &self,
        request: &ResultEvictionCandidatesRequest,
    ) -> ApiResult<ResultEvictionCandidates> {
        let request =
            serde_json::to_value(request).map_err(|_| invalid("invalid eviction query"))?;
        serde_json::from_value(self.call("Task.ResultEvictionCandidates", request)?)
            .map_err(|_| invalid("invalid eviction query response"))
    }

    /// Preview reclamation for exact Task revisions and obtain a short-lived plan token.
    pub fn prepare_result_eviction_batch(
        &self,
        request: &PrepareResultEvictionBatch,
    ) -> ApiResult<ResultEvictionPlan> {
        let request =
            serde_json::to_value(request).map_err(|_| invalid("invalid eviction request"))?;
        serde_json::from_value(self.call("Task.PrepareResultEvictionBatch", request)?)
            .map_err(|_| invalid("invalid eviction plan response"))
    }

    /// Reclaim the entire prepared batch and return its durable receipt.
    pub fn apply_result_eviction_batch(&self, plan_token: &str) -> ApiResult<ResultEvictionBatch> {
        let result = self.call(
            "Task.ApplyResultEvictionBatch",
            json!({"plan_token":plan_token}),
        )?;
        serde_json::from_value(result).map_err(|_| invalid("invalid eviction batch receipt"))
    }

    /// Admit the delayed plan once, preserving the original schedule on retry.
    pub fn schedule_result_eviction_batch(
        &self,
        plan_token: &str,
    ) -> ApiResult<ResultEvictionSchedule> {
        let result = self.call(
            "Task.ScheduleResultEvictionBatch",
            json!({"plan_token":plan_token}),
        )?;
        serde_json::from_value(result).map_err(|_| invalid("invalid eviction schedule"))
    }

    /// Read the current state and exact revision of an eviction schedule.
    pub fn get_result_eviction_schedule(&self, id: &str) -> ApiResult<ResultEvictionSchedule> {
        let result = self.call("Task.GetResultEvictionSchedule", json!({"id":id}))?;
        serde_json::from_value(result).map_err(|_| invalid("invalid eviction schedule"))
    }

    /// Cancel the exact pending revision before it settles.
    pub fn cancel_result_eviction_schedule(
        &self,
        id: &str,
        expected_resource_version: &str,
    ) -> ApiResult<ResultEvictionSchedule> {
        let result = self.call(
            "Task.CancelResultEvictionSchedule",
            json!({"id":id,"expected_resource_version":expected_resource_version}),
        )?;
        serde_json::from_value(result).map_err(|_| invalid("invalid eviction schedule"))
    }
}
