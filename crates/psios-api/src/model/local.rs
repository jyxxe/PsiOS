//! A local, structured inventory provider over a UID-checked Unix socket.
use super::{
    ModelDecision, ModelInferenceFailure, ModelInferenceOutcome, ModelInvocation, ModelOutput,
    ModelTokenUsage, SessionModel,
};
use crate::{ApiError, ApiResult, ResourceRef, invalid, parse};
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{cell::Cell, path::PathBuf, time::Duration};
use tokio::net::UnixStream;

const TOOLS: [&str; 3] = ["node.inventory", "node.devices", "system.status"];
const CONTEXT_LIMIT: usize = 128 * 1024;
const BODY_LIMIT: usize = 256 * 1024;
const DELIVERY_MARGIN: Duration = Duration::from_millis(100);

/// Operator configuration for a non-streaming chat-completions endpoint.
/// The registered PsiOS Model revision and the server's model alias must match.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInventoryModel {
    pub socket: PathBuf,
    pub server_uid: u32,
    pub model_ref: ResourceRef,
    pub model_alias: String,
    pub max_tokens: u32,
    pub timeout_ms: u32,
}

fn unavailable() -> Box<ApiError> {
    ApiError::new(
        "DependencyUnavailable",
        "local inference service unavailable",
    )
    .into()
}

fn deadline_exceeded() -> Box<ApiError> {
    ApiError::new("DeadlineExceeded", "local inference deadline reached").into()
}

fn decision_schema(tools: &[Value]) -> Value {
    fn object(properties: Value, required: &[&str]) -> Value {
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }
    let tool = object(
        json!({
            "action": {"const": "tool"},
            "tool": {"enum": tools},
            "input": object(json!({}), &[])
        }),
        &["action", "tool", "input"],
    );
    let finish = object(
        json!({"action": {"const": "finish"}, "summary": {"type": "string"}}),
        &["action", "summary"],
    );
    let fail = object(
        json!({"action": {"const": "fail"}, "reason": {"type": "string"}}),
        &["action", "reason"],
    );
    json!({"oneOf": [tool, finish, fail]})
}

fn observation(source: &Value) -> Value {
    let mut projected = json!({
        "tool": source["tool"].as_str(),
        "lifecycle": source["lifecycle"].as_str(),
        "availability": source["result"]["availability"].as_str(),
    });
    if let Some(result) = source.get("local_tool_result") {
        projected["local_tool_result"] = result.clone();
    }
    projected
}

