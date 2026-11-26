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
}
