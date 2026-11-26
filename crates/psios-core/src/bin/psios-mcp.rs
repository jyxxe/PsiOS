//! MCP 2025-11-25 STDIO adapter for explicitly delegated local diagnostics.
use clap::Parser;
use psios_api::{ApiError, ApiResult, Client, LocalReadTool, LocalReadToolsRequest, id, parse};
use rustix::{fs::OFlags, process::geteuid};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, BufRead, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const PROTOCOL: &str = "2025-11-25";
const MAX_LINE: usize = 64 * 1024;
const MAX_RESULT: usize = 16 * 1024;
const MAX_REQUESTS: usize = 1024;
const CALLS_PER_SECOND: u8 = 8;
const TASK_DEADLINE_SECS: i64 = 30;

#[derive(Parser)]
#[command(about = "Serve explicitly delegated local PsiOS tools on MCP STDIO")]
struct Options {
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    service_uid: Option<u32>,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    delegation: String,
    #[arg(long = "tool", default_value = "system.status", value_parser = parse_tool)]
    tools: Vec<LocalReadTool>,
}

fn parse_tool(name: &str) -> Result<LocalReadTool, &'static str> {
    serde_json::from_value(json!(name)).map_err(|_| "expected system.status or node.pressure")
}

fn credential(path: &Path) -> io::Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "expected a private, caller-owned token file",
        ));
    }
    let mut token = String::new();
    file.take(129).read_to_string(&mut token)?;
    let trimmed = token.trim();
    if token.len() > 128 || trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(io::Error::other("invalid token file"));
    }
    Ok(trimmed.to_owned())
}

#[derive(PartialEq, Eq)]
enum Phase {
    New,
    Initializing,
    Ready,
}

struct Adapter {
    client: Client,
    scope: LocalReadToolsRequest,
    phase: Phase,
    requests: HashSet<String>,
    call_window: Instant,
    calls: u8,
}

fn invalid_data() -> Box<ApiError> {
    ApiError::new("PreconditionFailed", "invalid local authority response").into()
}

impl Adapter {
    fn available(&self) -> ApiResult<Vec<LocalReadTool>> {
        let params = serde_json::to_value(&self.scope).map_err(|_| invalid_data())?;
        let reply = self.client.call("Capability.LocalReadTools", params)?;
        serde_json::from_value(reply["tools"].clone()).map_err(|_| invalid_data())
    }

    fn invoke(&mut self, tool: LocalReadTool) -> Result<Value, Value> {
        if self.call_window.elapsed() >= Duration::from_secs(1) {
            self.call_window = Instant::now();
            self.calls = 0;
        }
        if self.calls == CALLS_PER_SECOND {
            return Err(tool_error("RateLimited", None));
        }
        self.calls += 1;
        let allowed = self
            .available()
            .map_err(|error| tool_error(&error.code, None))?;
        if !allowed.contains(&tool) {
            return Err(tool_error("PermissionDenied", None));
        }
        // Idempotency is owned by the local submission ledger. Never reuse an MCP
        // request ID as a key across distinct connections or principals.
        let deadline = (chrono::Utc::now() + chrono::Duration::seconds(TASK_DEADLINE_SECS))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let prepared = self
            .client
            .call(
                "Task.Prepare",
                json!({"tool":tool.name(),"input":{},
            "key":id("mcp-call"),"delegation_id":self.scope.delegation_id,"deadline_at":deadline}),
            )
            .map_err(|error| tool_error(&error.code, None))?;
        let accepted = self
            .client
            .submit_prepared(prepared)
            .map_err(|error| tool_error(&error.code, None))?;
        let task_id = accepted["task_id"]
            .as_str()
            .ok_or_else(|| tool_error("PreconditionFailed", None))?;
        let failed = |code: &str| tool_error(code, Some(task_id));
        let task = self
            .client
            .wait(task_id, Duration::from_secs(10))
            .map_err(|error| failed(&error.code))?;
        if task["outcome_knowledge"] == "Unknown" {
            return Err(failed("OutcomeUnknown"));
        }
        if task["lifecycle"] != "Succeeded" || task["outcome_knowledge"] != "Known" {
            let code = task["conditions"][0]["type"]
                .as_str()
                .unwrap_or_else(|| task["lifecycle"].as_str().unwrap_or("ResultUnavailable"));
            return Err(failed(code));
        }
        let Some([reference]) = task["result"]["artifact_refs"]
            .as_array()
            .map(Vec::as_slice)
        else {
            return Err(failed("ResultUnavailable"));
        };
        let result = self
            .client
            .call("Artifact.Read", json!({"reference":reference}))
            .map_err(|error| failed(&error.code))?;
        // Do not return an old tool scope through this adapter just because a
        // separate Task history grant still permits reading the artifact.
        let live = self.available().map_err(|error| failed(&error.code))?;
        if !live.contains(&tool) {
            return Err(failed("PermissionDenied"));
        }
        let structured = json!({"task_id":task_id,"result":result});
        let text = structured.to_string();
        if text.len() > MAX_RESULT {
            return Err(failed("QuotaExceeded"));
        }
        Ok(
            json!({"isError":false,"structuredContent":structured,"content":[{"type":"text","text":text}]}),
        )
    }