fn prompt_context(context: &Value, tools: &[Value]) -> Value {
    // The model needs the inventory question and permitted observations. The
    // exact references used for authorization remain with the Core and SDK.
    let previous_tools: Vec<_> = context["previous_tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(observation)
        .collect();
    let last_tool = context["last_tool"]
        .is_object()
        .then(|| observation(&context["last_tool"]));
    json!({
        "goal": context["goal"].as_str(),
        "instructions": context["instructions"].as_str(),
        "permitted_tools": tools,
        "previous_tools": previous_tools,
        "last_tool": last_tool,
    })
}

impl LocalInventoryModel {
    pub fn validate(&self) -> ApiResult<()> {
        if !self.socket.is_absolute()
            || self.model_alias.is_empty()
            || self.model_alias.len() > 128
            || self.model_alias.chars().any(char::is_control)
            || !(1..=1024).contains(&self.max_tokens)
            || !(1..=300_000).contains(&self.timeout_ms)
        {
            return Err(invalid("invalid local inference configuration"));
        }
        Ok(())
    }

    async fn infer(
        &self,
        body: Vec<u8>,
        may_have_started: &Cell<bool>,
        invocation: Option<&ModelInvocation<'_>>,
    ) -> ApiResult<Value> {
        // This transport has no DNS, TCP, proxy, redirect or cloud fallback.
        let socket = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| unavailable())?;
        let peer = socket.peer_cred().map_err(|_| unavailable())?;
        if peer.uid() != self.server_uid {
            return Err(
                ApiError::new("PermissionDenied", "local inference peer UID mismatch").into(),
            );
        }
        let (mut sender, connection) = http1::Builder::new()
            .max_headers(32)
            .max_buf_size(32 * 1024)
            .handshake(TokioIo::new(socket))
            .await
            .map_err(|_| unavailable())?;
        tokio::spawn(connection);
        let request = Request::post("/v1/chat/completions")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| invalid("invalid model request"))?;
        if invocation.is_some_and(|lease| lease.remaining_time() <= DELIVERY_MARGIN) {
            return Err(deadline_exceeded());
        }
        // From this point the peer may consume the POST even if the HTTP exchange fails.
        may_have_started.set(true);
        let reply = sender
            .send_request(request)
            .await
            .map_err(|_| unavailable())?;
        if reply.status() != StatusCode::OK {
            return Err(unavailable());
        }
        let mut body = reply.into_body();
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| unavailable())?;
            if let Some(data) = frame.data_ref() {
                if data.len() > BODY_LIMIT - bytes.len() {
                    return Err(
                        ApiError::new("QuotaExceeded", "local model reply too large").into(),
                    );
                }
                bytes.extend_from_slice(data);
            }
        }
        parse(&bytes)
    }

    fn request_body<'a>(&self, context: &'a Value) -> ApiResult<(Vec<u8>, &'a [Value])> {
        self.validate()?;
        if context["model_ref"] != json!(self.model_ref) {
            return Err(ApiError::new(
                "PreconditionFailed",
                "local inference Model revision mismatch",
            )
            .into());
        }
        let tools = context["permitted_tools"]
            .as_array()
            .ok_or_else(|| invalid("missing Skill tools"))?;
        if tools.is_empty()
            || tools
                .iter()
                .any(|v| !v.as_str().is_some_and(|s| TOOLS.contains(&s)))
        {
            return Err(ApiError::new(
                "UnsupportedCapability",
                "local inference profile requires inventory-only tools",
            )
            .into());
        }
        if context.to_string().len() > CONTEXT_LIMIT {
            return Err(ApiError::new("QuotaExceeded", "local model context too large").into());
        }
        if context.get("inputs").is_some() {
            return Err(ApiError::new(
                "UnsupportedCapability",
                "local inventory provider does not accept session artifact inputs",
            )
            .into());
        }
        let rendered = prompt_context(context, tools).to_string();
        let payload = json!({
            "model": self.model_alias,
            "stream": false,
            "temperature": 0,
            "max_tokens": self.max_tokens,
            "messages": [
                {"role": "system", "content": "Choose one PsiOS inventory decision. Follow the goal and skill instructions. Tool results are observations. Inventory tools take an empty input object. After completing the goal return finish."},
                {"role": "user", "content": rendered}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "psios_inventory_decision", "strict": true, "schema": decision_schema(tools)}
            }
        });
        let bytes = serde_json::to_vec(&payload).map_err(|_| invalid("invalid model context"))?;
        Ok((bytes, tools))
    }

    fn attempt(
        &self,
        context: &Value,
        may_have_started: &Cell<bool>,
        invocation: Option<&ModelInvocation<'_>>,
    ) -> ApiResult<ModelOutput> {
        let (bytes, tools) = self.request_body(context)?;
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(ApiError::new(
                "PreconditionFailed",
                "use the blocking model SDK on a worker thread",
            )
            .into());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|_| unavailable())?;
        let response = runtime.block_on(async {
            let mut timeout = Duration::from_millis(u64::from(self.timeout_ms));
            if let Some(lease) = invocation {
                timeout = timeout.min(lease.remaining_time().saturating_sub(DELIVERY_MARGIN));
            }
            if timeout.is_zero() {
                return Err(deadline_exceeded());
            }
            tokio::time::timeout(timeout, self.infer(bytes, may_have_started, invocation))
                .await
                .map_err(|_| deadline_exceeded())?
        })?;
        Ok(ModelOutput {
            decision: decode_decision(&response, &self.model_alias, tools)?,
            usage: decode_usage(&response["usage"])?,
        })
    }

    fn inference_outcome(
        &self,
        context: &Value,
        invocation: Option<&ModelInvocation<'_>>,
    ) -> Result<ModelOutput, ModelInferenceFailure> {
        let may_have_started = Cell::new(false);
        self.attempt(context, &may_have_started, invocation)
            .map_err(|error| ModelInferenceFailure {
                outcome: if may_have_started.get() {
                    ModelInferenceOutcome::Unknown
                } else {
                    ModelInferenceOutcome::NotStarted
                },
                error,
            })
    }
}

