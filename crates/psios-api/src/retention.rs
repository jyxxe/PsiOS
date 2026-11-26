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
