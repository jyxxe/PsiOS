//! MCP STDIO adapter for explicitly delegated local diagnostics.
#[path = "psios_mcp/modern.rs"]
mod modern;

use clap::{Parser, ValueEnum};
use psios_api::{
    ApiError, ApiResult, Client, LocalReadBinding, LocalReadTool, LocalReadToolsRequest,
    MAX_DEVICE_INVENTORY_PAGE, MAX_PCI_INVENTORY_ROWS, MAX_USB_INVENTORY_ROWS,
    MAX_VOLUME_FILE_CHUNK, MAX_VOLUME_FILE_OFFSET, MAX_VOLUME_FILES_PAGE,
    ResolveLocalReadToolsRequest, ResolvedLocalReadTool, ResourceRef, VolumeFileChunkQuery,
    VolumeFilesQuery, id, parse,
};
use rustix::{fs::OFlags, process::geteuid};
use serde_json::{Map, Value, json};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, BufRead, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const PROTOCOL: &str = "2025-11-25";
const MAX_LINE: usize = 64 * 1024;
const MAX_RESULT: usize = 16 * 1024;
const MAX_REQUESTS: usize = 1024;
const CALLS_PER_SECOND: u8 = 8;
const TASK_DEADLINE_SECS: i64 = 30;

#[derive(Clone, Copy, ValueEnum)]
enum Protocol {
    #[value(name = "2025-11-25")]
    Legacy,
    #[value(name = "2026-07-28")]
    Modern,
}

#[derive(Parser)]
#[command(about = "Serve explicitly delegated local PsiOS tools on MCP STDIO")]
struct Options {
    #[arg(long, value_enum, default_value = "2025-11-25")]
    protocol: Protocol,
    #[arg(long)]
    socket: PathBuf,
    #[arg(long)]
    service_uid: Option<u32>,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    delegation: String,
    /// Include an unbound diagnostic. Defaults to system.status when no tools are selected.
    #[arg(long = "tool", value_parser = parse_tool)]
    tools: Vec<LocalReadTool>,
    /// Include root usage for one exact Volume ID.
    #[arg(long, value_name = "VOLUME_ID", value_parser = parse_target)]
    volume_usage: Option<String>,
    /// Include paginated directory metadata for one exact Volume ID.
    #[arg(long, value_name = "VOLUME_ID", value_parser = parse_target)]
    volume_files: Option<String>,
    /// Include bounded binary file reads for one exact Volume ID.
    #[arg(long, value_name = "VOLUME_ID", value_parser = parse_target)]
    volume_read_chunk: Option<String>,
    /// Include observations for one exact registered HostService ID.
    #[arg(long, value_name = "HOST_SERVICE_ID", value_parser = parse_target)]
    service_inspect: Option<String>,
    /// Include live observations for one exact managed Application ID.
    #[arg(long, value_name = "APPLICATION_ID", value_parser = parse_target)]
    application_inspect: Option<String>,
    /// Enable modern durable Tasks for an exact local endpoint identity.
    #[arg(long, value_name = "ENDPOINT_ID", value_parser = parse_endpoint)]
    tasks_endpoint: Option<String>,
    /// Lifetime of new task handles in seconds.
    #[arg(long, requires = "tasks_endpoint", value_parser = clap::value_parser!(u32).range(1..=600))]
    tasks_ttl_seconds: Option<u32>,
}

fn parse_tool(name: &str) -> Result<LocalReadTool, &'static str> {
    serde_json::from_value(json!(name)).map_err(
        |_| "expected system.status, node.pressure, node.inventory, node.devices, node.resolver, node.pci or node.usb",
    )
}

fn parse_target(id: &str) -> Result<String, &'static str> {
    if id.is_empty() || id.len() > 200 || id.chars().any(char::is_control) {
        return Err("invalid resource ID");
    }
    Ok(id.to_owned())
}

fn parse_endpoint(id: &str) -> Result<String, &'static str> {
    if (1..=96).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Ok(id.to_owned())
    } else {
        Err("invalid MCP Tasks endpoint identity")
    }
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
    delegation_id: String,
    bindings: Vec<LocalReadBinding>,
    tasks: Option<TaskProfile>,
    phase: Phase,
    requests: HashSet<String>,
    call_window: Instant,
    calls: u8,
}

