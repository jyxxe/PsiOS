//! Provider implementations run as authenticated peers. Only the authority can commit a tool step.
mod journal;
#[cfg(feature = "local-inference")]
mod local;
use crate::{ApiError, ApiResult, Client, ResourceRef};
#[cfg(feature = "local-inference")]
pub use local::LocalInventoryModel;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelDecision {
    Tool { tool: String, input: Value },
    Finish { summary: String },
    Fail { reason: String },
}
/// Token quantities reported by the selected inference provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

pub struct ModelOutput {
    pub decision: ModelDecision,
    pub usage: Option<ModelTokenUsage>,
}

/// Outcome knowledge from the provider transport, separate from model output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelInferenceOutcome {
    NotStarted,
    Unknown,
}

#[derive(Debug)]
pub struct ModelInferenceFailure {
    pub outcome: ModelInferenceOutcome,
    pub error: Box<ApiError>,
}

/// Settlement of an authenticated provider's failed attempt for an exact lease.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFailureReport {
    pub session_id: String,
    pub model_ref: ResourceRef,
    pub lease_id: String,
    pub outcome: ModelInferenceOutcome,
}

pub trait SessionModel {
    fn decide(&self, context: &Value) -> ApiResult<ModelDecision>;

    /// Opt into receiving authorized artifact data in a directly claimed session.
    fn accepts_session_inputs(&self) -> bool {
        false
    }

    fn decide_with_usage(&self, context: &Value) -> ApiResult<ModelOutput> {
        Ok(ModelOutput {
            decision: self.decide(context)?,
            usage: None,
        })
    }