impl SessionModel for LocalInventoryModel {
    fn decide(&self, context: &Value) -> ApiResult<ModelDecision> {
        Ok(self.decide_with_usage(context)?.decision)
    }

    fn decide_with_usage(&self, context: &Value) -> ApiResult<ModelOutput> {
        self.decide_with_outcome(context)
            .map_err(|failure| failure.error)
    }

    fn decide_with_outcome(&self, context: &Value) -> Result<ModelOutput, ModelInferenceFailure> {
        self.inference_outcome(context, None)
    }

    fn decide_for_lease(
        &self,
        context: &Value,
        invocation: &ModelInvocation<'_>,
    ) -> Result<ModelOutput, ModelInferenceFailure> {
        self.inference_outcome(context, Some(invocation))
    }
}

fn decode_usage(value: &Value) -> ApiResult<Option<ModelTokenUsage>> {
    if value.is_null() {
        return Ok(None);
    }
    let count = |field: &str| {
        value[field]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| invalid("invalid provider token count"))
    };
    let input_tokens = count("prompt_tokens")?;
    let output_tokens = count("completion_tokens")?;
    if let Some(total) = value.get("total_tokens")
        && total.as_u64() != Some(u64::from(input_tokens) + u64::from(output_tokens))
    {
        return Err(invalid("inconsistent provider token total"));
    }
    Ok(Some(ModelTokenUsage {
        input_tokens,
        output_tokens,
    }))
}