struct TaskProfile {
    endpoint: String,
    ttl_seconds: u32,
}

struct AvailableTool<'a> {
    binding: &'a LocalReadBinding,
    target_ref: Option<ResourceRef>,
}

fn invalid_data() -> Box<ApiError> {
    ApiError::new("PreconditionFailed", "invalid local authority response").into()
}

impl Adapter {
    fn available(&self) -> ApiResult<Vec<AvailableTool<'_>>> {
        let unbound = self
            .bindings
            .iter()
            .map(LocalReadBinding::unbound_tool)
            .collect::<Option<Vec<_>>>();
        let authorized: Vec<_> = if let Some(tools) = unbound {
            let params = serde_json::to_value(LocalReadToolsRequest {
                delegation_id: self.delegation_id.clone(),
                tools,
            })
            .map_err(|_| invalid_data())?;
            let reply = self.client.call("Capability.LocalReadTools", params)?;
            let tools: Vec<LocalReadTool> =
                serde_json::from_value(reply["tools"].clone()).map_err(|_| invalid_data())?;
            tools
                .into_iter()
                .map(|tool| (tool.name().to_owned(), None))
                .collect()
        } else {
            let params = serde_json::to_value(ResolveLocalReadToolsRequest {
                delegation_id: self.delegation_id.clone(),
                bindings: self.bindings.clone(),
            })
            .map_err(|_| invalid_data())?;
            let reply = self
                .client
                .call("Capability.ResolveLocalReadTools", params)?;
            let tools: Vec<ResolvedLocalReadTool> =
                serde_json::from_value(reply["tools"].clone()).map_err(|_| invalid_data())?;
            tools
                .into_iter()
                .map(|tool| (tool.tool, Some(tool.target_ref)))
                .collect()
        };
        let mut seen = HashSet::new();
        authorized
            .into_iter()
            .map(|(name, target_ref)| {
                let binding = self
                    .bindings
                    .iter()
                    .find(|binding| binding.name() == name)
                    .filter(|binding| seen.insert(binding.name()))
                    .ok_or_else(invalid_data)?;
                if let Some((id, _)) = binding.target()
                    && !target_ref
                        .as_ref()
                        .is_some_and(|reference| reference.resource_id == id)
                {
                    return Err(invalid_data());
                }
                Ok(AvailableTool {
                    binding,
                    target_ref,
                })
            })
            .collect()
    }

    fn invoke(
        &mut self,
        binding: &LocalReadBinding,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Value, Value> {
        let name = binding.name();
        let abandoned = || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire));
        let selected = self
            .available()
            .map_err(|error| tool_error(&error.code, None))?
            .into_iter()
            .find(|tool| tool.binding.name() == name)
            .ok_or_else(|| tool_error("PermissionDenied", None))?;
        let input = binding
            .task_input(selected.target_ref.as_ref())
            .ok_or_else(|| tool_error("PreconditionFailed", None))?;
        if abandoned() {
            return Err(tool_error("Cancelled", None));
        }
        // Idempotency is owned by the local submission ledger. Never reuse an MCP
        // request ID as a key across distinct connections or principals.
        let deadline = (chrono::Utc::now() + chrono::Duration::seconds(TASK_DEADLINE_SECS))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let prepared = self
            .client
            .call(
                "Task.Prepare",
                json!({"tool":name,"input":input,
            "key":id("mcp-call"),"delegation_id":self.delegation_id,"deadline_at":deadline}),
            )
            .map_err(|error| tool_error(&error.code, None))?;
        let accepted = self
            .client
            .submit_prepared_while(prepared, || !abandoned())
            .map_err(|error| tool_error(&error.code, None))?;
        let task_id = accepted["task_id"]
            .as_str()
            .ok_or_else(|| tool_error("PreconditionFailed", None))?;
        let failed = |code: &str| tool_error(code, Some(task_id));
        let task = match cancelled {
            Some(flag) => modern::wait_task(&self.client, task_id, flag),
            None => self.client.wait(task_id, Duration::from_secs(10)),
        }
        .map_err(|error| failed(&error.code))?;
        if abandoned() {
            return Err(failed("Cancelled"));
        }
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
        let live = self
            .available()
            .map_err(|error| failed(&error.code))?
            .into_iter()
            .find(|tool| tool.binding.name() == name)
            .ok_or_else(|| failed("PermissionDenied"))?;
        if selected.binding.target().is_some() && live.target_ref != selected.target_ref {
            return Err(failed("PreconditionFailed"));
        }
        tool_result(task_id, result)
    }

    fn tool_call(
        &mut self,
        params: &Map<String, Value>,
        cancelled: Option<&AtomicBool>,
        invocation: Option<&str>,
    ) -> Result<Value, (i32, &'static str)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "Invalid tool name"))?;
        let configured = self
            .bindings
            .iter()
            .find(|tool| tool.name() == name)
            .ok_or((-32602, "Unknown tool"))?;
        let binding = match params.get("arguments") {
            Some(args) if !args.is_object() => {
                return Err((-32602, "Tool arguments must be an object"));
            }
            Some(Value::Object(args)) if !args.is_empty() => match configured {
                LocalReadBinding::NodeDevices(_) => {
                    let Ok(query) = serde_json::from_value(Value::Object(args.clone())) else {
                        return Ok(tool_error("InvalidArgument", None));
                    };
                    LocalReadBinding::NodeDevices(query)
                }
                LocalReadBinding::NodePci(_) | LocalReadBinding::NodeUsb(_) => {
                    let Ok(query) = serde_json::from_value(Value::Object(args.clone())) else {
                        return Ok(tool_error("InvalidArgument", None));
                    };
                    if matches!(configured, LocalReadBinding::NodePci(_)) {
                        LocalReadBinding::NodePci(query)
                    } else {
                        LocalReadBinding::NodeUsb(query)
                    }
                }
                LocalReadBinding::VolumeFiles { target_id, .. } => {
                    let Ok(query) =
                        serde_json::from_value::<VolumeFilesQuery>(Value::Object(args.clone()))
                    else {
                        return Ok(tool_error("InvalidArgument", None));
                    };
                    if !query.within_limits() {
                        return Ok(tool_error("InvalidArgument", None));
                    }
                    LocalReadBinding::VolumeFiles {
                        target_id: target_id.clone(),
                        query,
                    }
                }
                LocalReadBinding::VolumeReadChunk { target_id, .. } => {
                    let Ok(query) =
                        serde_json::from_value::<VolumeFileChunkQuery>(Value::Object(args.clone()))
                    else {
                        return Ok(tool_error("InvalidArgument", None));
                    };
                    if !query.within_limits() {
                        return Ok(tool_error("InvalidArgument", None));
                    }
                    LocalReadBinding::VolumeReadChunk {
                        target_id: target_id.clone(),
                        query: Some(query),
                    }
                }
                LocalReadBinding::ApplicationInspect { target_id, .. } => {
                    let Some(include_usage) = args
                        .get("include_usage")
                        .and_then(Value::as_bool)
                        .filter(|_| args.len() == 1)
                    else {
                        return Ok(tool_error("InvalidArgument", None));
                    };
                    LocalReadBinding::ApplicationInspect {
                        target_id: target_id.clone(),
                        include_usage,
                    }
                }
                _ => return Ok(tool_error("InvalidArgument", None)),
            },
            _ if matches!(configured, LocalReadBinding::VolumeReadChunk { .. }) => {
                return Ok(tool_error("InvalidArgument", None));
            }
            _ => configured.clone(),
        };
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Ok(tool_error("Cancelled", None));
        }
        if self.call_window.elapsed() >= Duration::from_secs(1) {
            self.call_window = Instant::now();
            self.calls = 0;
        }
        if self.calls == CALLS_PER_SECOND {
            return Ok(tool_error("RateLimited", None));
        }
        self.calls += 1;
        if let Some(key) = invocation {
            let profile = self
                .tasks
                .as_ref()
                .ok_or((-32601, "MCP Tasks is disabled"))?;
            let result = self.client.call(
                "McpTask.Start",
                json!({"endpoint":profile.endpoint,
                "delegation_id":self.delegation_id,"binding":binding,"invocation_key":key,
                "ttl_seconds":profile.ttl_seconds}),
            );
            return Ok(result
                .map(|mut task| {
                    task["resultType"] = json!("task");
                    task
                })
                .unwrap_or_else(|error| {
                    let code = if error.code == "DependencyUnavailable" {
                        "OutcomeUnknown"
                    } else {
                        &error.code
                    };
                    tool_error(code, None)
                }));
        }
        Ok(self
            .invoke(&binding, cancelled)
            .unwrap_or_else(|failure| failure))
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
        self.available()
            .map_err(|_| (-32001, "Cannot authenticate with local PsiOS"))?;
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
                let tools: Vec<_> = tools
                    .into_iter()
                    .map(|tool| descriptor(tool.binding, Protocol::Legacy))
                    .collect();
                Ok(json!({"tools":tools}))
            }
            "tools/call" if fields(&["name", "arguments"]) => self.tool_call(params, None, None),
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