    fn initialize(&mut self, params: &Map<String, Value>) -> Result<Value, (i32, &'static str)> {
        if self.phase != Phase::New {
            return Err((-32600, "Connection already initialized"));
        }
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s.len() <= 256)
        };
        let info = params.get("clientInfo");
        if !text(params.get("protocolVersion"))
            || !params.get("capabilities").is_some_and(Value::is_object)
            || !text(info.and_then(|v| v.get("name")))
            || !text(info.and_then(|v| v.get("version")))
        {
            return Err((-32602, "Invalid initialize parameters"));
        }
        let principal = self
            .client
            .call("Principal.WhoAmI", json!({}))
            .map_err(|_| (-32001, "Cannot authenticate with local PsiOS"))?;
        if !principal["principal_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("principal-"))
        {
            return Err((-32001, "A dedicated local Agent credential is required"));
        }
        self.phase = Phase::Initializing;
        Ok(
            json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"psios-mcp","version":env!("CARGO_PKG_VERSION")}}),
        )
    }

    fn request(
        &mut self,
        method: &str,
        params: &Map<String, Value>,
    ) -> Result<Value, (i32, &'static str)> {
        let fields = |allowed: &[&str]| {
            params
                .keys()
                .all(|key| key == "_meta" || allowed.contains(&key.as_str()))
        };
        if params.get("_meta").is_some_and(|value| !value.is_object()) {
            return Err((-32602, "Invalid protocol metadata"));
        }
        if method == "initialize" {
            return self.initialize(params);
        }
        if method == "ping" && fields(&[]) {
            return Ok(json!({}));
        }
        if self.phase != Phase::Ready {
            return Err((-32002, "Connection is not initialized"));
        }
        match method {
            "tools/list" if fields(&[]) => {
                let tools = self
                    .available()
                    .map_err(|_| (-32001, "Local capability query unavailable"))?;
                let tools: Vec<_> = tools.into_iter().map(descriptor).collect();
                Ok(json!({"tools":tools}))
            }
            "tools/call" if fields(&["name", "arguments"]) => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or((-32602, "Invalid tool name"))?;
                let tool = self
                    .scope
                    .tools
                    .iter()
                    .copied()
                    .find(|tool| tool.name() == name)
                    .ok_or((-32602, "Unknown tool"))?;
                if params
                    .get("arguments")
                    .is_some_and(|arguments| !arguments.is_object())
                {
                    return Err((-32602, "Tool arguments must be an object"));
                }
                if params
                    .get("arguments")
                    .and_then(Value::as_object)
                    .is_some_and(|arguments| !arguments.is_empty())
                {
                    return Ok(tool_error("InvalidArgument", None));
                }
                Ok(self.invoke(tool).unwrap_or_else(|failure| failure))
            }
            "tools/list" | "tools/call" | "ping" => {
                Err((-32602, "Invalid or unsupported parameters"))
            }
            _ => Err((-32601, "Method not supported")),
        }
    }

    fn handle(&mut self, frame: Value) -> Option<Value> {
        let request_id = frame
            .get("id")
            .filter(|id| id.as_str().is_some_and(|s| s.len() <= 128) || id.is_i64() || id.is_u64());
        let method = frame["method"].as_str();
        if frame["jsonrpc"] != "2.0"
            || method.is_none()
            || (frame.get("id").is_some() && request_id.is_none())
        {
            return Some(rpc_error(request_id, -32600, "Invalid request"));
        }
        let method = method?;
        let empty = Map::new();
        let params = match frame.get("params") {
            Some(value) => value.as_object(),
            None => Some(&empty),
        };
        if let Some(request_id) = request_id {
            let result = if !self.requests.insert(request_id.to_string()) {
                Err((-32600, "Request ID already used"))
            } else if let Some(params) = params {
                self.request(method, params)
            } else {
                Err((-32602, "Parameters must be an object"))
            };
            return Some(match result {
                Ok(result) => json!({"jsonrpc":"2.0","id":request_id,"result":result}),
                Err((code, text)) => rpc_error(Some(request_id), code, text),
            });
        }
        if method == "notifications/initialized"
            && params.is_some()
            && self.phase == Phase::Initializing
        {
            self.phase = Phase::Ready;
        }
        None
    }
}

