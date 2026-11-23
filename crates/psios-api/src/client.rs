use crate::*;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    time::Duration,
};

/// Four-byte big-endian length followed by exactly one UTF-8 JSON document.
pub fn read_frame(r: &mut impl Read) -> ApiResult<Vec<u8>> {
    let mut header = [0; 4];
    r.read_exact(&mut header)
        .map_err(|_| invalid("incomplete frame header"))?;
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(invalid("frame outside size limit"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)
        .map_err(|_| invalid("incomplete frame"))?;
    Ok(body)
}
pub fn write_frame(w: &mut impl Write, body: &[u8]) -> ApiResult<()> {
    if body.is_empty() || body.len() > MAX_FRAME {
        return Err(invalid("frame outside size limit"));
    }
    w.write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|_| w.write_all(body))
        .map_err(|_| ApiError::new("DependencyUnavailable", "socket write failed").into())
}

pub struct Client {
    socket: PathBuf,
    token: String,
    service_uid: u32,
}
impl Client {
    pub fn new(socket: impl AsRef<Path>, token: impl Into<String>) -> Self {
        Self {
            socket: socket.as_ref().into(),
            token: token.into(),
            service_uid: rustix::process::geteuid().as_raw(),
        }
    }
    /// Pin the daemon UID when the SDK runs under a separate agent account.
    pub fn with_service_uid(mut self, uid: u32) -> Self {
        self.service_uid = uid;
        self
    }
    pub fn call(&self, method: &str, params: Value) -> ApiResult<Value> {
        let req = Request {
            schema_version: VERSION.into(),
            request_id: id("request"),
            method: method.into(),
            token: self.token.clone(),
            params,
        };
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|_| ApiError::new("DependencyUnavailable", "cannot connect to psiosd"))?;
        let peer = rustix::net::sockopt::socket_peercred(&stream)
            .map_err(|_| invalid("socket peer unavailable"))?;
        if peer.uid.as_raw() != self.service_uid {
            return Err(
                ApiError::new("AuthenticationRequired", "system authority UID mismatch").into(),
            );
        }
        let timeout = Some(Duration::from_secs(15));
        stream
            .set_read_timeout(timeout)
            .and_then(|_| stream.set_write_timeout(timeout))
            .map_err(|_| invalid("socket timeout"))?;
        write_frame(
            &mut stream,
            &serde_json::to_vec(&req).map_err(|_| invalid("request encoding"))?,
        )?;
        let bytes = read_frame(&mut stream).map_err(|_| {
            ApiError::new(
                "DependencyUnavailable",
                "complete authority response unavailable",
            )
        })?;
        let response = parse(&bytes)?;
        if response["request_id"] != req.request_id {
            return Err(invalid("response correlation mismatch"));
        }
        if let Some(e) = response.get("error") {
            contract("error", e)?;
            let e: ApiError =
                serde_json::from_value(e.clone()).map_err(|_| invalid("invalid error response"))?;
            return Err(Box::new(e));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| invalid("missing result"))
    }
    pub fn submit(
        &self,
        tool: &str,
        input: Value,
        key: &str,
        delegation: &str,
    ) -> ApiResult<Value> {
        let request = self.call(
            "Task.Prepare",
            json!({"tool": tool, "input": input, "key": key, "delegation_id": delegation}),
        )?;
        match self.call("Task.Submit", request.clone()) {
            Err(e) if e.code == "DependencyUnavailable" => {
                let state = self
                    .call("Task.LookupSubmission", request.clone())
                    .map_err(|_| {
                        ApiError::new(
                            "OutcomeUnknown",
                            "submission lookup indeterminate; retain original key",
                        )
                    })?;
                match state["status"].as_str() {
                    Some("Found") => {
                        let mut receipt = state["accepted"].clone();
                        receipt["deduplicated"] = json!(true);
                        Ok(receipt)
                    }
                    Some("AbsentWithinWindow") => self.call("Task.Submit", request),
                    Some("Expired") => Err(ApiError::new(
                        "IdempotencyKeyExpired",
                        "submission namespace expired",
                    )
                    .into()),
                    _ => Err(
                        ApiError::new("OutcomeUnknown", "submission lookup indeterminate").into(),
                    ),
                }
            }
            result => result,
        }
    }
    pub fn wait(&self, task: &str, timeout: Duration) -> ApiResult<Value> {
        let start = std::time::Instant::now();
        loop {
            let v = self.call("Task.Get", json!({"id": task}))?;
            match v["lifecycle"].as_str() {
                Some("Succeeded" | "Failed" | "Cancelled" | "Paused" | "WaitingApproval") => {
                    return Ok(v);
                }
                _ if start.elapsed() >= timeout => {
                    return Err(ApiError::new("DeadlineExceeded", "wait deadline reached").into());
                }
                _ => std::thread::sleep(Duration::from_millis(60)),
            }
        }
    }
}
/// Providers select a registered tool and structured input. psiosd owns execution and policy.
pub trait AgentProvider {
    fn propose(&self, status: &Value, capabilities: &Value) -> ApiResult<(String, Value)>;
}
pub struct StatusProvider;
impl AgentProvider for StatusProvider {
    fn propose(&self, _: &Value, caps: &Value) -> ApiResult<(String, Value)> {
        if !caps
            .as_array()
            .is_some_and(|a| a.iter().any(|c| c["capability_id"] == "system.status"))
        {
            return Err(ApiError::new("PermissionDenied", "system.status unavailable").into());
        }
        Ok(("system.status".into(), json!({})))
    }
}
impl Client {
    pub fn execute_agent(
        &self,
        provider: &impl AgentProvider,
        key: &str,
        delegation: &str,
    ) -> ApiResult<Value> {
        let status = self.call(
            "Tool.Call",
            json!({"tool": "system.status", "input": {}, "delegation_id": delegation}),
        )?;
        let caps = self.call("Capability.Discover", json!({}))?;
        let (tool, input) = provider.propose(&status, &caps)?;
        let accepted = self.submit(&tool, input, key, delegation)?;
        let task = self.wait(
            accepted["task_id"]
                .as_str()
                .ok_or_else(|| invalid("task ID"))?,
            Duration::from_secs(30),
        )?;
        if task["lifecycle"] != "Succeeded" {
            return Ok(json!({"task": task}));
        }
        let artifact = self.call(
            "Artifact.Read",
            json!({"reference": task["result"]["artifact_refs"][0]}),
        )?;
        Ok(json!({"task": task, "result": artifact}))
    }
}