    /// Only report `NotStarted` when the transport knows inference could not have started.
    fn decide_with_outcome(&self, context: &Value) -> Result<ModelOutput, ModelInferenceFailure> {
        self.decide_with_usage(context)
            .map_err(|error| ModelInferenceFailure {
                outcome: ModelInferenceOutcome::Unknown,
                error,
            })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest {
    session_id: String,
    lease_id: String,
    decision: ModelDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<ModelTokenUsage>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Delivery {
    Decision(DecisionRequest),
    Failure(ModelFailureReport),
}

impl Delivery {
    fn lease_id(&self) -> &str {
        match self {
            Self::Decision(request) => &request.lease_id,
            Self::Failure(report) => &report.lease_id,
        }
    }

    fn session_id(&self) -> &str {
        match self {
            Self::Decision(request) => &request.session_id,
            Self::Failure(report) => &report.session_id,
        }
    }

    fn format(&self) -> u32 {
        match self {
            Self::Decision(request) => {
                if request.usage.is_some() {
                    2
                } else {
                    1
                }
            }
            Self::Failure(_) => 3,
        }
    }

    fn method(&self) -> &'static str {
        match self {
            Self::Decision(_) => "Model.Respond",
            Self::Failure(_) => "Model.ReportFailure",
        }
    }
}

enum Turn {
    Completed(Value),
    Deliver {
        request: Delivery,
        late_decision_model_ref: Option<ResourceRef>,
    },
}

fn direct_claim_parameters(
    session: &str,
    key: &str,
    provider: &(impl SessionModel + ?Sized),
) -> Value {
    let mut params = json!({"session_id":session,"key":key});
    if provider.accepts_session_inputs() {
        params["accepts_session_inputs"] = json!(true);
    }
    params
}

impl Client {
    fn decide_turn(
        &self,
        session: &str,
        key: &str,
        provider: &(impl SessionModel + ?Sized),
    ) -> ApiResult<Turn> {
        let claim = self.call(
            "Model.Claim",
            direct_claim_parameters(session, key, provider),
        )?;
        Self::decide_claim(session, claim, provider, None)
    }

    fn decide_claim(
        session: &str,
        claim: Value,
        provider: &(impl SessionModel + ?Sized),
        expected_model: Option<&ResourceRef>,
    ) -> ApiResult<Turn> {
        if let Some(receipt) = claim.get("completed") {
            return Ok(Turn::Completed(receipt.clone()));
        }
        if claim["recovered"] == true {
            let message = if claim["inference_outcome_policy"] == "Strict" {
                "provider call may have started. Recover a retained delivery for this lease"
            } else {
                "provider call may have started. Submit the retained decision or wait for a new lease"
            };
            return Err(ApiError::new("OutcomeUnknown", message).into());
        }
        let lease_id = claim["lease_id"]
            .as_str()
            .ok_or_else(|| crate::invalid("missing model lease"))?
            .to_owned();
        if claim["context"]["session_id"] != session {
            return Err(crate::invalid(
                "model lease context belongs to another session",
            ));
        }
        if let Some(expected) = claim.get("context_digest")
            && expected.as_str()
                != Some(crate::sha256(&crate::canonical(&claim["context"])).as_str())
        {
            return Err(crate::invalid("model lease context digest differs"));
        }
        let context = &claim["context"];
        if context.get("inputs").is_some()
            && (expected_model.is_some() || !provider.accepts_session_inputs())
        {
            return Err(ApiError::new(
                "UnsupportedCapability",
                "model provider has not opted into session artifact inputs",
            )
            .into());
        }
        let observe_late = match claim.get("late_decision_observation") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) if claim["inference_outcome_policy"] == "Strict" => true,
            Some(_) => {
                return Err(crate::invalid(
                    "invalid late decision observation capability",
                ));
            }
        };
        let (output, late_decision_model_ref) = match claim.get("inference_outcome_policy") {
            Some(policy) if policy == "Strict" => {
                let model_ref: ResourceRef =
                    serde_json::from_value(context["model_ref"].clone())
                        .map_err(|_| crate::invalid("strict model lease has no model reference"))?;
                if expected_model.is_some_and(|expected| expected != &model_ref) {
                    return Err(crate::invalid(
                        "strict model lease belongs to another model",
                    ));
                }
                if observe_late && claim["model_ref"] != json!(model_ref) {
                    return Err(crate::invalid(
                        "late decision capability model reference differs",
                    ));
                }
                match provider.decide_with_outcome(context) {
                    Ok(output) => (output, observe_late.then_some(model_ref)),
                    Err(failure) => {
                        return Ok(Turn::Deliver {
                            request: Delivery::Failure(ModelFailureReport {
                                session_id: session.to_owned(),
                                model_ref,
                                lease_id,
                                outcome: failure.outcome,
                            }),
                            late_decision_model_ref: None,
                        });
                    }
                }
            }
            None => (provider.decide_with_usage(context)?, None),
            Some(_) => {
                return Err(ApiError::new(
                    "UnsupportedCapability",
                    "unsupported model inference outcome policy",
                )
                .into());
            }
        };
        Ok(Turn::Deliver {
            request: Delivery::Decision(DecisionRequest {
                session_id: session.to_owned(),
                lease_id,
                decision: output.decision,
                usage: output
                    .usage
                    .filter(|_| claim["usage_format"] == "tokens.v1"),
            }),
            late_decision_model_ref,
        })
    }

    fn send_delivery(&self, delivery: &Delivery) -> ApiResult<Value> {
        let method = delivery.method();
        let request = json!(delivery);
        match self.call(method, request.clone()) {
            Err(e) if e.code == "DependencyUnavailable" => self.call(method, request),
            other => other,
        }
    }

    /// Submit an outcome for an adapter coordinating its own provider transport.
    pub fn report_model_failure(&self, report: ModelFailureReport) -> ApiResult<Value> {
        self.send_delivery(&Delivery::Failure(report))
    }

    pub fn resume_session(&self, session: &str, revision: &str) -> ApiResult<Value> {
        self.call(
            "Session.Resume",
            json!({"id":session,"expected_resource_version":revision}),
        )
    }