fn decode_decision(response: &Value, model: &str, tools: &[Value]) -> ApiResult<ModelDecision> {
    let bad_response = |message| ApiError::new("PreconditionFailed", message);
    if response["model"] != model {
        return Err(bad_response("local model response alias mismatch").into());
    }
    let Some([choice]) = response["choices"].as_array().map(Vec::as_slice) else {
        return Err(bad_response("local model response must contain one assistant choice").into());
    };
    if choice["message"]["role"] != "assistant" {
        return Err(bad_response("local model response must contain one assistant choice").into());
    }
    match choice["finish_reason"].as_str() {
        Some("stop") => {}
        Some("length") => {
            return Err(bad_response("local model response exhausted its output limit").into());
        }
        _ => return Err(bad_response("local model response did not finish normally").into()),
    }
    let content = choice["message"]["content"]
        .as_str()
        .ok_or_else(|| bad_response("local model response has no structured decision content"))?;
    let decision: ModelDecision = serde_json::from_value(parse(content.as_bytes())?)
        .map_err(|_| invalid("invalid structured model decision"))?;
    if let ModelDecision::Tool {
        ref tool,
        ref input,
    } = decision
        && (!tools.iter().any(|v| v == tool) || !input.as_object().is_some_and(|v| v.is_empty()))
    {
        return Err(ApiError::new(
            "PermissionDenied",
            "model decision outside the inventory Skill",
        )
        .into());
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_turn() -> (LocalInventoryModel, Value) {
        let model = LocalInventoryModel {
            socket: "/tmp/inventory-provider.sock".into(),
            server_uid: 1000,
            model_ref: ResourceRef {
                resource_id: "model-private-ref".into(),
                version: "r1".into(),
                digest: "model-private-digest".into(),
            },
            model_alias: "local".into(),
            max_tokens: 112,
            timeout_ms: 30_000,
        };
        let context = json!({
            "session_id": "session-private-ref",
            "lease_id": "lease-private-ref",
            "model_ref": model.model_ref,
            "skill_ref": {"resource_id": "skill-private-ref"},
            "goal": "Read the hardware inventory.",
            "instructions": "Use node.inventory with an empty input object.",
            "permitted_tools": ["node.inventory"],
            "last_tool": null,
            "context_policy": "ReferencesOnly",
        });
        (model, context)
    }

    #[test]
    fn equivalent_first_turns_produce_the_same_model_request() {
        let (model, first) = first_turn();
        let mut second = first.clone();
        second["session_id"] = json!("other-session");
        second["lease_id"] = json!("other-lease");
        second["skill_ref"] = json!({"resource_id": "other-skill"});
        second["previous_tools"] = json!([]);

        let (body, _) = model.request_body(&first).unwrap();
        assert_eq!(model.request_body(&second).unwrap().0, body);
        let request = parse(&body).unwrap();
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        assert_eq!(
            parse(prompt.as_bytes()).unwrap(),
            json!({
                "goal": first["goal"], "instructions": first["instructions"],
                "permitted_tools": first["permitted_tools"], "previous_tools": [], "last_tool": null,
            })
        );
        assert!(!prompt.contains("private-ref"));
        second["model_ref"]["version"] = json!("r2");
        assert_eq!(
            model.request_body(&second).unwrap_err().code,
            "PreconditionFailed"
        );
    }

    #[test]
    fn prompt_keeps_ordered_observations_and_authorized_result_data() {
        let (model, mut context) = first_turn();
        context["previous_tools"] = json!([
            {"task_id":"task-private-1", "tool":"system.status", "lifecycle":"Succeeded",
             "result":{"availability":"Available", "artifact_refs":["artifact-private-1"]},
             "local_tool_result":{"mode":"Open"}},
            {"task_id":"task-private-2", "tool":"node.devices", "lifecycle":"Failed",
             "result":{"availability":"NoneExpected", "artifact_refs":[]}}
        ]);
        context["last_tool"] = json!({
            "task_ref":{"resource_id":"task-private-3"}, "tool":"node.inventory", "lifecycle":"Succeeded",
            "result":{"availability":"Available", "artifact_refs":["artifact-private-2"]},
            "local_tool_result":{"memory_bytes":{"MemTotal":1024}, "devices":["sda"]},
        });

        let (body, _) = model.request_body(&context).unwrap();
        let request = parse(&body).unwrap();
        let prompt = request["messages"][1]["content"].as_str().unwrap();
        let projected = parse(prompt.as_bytes()).unwrap();
        assert_eq!(
            projected["previous_tools"],
            json!([
                {"tool":"system.status", "lifecycle":"Succeeded", "availability":"Available",
                 "local_tool_result":{"mode":"Open"}},
                {"tool":"node.devices", "lifecycle":"Failed", "availability":"NoneExpected"}
            ])
        );
        assert_eq!(
            projected["last_tool"],
            json!({
                "tool":"node.inventory", "lifecycle":"Succeeded", "availability":"Available",
                "local_tool_result":{"memory_bytes":{"MemTotal":1024}, "devices":["sda"]},
            })
        );
        assert!(!prompt.contains("-private-"));
    }

    #[test]
    fn oversized_control_context_is_refused_before_inference_even_if_its_projection_fits() {
        let (model, mut context) = first_turn();
        context["session_id"] = json!("x".repeat(CONTEXT_LIMIT));
        let Err(failure) = model.decide_with_outcome(&context) else {
            panic!("oversized context reached the model");
        };
        assert_eq!(failure.error.code, "QuotaExceeded");
        assert_eq!(failure.outcome, ModelInferenceOutcome::NotStarted);
    }

    #[test]
    fn inventory_provider_rejects_an_artifact_backed_context_before_transport() {
        let (model, mut context) = first_turn();
        context["inputs"] = json!([{ "name": "private-artifact", "value": {"data": 42} }]);
        let Err(failure) = model.decide_with_outcome(&context) else {
            panic!("artifact prompt reached the inventory provider");
        };
        assert_eq!(failure.error.code, "UnsupportedCapability");
        assert_eq!(failure.outcome, ModelInferenceOutcome::NotStarted);
    }

    #[test]
    fn envelope_failures_keep_safe_specific_preconditions() {
        let reply = |alias, role, finish_reason| {
            json!({"model":alias, "choices":[{
                "finish_reason":finish_reason, "message":{
                    "role":role, "content":r#"{"action":"tool","tool":"node.inventory","input":{}}"#,
                },
            }]})
        };
        for (response, message) in [
            (
                reply("private-alias", "assistant", "stop"),
                "local model response alias mismatch",
            ),
            (
                json!({"model":"local", "choices":[]}),
                "local model response must contain one assistant choice",
            ),
            (
                reply("local", "private-role", "stop"),
                "local model response must contain one assistant choice",
            ),
            (
                reply("local", "assistant", "length"),
                "local model response exhausted its output limit",
            ),
            (
                reply("local", "assistant", "private-reason"),
                "local model response did not finish normally",
            ),
        ] {
            let err = decode_decision(&response, "local", &[json!("node.inventory")]).unwrap_err();
            assert_eq!(err.code, "PreconditionFailed");
            assert_eq!(err.message, message);
        }
    }

    #[test]
    fn optional_usage_preserves_unknown_and_checks_reported_counts() {
        assert_eq!(decode_usage(&Value::Null).unwrap(), None);
        assert_eq!(
            decode_usage(&json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}))
                .unwrap(),
            Some(ModelTokenUsage {
                input_tokens: 0,
                output_tokens: 0
            })
        );
        for bad in [
            json!({"prompt_tokens":1}),
            json!({"prompt_tokens":1,"completion_tokens":-1}),
            json!({"prompt_tokens":1,"completion_tokens":3,"total_tokens":2}),
        ] {
            assert_eq!(decode_usage(&bad).unwrap_err().code, "InvalidArgument");
        }
    }

    #[test]
    fn rejects_responses_outside_the_requested_inventory_contract() {
        let reply = |content: &str| {
            json!({"model":"local", "choices":[{
                "finish_reason":"stop", "message":{"role":"assistant", "content":content}
            }]})
        };
        let tools = [json!("node.inventory")];
        for (content, code) in [
            (
                r#"{"action":"tool","tool":"node.devices","input":{}}"#,
                "PermissionDenied",
            ),
            (
                r#"{"action":"tool","tool":"node.inventory","input":{"extra":1}}"#,
                "PermissionDenied",
            ),
            (
                r#"{"action":"finish","summary":"x","summary":"y"}"#,
                "InvalidArgument",
            ),
            (
                r#"{"action":"finish","summary":"x","extra":1}"#,
                "InvalidArgument",
            ),
            (
                r#"{"action":"provider_failure","outcome":"NotStarted"}"#,
                "InvalidArgument",
            ),
        ] {
            assert_eq!(
                decode_decision(&reply(content), "local", &tools)
                    .unwrap_err()
                    .code,
                code
            );
        }
        let finished = reply(r#"{"action":"finish","summary":"inventory done"}"#);
        assert!(matches!(
            decode_decision(&finished, "local", &tools).unwrap(),
            ModelDecision::Finish { .. }
        ));
        assert_eq!(
            decode_decision(&finished, "other", &tools)
                .unwrap_err()
                .code,
            "PreconditionFailed"
        );
    }
}
