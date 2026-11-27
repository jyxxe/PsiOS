use super::ModelTokenUsage;
use crate::{ApiError, ApiResult, Client, ResourceRef};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The model charges and accepted provider measurements for one Session.
#[derive(Debug, Deserialize, Serialize)]
pub struct SessionModelUsage {
    pub session_ref: ResourceRef,
    pub budget_ref: ResourceRef,
    pub charged_calls: u32,
    pub lease_time: ModelLeaseTimeUsage,
    pub provider_reported: ProviderReportedModelUsage,
    pub calls: Vec<ModelCallUsage>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ModelLeaseTimeUsage {
    pub active_reserved_ms: u64,
    pub settled_ms: u64,
    pub retained_unknown_ms: u64,
    pub unmetered_calls: u32,
}

/// Sum of token counts from accepted Provider reports, with coverage by call.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct ProviderReportedModelUsage {
    pub reported_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ModelCallOutcome {
    Active,
    Responded,
    NotStarted,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ModelCallUsage {
    pub turn: u32,
    pub lease_id: String,
    pub outcome: ModelCallOutcome,
    pub charged_lease_ms: Option<u32>,
    pub provider_reported_tokens: Option<ModelTokenUsage>,
}

impl Client {
    /// Read a Session's authority charges and accepted provider measurements.
    pub fn session_model_usage(&self, id: &str) -> ApiResult<SessionModelUsage> {
        let reply = self.call("Session.ModelUsage", json!({"id": id}))?;
        let usage: SessionModelUsage = serde_json::from_value(reply).map_err(|_| {
            ApiError::new("RecoveryRequired", "invalid session model usage response")
        })?;
        if usage.session_ref.resource_id != id {
            return Err(
                ApiError::new("RecoveryRequired", "session model usage identity changed").into(),
            );
        }
        Ok(usage)
    }
}
