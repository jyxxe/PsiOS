//! Provider implementations run as authenticated peers. Only the authority can commit a tool step.
use crate::{ApiError, ApiResult, Client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelDecision {
    Tool { tool: String, input: Value },
    Finish { summary: String },
    Fail { reason: String },
}
pub trait SessionModel {
    fn decide(&self, context: &Value) -> ApiResult<ModelDecision>;
}
impl Client {
    pub fn model_turn(
        &self,
        session: &str,
        key: &str,
        provider: &impl SessionModel,
    ) -> ApiResult<Value> {
        let claim = self.call("Model.Claim", json!({"session_id":session,"key":key}))?;
        if let Some(receipt) = claim.get("completed") {
            return Ok(receipt.clone());
        }
        if claim["recovered"] == true {
            return Err(ApiError::new("OutcomeUnknown","provider call may have started; submit the retained decision or wait for a new lease").into());
        }
        let decision = provider.decide(&claim["context"])?;
        let request =
            json!({"session_id":session,"lease_id":claim["lease_id"],"decision":decision});
        match self.call("Model.Respond", request.clone()) {
            Err(e) if e.code == "DependencyUnavailable" => self.call("Model.Respond", request),
            other => other,
        }
    }
}
/// Offline diagnostics provider. It requests the two inventory tools through the same gateway as other providers.
pub struct NodeSurvey;
impl SessionModel for NodeSurvey {
    fn decide(&self, ctx: &Value) -> ApiResult<ModelDecision> {
        let last = &ctx["last_tool"];
        let next = match last["tool"].as_str() {
            None => Some("node.inventory"),
            Some("node.inventory") => Some("node.devices"),
            Some("node.devices") => None,
            _ => {
                return Err(
                    ApiError::new("PreconditionFailed", "unrecognized survey checkpoint").into(),
                );
            }
        };
        if let Some(tool) = next {
            if !ctx["permitted_tools"]
                .as_array()
                .is_some_and(|items| items.iter().any(|v| v == tool))
            {
                return Err(
                    ApiError::new("PermissionDenied", "survey tool absent from skill").into(),
                );
            }
            Ok(ModelDecision::Tool {
                tool: tool.to_owned(),
                input: json!({}),
            })
        } else {
            let devices = last["local_tool_result"]["devices"].as_array();
            let summary = devices
                .map(|v| {
                    format!(
                        "Collected node inventory and observed {} block devices.",
                        v.len()
                    )
                })
                .unwrap_or_else(|| {
                    "Collected node inventory and block device result references.".into()
                });
            Ok(ModelDecision::Finish { summary })
        }
    }
}