fn descriptor(tool: LocalReadTool) -> Value {
    let description = match tool {
        LocalReadTool::SystemStatus => "Read current PsiOS system status from the local node.",
        LocalReadTool::NodePressure => {
            "Sample Linux kernel and Core cgroup resource pressure on the local node."
        }
    };
    json!({"name":tool.name(),"description":description,
        "inputSchema":{"type":"object","additionalProperties":false},
        "annotations":{"readOnlyHint":true},"execution":{"taskSupport":"forbidden"}})
}

fn tool_error(code: &str, task: Option<&str>) -> Value {
    let mut content = json!({"code":code});
    if let Some(task) = task {
        content["task_id"] = json!(task);
    }
    json!({"isError":true,"structuredContent":content,
        "content":[{"type":"text","text":format!("PsiOS tool did not complete: {code}")}]})
}

fn rpc_error(id: Option<&Value>, code: i32, message: &str) -> Value {
    let mut frame = json!({"jsonrpc":"2.0","error":{"code":code,"message":message}});
    // The 2025-11-25 schema omits the ID when a malformed request has no
    // correlatable string or integer ID. Null is not an MCP RequestId.
    if let Some(id) = id {
        frame["id"] = id.clone();
    }
    frame
}

fn serve(mut adapter: Adapter, mut input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if Read::take(&mut input, (MAX_LINE + 1) as u64).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        let oversized = line.len() > MAX_LINE || line.last() != Some(&b'\n');
        let response = if oversized {
            Some(rpc_error(None, -32700, "Invalid or oversized frame"))
        } else {
            match parse(&line) {
                Ok(frame) => adapter.handle(frame),
                Err(_) => Some(rpc_error(None, -32700, "Invalid JSON frame")),
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        if oversized || adapter.requests.len() == MAX_REQUESTS {
            return Ok(());
        }
    }
}

fn main() -> io::Result<()> {
    let options = Options::parse();
    if options.tools.len() > 2 || options.tools.len() == 2 && options.tools[0] == options.tools[1] {
        return Err(io::Error::other("select one or two distinct tools"));
    }
    let client = Client::new(options.socket, credential(&options.token_file)?)
        .with_service_uid(options.service_uid.unwrap_or_else(|| geteuid().as_raw()));
    serve(
        Adapter {
            client,
            scope: LocalReadToolsRequest {
                delegation_id: options.delegation,
                tools: options.tools,
            },
            phase: Phase::New,
            requests: HashSet::new(),
            call_window: Instant::now(),
            calls: 0,
        },
        io::stdin().lock(),
        io::stdout().lock(),
    )
}
