//! Requests for extending a settled schedule through a new bounded interval series.

use crate::{ApiResult, Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRenewalRequest {
    pub id: String,
    pub expected_resource_version: String,
    pub key: String,
    pub delegation_id: String,
    pub run_at: String,
    pub deadline_at: String,
}

impl Client {
    /// Renew a completed interval schedule under a currently authorized delegation.
    pub fn renew_schedule(&self, request: &ScheduleRenewalRequest) -> ApiResult<Value> {
        self.call("Schedule.Renew", json!(request))
    }
}