    pub fn model_turn(
        &self,
        session: &str,
        key: &str,
        provider: &(impl SessionModel + ?Sized),
    ) -> ApiResult<Value> {
        match self.decide_turn(session, key, provider)? {
            Turn::Completed(receipt) => Ok(receipt),
            Turn::Deliver { request, .. } => self.send_delivery(&request),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn direct_model_input_receipt_requires_explicit_provider_support() {
        struct InputProvider(Cell<bool>);
        impl SessionModel for InputProvider {
            fn decide(&self, context: &Value) -> ApiResult<ModelDecision> {
                assert_eq!(context["inputs"][0]["value"], json!({"inventory": true}));
                self.0.set(true);
                Ok(ModelDecision::Finish {
                    summary: "Recorded.".into(),
                })
            }

            fn accepts_session_inputs(&self) -> bool {
                true
            }
        }

        let provider = InputProvider(Cell::new(false));
        assert_eq!(
            direct_claim_parameters("session-fixture", "first", &NodeSurvey),
            json!({"session_id":"session-fixture","key":"first"})
        );
        assert_eq!(
            direct_claim_parameters("session-fixture", "first", &provider),
            json!({"session_id":"session-fixture","key":"first","accepts_session_inputs":true})
        );
        let mut claim = json!({"lease_id":"lease-fixture","context":{
            "session_id":"session-fixture","inputs":[{"value":{"inventory":true}}],
        }});
        assert!(
            matches!(Client::decide_claim("session-fixture", claim.clone(), &NodeSurvey, None),
                         Err(error) if error.code == "UnsupportedCapability")
        );
        let queued = ResourceRef {
            resource_id: "model-fixture".into(),
            version: "r1".into(),
            digest: "sha256:fixture".into(),
        };
        assert!(
            matches!(Client::decide_claim("session-fixture", claim.clone(), &provider, Some(&queued)),
                         Err(error) if error.code == "UnsupportedCapability")
        );
        assert!(!provider.0.get());
        assert!(matches!(
            Client::decide_claim("session-fixture", claim.clone(), &provider, None),
            Ok(Turn::Deliver {
                request: Delivery::Decision(_),
                ..
            })
        ));
        assert!(provider.0.get());

        let receipt = json!({"task_id":"existing-task"});
        claim["completed"] = receipt.clone();
        assert!(
            matches!(Client::decide_claim("session-fixture", claim, &NodeSurvey, None),
                         Ok(Turn::Completed(previous)) if previous == receipt)
        );
    }

    #[test]
    fn only_a_fresh_strict_claim_invokes_the_provider_and_reports_its_failure() {
        struct Offline(Cell<u32>);
        impl SessionModel for Offline {
            fn decide(&self, _: &Value) -> ApiResult<ModelDecision> {
                self.0.set(self.0.get() + 1);
                Err(ApiError::new("DependencyUnavailable", "fixture unavailable").into())
            }
        }
        let provider = Offline(Cell::new(0));
        let model = ResourceRef {
            resource_id: "model-fixture".into(),
            version: "r1".into(),
            digest: crate::sha256(b"fixture"),
        };
        let mut claim = json!({"lease_id":"lease-fixture","recovered":true,
            "context":{"session_id":"session-fixture","model_ref":model},
            "inference_outcome_policy":"Strict"});
        assert!(matches!(
            Client::decide_claim("session-fixture", claim.clone(), &provider, Some(&model)),
            Err(error) if error.code == "OutcomeUnknown"
        ));
        assert_eq!(provider.0.get(), 0);
        claim["recovered"] = json!(false);
        let pending =
            Client::decide_claim("session-fixture", claim.clone(), &provider, Some(&model));
        assert!(
            matches!(pending, Ok(Turn::Deliver { request: Delivery::Failure(report), late_decision_model_ref: None })
            if report.model_ref == model && report.outcome == ModelInferenceOutcome::Unknown)
        );
        claim
            .as_object_mut()
            .unwrap()
            .remove("inference_outcome_policy");
        assert!(matches!(
            Client::decide_claim("session-fixture", claim, &provider, None),
            Err(error) if error.code == "DependencyUnavailable"
        ));
        assert_eq!(provider.0.get(), 2);
    }
}