fn descriptor(tool: &LocalReadBinding, protocol: Protocol) -> Value {
    let description = match tool {
        LocalReadBinding::SystemStatus {} => {
            "Read current PsiOS system status from the local node."
        }
        LocalReadBinding::NodePressure {} => {
            "Sample Linux kernel and Core cgroup resource pressure on the local node."
        }
        LocalReadBinding::NodeInventory {} => {
            "Observe verified Linux kernel, CPU, memory and load data on the local node."
        }
        LocalReadBinding::NodeDevices(_) => {
            "Read verified local Linux block devices. Provide a limit for a bounded page and use the returned cursor to continue."
        }
        LocalReadBinding::NodeResolver {} => {
            "Observe the nameservers configured in the Core's mount namespace."
        }
        LocalReadBinding::NodePci(_) => {
            "Read a bounded page of verified PCI devices and their bound Linux drivers on the local node."
        }
        LocalReadBinding::NodeUsb(_) => {
            "Read a bounded page of verified USB devices, ports and root hubs on the local node."
        }
        LocalReadBinding::VolumeUsage { .. } => {
            "Measure file and allocation usage of the configured volume root."
        }
        LocalReadBinding::VolumeFiles { .. } => {
            "List a directory in the configured volume. Continue with next_after, volume_version and directory_revision from the previous page."
        }
        LocalReadBinding::VolumeReadChunk { .. } => {
            "Read a bounded Base64 chunk in the configured volume. Continue with next_offset, volume_version and file_revision."
        }
        LocalReadBinding::ServiceInspect { .. } => "Observe the configured systemd service state.",
        LocalReadBinding::ApplicationInspect { .. } => {
            "Observe the configured OCI application. Request include_usage for live cgroup v2 accounting."
        }
    };
    let mut descriptor = json!({"name":tool.name(),"description":description,
        "inputSchema":{"type":"object","additionalProperties":false},
        "annotations":{"readOnlyHint":true}});
    let inventory_offset_maximum = match tool {
        LocalReadBinding::NodeDevices(_) => Some(1023),
        LocalReadBinding::NodePci(_) => Some(MAX_PCI_INVENTORY_ROWS - 1),
        LocalReadBinding::NodeUsb(_) => Some(MAX_USB_INVENTORY_ROWS - 1),
        _ => None,
    };
    if let Some(maximum) = inventory_offset_maximum {
        descriptor["inputSchema"]["properties"] = json!({
            "limit":{"type":["integer","null"],"minimum":1,"maximum":MAX_DEVICE_INVENTORY_PAGE},
            "cursor":{"type":["object","null"],"additionalProperties":false,
                "required":["snapshot_digest","offset"],"properties":{
                    "snapshot_digest":{"type":"string","pattern":"^sha256:[0-9a-f]{64}$"},
                    "offset":{"type":"integer","minimum":1,"maximum":maximum}
                }
            }
        });
    }
    if matches!(tool, LocalReadBinding::VolumeFiles { .. }) {
        descriptor["inputSchema"]["properties"] = json!({
            "path":{"type":"string","default":"","maxLength":2048},
            "limit":{"type":["integer","null"],"minimum":1,"maximum":MAX_VOLUME_FILES_PAGE},
            "after":{"type":["string","null"],"minLength":1,"maxLength":255},
            "directory_revision":{"type":["string","null"],"pattern":"^sha256:[0-9a-f]{64}$"},
            "volume_version":{"type":["string","null"],"minLength":1,"maxLength":128}
        });
    }
    if matches!(tool, LocalReadBinding::VolumeReadChunk { .. }) {
        descriptor["inputSchema"]["required"] = json!(["path"]);
        descriptor["inputSchema"]["properties"] = json!({
            "path":{"type":"string","minLength":1,"maxLength":2048},
            "offset":{"type":"integer","default":0,"minimum":0,"maximum":MAX_VOLUME_FILE_OFFSET},
            "limit":{"type":["integer","null"],"minimum":1,"maximum":MAX_VOLUME_FILE_CHUNK},
            "volume_version":{"type":["string","null"],"minLength":1,"maxLength":128},
            "file_revision":{"type":["string","null"],"pattern":"^sha256:[0-9a-f]{64}$"}
        });
    }
    if matches!(tool, LocalReadBinding::ApplicationInspect { .. }) {
        descriptor["inputSchema"]["properties"] = json!({
            "include_usage":{"type":"boolean","default":false}
        });
    }
    if matches!(protocol, Protocol::Legacy) {
        descriptor["execution"] = json!({"taskSupport":"forbidden"});
    }
    descriptor
}

