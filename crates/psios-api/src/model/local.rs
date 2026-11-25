//! A local, structured inventory provider over a UID-checked Unix socket.
use super::{
    ModelDecision, ModelInferenceFailure, ModelInferenceOutcome, ModelOutput, ModelTokenUsage,
    SessionModel,
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
const BODY_LIMIT: usize = 256 * 1024;

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

    async fn infer(&self, body: Vec<u8>, may_have_started: &Cell<bool>) -> ApiResult<Value> {
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

    fn attempt(&self, context: &Value, may_have_started: &Cell<bool>) -> ApiResult<ModelOutput> {
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
        let rendered = context.to_string();
        if rendered.len() > 131_072 {
            return Err(ApiError::new("QuotaExceeded", "local model context too large").into());
        }
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
        let response = runtime
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_millis(u64::from(self.timeout_ms)),
                    self.infer(bytes, may_have_started),
                )
                .await
            })
            .map_err(|_| ApiError::new("DeadlineExceeded", "local inference deadline reached"))??;
        Ok(ModelOutput {
            decision: decode_decision(&response, &self.model_alias, tools)?,
            usage: decode_usage(&response["usage"])?,
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
        let may_have_started = Cell::new(false);
        self.attempt(context, &may_have_started)
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
    let choices = response["choices"]
        .as_array()
        .ok_or_else(|| invalid("missing model choices"))?;
    if response["model"] != model
        || choices.len() != 1
        || choices[0]["finish_reason"] != "stop"
        || choices[0]["message"]["role"] != "assistant"
    {
        return Err(ApiError::new(
            "PreconditionFailed",
            "local model returned an incomplete or mismatched response",
        )
        .into());
    }
    let content = choices[0]["message"]["content"]
        .as_str()
        .ok_or_else(|| invalid("missing structured model decision"))?;
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
