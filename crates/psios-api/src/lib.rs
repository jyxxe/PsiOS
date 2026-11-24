//! Local system API contracts, bounded framing and synchronous Rust client.
mod backup;
mod client;
mod model;
pub use backup::{BackupChunk, BackupMember, BackupMemberReceipt};
pub use model::*;
mod json;
pub use client::*;
pub use json::{canonical, contract, digest, parse, sha256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const VERSION: &str = "1.0.0";
pub const MAX_FRAME: usize = 512 * 1024;
pub const MAX_ARTIFACT_CHUNK: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResourceRef {
    pub resource_id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: String,
    pub request_id: String,
    pub method: String,
    pub token: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub schema_version: String,
    pub code: String,
    pub message: String,
    pub request_id: String,
    pub retryable: bool,
    pub retry_condition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}
impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        let condition = match code {
            "OutcomeUnknown" | "RecoveryRequired" => "ReconcileOutcome",
            "Conflict" | "ApprovalInvalidated" | "PreconditionFailed" => "Replan",
            "QueueFull" => "SameRequestAfterBackoff",
            "CursorExpired" => "Resynchronize",
            "AuthenticationRequired" => "Reauthenticate",
            "DependencyUnavailable" => "DependencyReady",
            _ => "Never",
        };
        Self {
            schema_version: VERSION.into(),
            code: code.into(),
            message: message.into(),
            request_id: "unassigned".into(),
            retryable: condition == "SameRequestAfterBackoff",
            retry_condition: condition.into(),
            operation_id: None,
        }
    }
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ApiError {}
pub type ApiResult<T> = Result<T, Box<ApiError>>;
pub fn invalid(msg: impl Into<String>) -> Box<ApiError> {
    Box::new(ApiError::new("InvalidArgument", msg))
}
pub fn id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}