fn tool_result(task_id: &str, result: Value) -> Result<Value, Value> {
    let structured = json!({"task_id":task_id,"result":result});
    let text = structured.to_string();
    if text.len() > MAX_RESULT {
        return Err(tool_error("QuotaExceeded", Some(task_id)));
    }
    Ok(
        json!({"isError":false,"structuredContent":structured,"content":[{"type":"text","text":text}]}),
    )
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
    // Both MCP schemas omit the ID when a malformed request has no
    // correlatable string or integer ID. Null is not an MCP RequestId.
    if let Some(id) = id {
        frame["id"] = id.clone();
    }
    frame
}

fn write_frame(output: &mut impl Write, response: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, response)?;
    output.write_all(b"\n")?;
    output.flush()
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
            write_frame(&mut output, &response)?;
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
    let mut bindings: Vec<_> = options
        .tools
        .into_iter()
        .map(LocalReadBinding::from)
        .collect();
    if let Some(target_id) = options.volume_usage {
        bindings.push(LocalReadBinding::VolumeUsage { target_id });
    }
    if let Some(target_id) = options.volume_files {
        bindings.push(LocalReadBinding::VolumeFiles {
            target_id,
            query: VolumeFilesQuery::default(),
        });
    }
    if let Some(target_id) = options.volume_read_chunk {
        bindings.push(LocalReadBinding::VolumeReadChunk {
            target_id,
            query: None,
        });
    }
    if let Some(target_id) = options.service_inspect {
        bindings.push(LocalReadBinding::ServiceInspect { target_id });
    }
    if let Some(target_id) = options.application_inspect {
        bindings.push(LocalReadBinding::ApplicationInspect {
            target_id,
            include_usage: false,
        });
    }
    if bindings.is_empty() {
        bindings.push(LocalReadBinding::SystemStatus {});
    }
    if options.tasks_endpoint.is_some() && matches!(options.protocol, Protocol::Legacy) {
        return Err(io::Error::other("MCP Tasks requires --protocol 2026-07-28"));
    }
    let client = Client::new(options.socket, credential(&options.token_file)?)
        .with_service_uid(options.service_uid.unwrap_or_else(|| geteuid().as_raw()))
        .with_fail_fast_connect();
    let adapter = Adapter {
        client,
        delegation_id: options.delegation,
        bindings,
        tasks: options.tasks_endpoint.map(|endpoint| TaskProfile {
            endpoint,
            ttl_seconds: options.tasks_ttl_seconds.unwrap_or(300),
        }),
        phase: Phase::New,
        requests: HashSet::new(),
        call_window: Instant::now(),
        calls: 0,
    };
    match options.protocol {
        Protocol::Legacy => serve(adapter, io::stdin().lock(), io::stdout().lock()),
        Protocol::Modern => modern::serve(adapter, io::stdout()),
    }
}
