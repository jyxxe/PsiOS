//! MCP 2026-07-28 per-request protocol and bounded STDIO work admission.
use super::{Adapter, MAX_LINE, Protocol, descriptor, rpc_error};
use psios_api::{ApiError, ApiResult, Client, parse, task_wait_ready};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::AsFd,
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::{Errno, write},
};
use serde_json::{Map, Value, json};
use std::{
    io::{self, BufRead, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const VERSION: &str = "2026-07-28";
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
const CLIENT_INFO_KEY: &str = "io.modelcontextprotocol/clientInfo";
const SERVER_BUSY: i32 = -32009;

enum Request {
    Discover,
    List,
    Call(Map<String, Value>),
}

fn request(method: &str, parameters: Option<&Value>, id: &Value) -> Result<Request, Value> {
    let invalid = |message| rpc_error(Some(id), -32602, message);
    if method == "initialize" {
        return Err(rpc_error(
            Some(id),
            -32601,
            "Select MCP 2026-07-28 with per-request metadata",
        ));
    }
    let params = parameters
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("Per-request metadata required"))?;
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("Per-request metadata required"))?;
    let version = meta
        .get(VERSION_KEY)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(|| invalid("Invalid or missing protocol version"))?;
    if !meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object) {
        return Err(invalid("Invalid or missing client capabilities"));
    }
    if meta.get(CLIENT_INFO_KEY).is_some_and(|info| {
        !["name", "version"].into_iter().all(|key| {
            info[key]
                .as_str()
                .is_some_and(|value| !value.is_empty() && value.len() <= 256)
        })
    }) {
        return Err(invalid("Invalid client information"));
    }
    if version != VERSION {
        let mut error = rpc_error(Some(id), -32022, "Unsupported protocol version");
        error["error"]["data"] = json!({"supported":[VERSION],"requested":version});
        return Err(error);
    }

    let fields = |allowed: &[&str]| {
        params
            .keys()
            .all(|key| key == "_meta" || allowed.contains(&key.as_str()))
    };
    match method {
        "server/discover" if fields(&[]) => Ok(Request::Discover),
        "tools/list" if fields(&[]) => Ok(Request::List),
        "tools/call" if fields(&["name", "arguments"]) => Ok(Request::Call(params.clone())),
        "server/discover" | "tools/list" | "tools/call" => {
            Err(invalid("Invalid or unsupported parameters"))
        }
        _ => Err(rpc_error(Some(id), -32601, "Method not supported")),
    }
}

fn complete(result: &mut Value, cacheable: bool) {
    result["resultType"] = json!("complete");
    result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": {
        "name":"psios-mcp", "version":env!("CARGO_PKG_VERSION"),
    }});
    if cacheable {
        result["ttlMs"] = json!(0);
        result["cacheScope"] = json!("private");
    }
}

fn dispatch(adapter: &mut Adapter, request: Request, id: &Value, cancelled: &AtomicBool) -> Value {
    let cacheable = !matches!(request, Request::Call(_));
    let result = match request {
        Request::Discover => adapter
            .available()
            .map(|_| {
                json!({
                    "supportedVersions":[VERSION], "capabilities":{"tools":{"listChanged":false}},
                })
            })
            .map_err(|_| (-32603, "Local capability query unavailable")),
        Request::List => adapter
            .available()
            .map(|tools| {
                let tools: Vec<_> = tools
                    .into_iter()
                    .map(|tool| descriptor(tool, Protocol::Modern))
                    .collect();
                json!({"tools":tools})
            })
            .map_err(|_| (-32603, "Local capability query unavailable")),
        Request::Call(params) => adapter.tool_call(&params, Some(cancelled)),
    };
    match result {
        Ok(mut result) => {
            complete(&mut result, cacheable);
            json!({"jsonrpc":"2.0","id":id,"result":result})
        }
        Err((code, message)) => rpc_error(Some(id), code, message),
    }
}

#[derive(Clone)]
struct Active {
    id: Value,
    cancelled: Arc<AtomicBool>,
}

struct Job {
    request: Request,
    active: Active,
}

enum TransportEvent {
    Frame(Vec<u8>),
    EndOfInput,
    Failed(io::Error),
}

fn read_frames(events: mpsc::SyncSender<TransportEvent>) {
    let mut input = io::stdin().lock();
    loop {
        let mut line = Vec::new();
        let result = Read::take(&mut input, (MAX_LINE + 1) as u64).read_until(b'\n', &mut line);
        let event = match result {
            Ok(0) => TransportEvent::EndOfInput,
            Ok(_) => TransportEvent::Frame(line),
            Err(error) => TransportEvent::Failed(error),
        };
        let terminal = !matches!(event, TransportEvent::Frame(_));
        if events.send(event).is_err() || terminal {
            return;
        }
    }
}

struct Transport<W> {
    output: W,
    active: Option<Active>,
    closed: bool,
}

fn valid_id(value: &Value) -> bool {
    value.as_str().is_some_and(|s| s.len() <= 128) || value.is_i64() || value.is_u64()
}

fn write_frame(output: impl AsFd, response: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LINE {
        return Err(io::Error::other("MCP response exceeds frame limit"));
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "MCP output timed out",
            ));
        }
        match write(output.as_fd(), remaining) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "MCP output closed",
                ));
            }
            Ok(count) => remaining = &remaining[count..],
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) => {
                let timeout = Timespec::try_from(left).expect("MCP output deadline fits timespec");
                match poll(&mut [PollFd::new(&output, PollFlags::OUT)], Some(&timeout)) {
                    Ok(_) | Err(Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

impl<W: AsFd> Transport<W> {
    fn receive(&mut self, frame: Value, work: &mpsc::SyncSender<Job>) -> io::Result<()> {
        let id = frame.get("id").filter(|id| valid_id(id));
        if id.is_some_and(|id| self.active.as_ref().is_some_and(|active| &active.id == id)) {
            // An in-flight ID may not be reused. Keep the first request and its
            // cancellation scope, without creating a second reply for that ID.
            return Ok(());
        }
        let Some(method) = frame["method"]
            .as_str()
            .filter(|_| frame["jsonrpc"] == "2.0" && (frame.get("id").is_none() || id.is_some()))
        else {
            return write_frame(&self.output, &rpc_error(id, -32600, "Invalid request"));
        };
        let params = frame.get("params");
        let Some(id) = id else {
            if method == "notifications/cancelled"
                && let Some(active) = &self.active
                && params.is_some_and(|params| {
                    params["requestId"] == active.id
                        && params.get("reason").is_none_or(Value::is_string)
                })
            {
                active.cancelled.store(true, Ordering::Release);
            }
            return Ok(());
        };

        let request = match request(method, params, id) {
            Ok(request) => request,
            Err(error) => return write_frame(&self.output, &error),
        };
        if self.active.is_some() {
            return write_frame(
                &self.output,
                &rpc_error(Some(id), SERVER_BUSY, "Local request already in progress"),
            );
        }
        let active = Active {
            id: id.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.active = Some(active.clone());
        work.try_send(Job { request, active })
            .map_err(|_| io::Error::other("local MCP worker unavailable"))
    }
}

pub(super) fn serve(mut adapter: Adapter, output: impl AsFd + Send + 'static) -> io::Result<()> {
    fcntl_setfl(&output, fcntl_getfl(&output)? | OFlags::NONBLOCK)?;
    let transport = Arc::new(Mutex::new(Transport {
        output,
        active: None,
        closed: false,
    }));
    let (work, jobs) = mpsc::sync_channel::<Job>(1);
    let (transport_events, events) = mpsc::sync_channel(1);
    let input_events = transport_events.clone();
    let _input = thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || read_frames(input_events))?;
    let output = Arc::clone(&transport);
    let _worker = thread::Builder::new()
        .name("mcp-request".into())
        .spawn(move || {
            for job in jobs {
                let active = job.active;
                let response = (!active.cancelled.load(Ordering::Acquire))
                    .then(|| dispatch(&mut adapter, job.request, &active.id, &active.cancelled));
                let Ok(mut state) = output.lock() else {
                    return;
                };
                if state.closed {
                    return;
                }
                // Serialize this last check with reading a cancellation. Once the
                // frame is committed to the output stream it may already be visible.
                if let Some(response) = response
                    && !active.cancelled.load(Ordering::Acquire)
                    && let Err(error) = write_frame(&state.output, &response)
                {
                    state.closed = true;
                    // Wake the coordinator even if the peer has left stdin open.
                    drop(state);
                    let _ = transport_events.send(TransportEvent::Failed(error));
                    return;
                }
                state.active = None;
            }
        })?;

    for event in events {
        let mut state = transport
            .lock()
            .map_err(|_| io::Error::other("local MCP transport unavailable"))?;
        if state.closed {
            return match event {
                TransportEvent::Failed(error) => Err(error),
                _ => Err(io::Error::other("MCP output closed")),
            };
        }
        let line = match event {
            TransportEvent::Frame(line) => line,
            TransportEvent::Failed(error) => {
                state.closed = true;
                return Err(error);
            }
            TransportEvent::EndOfInput => {
                state.closed = true;
                return Ok(());
            }
        };
        if line.len() > MAX_LINE || line.last() != Some(&b'\n') {
            state.closed = true;
            return write_frame(
                &state.output,
                &rpc_error(None, -32700, "Invalid or oversized frame"),
            );
        }
        match parse(&line) {
            Ok(frame) => state.receive(frame, &work)?,
            Err(_) => write_frame(
                &state.output,
                &rpc_error(None, -32700, "Invalid JSON frame"),
            )?,
        }
    }
    Ok(())
}

fn cancel_task(client: &Client, id: &str) {
    // Only normal, authorized CAS on the exact admitted Task. A racing terminal
    // commit (or a withdrawn Grant) wins and keeps its evidence.
    if let Ok(task) = client.call("Task.Results", json!({"id":id}))
        && matches!(task["lifecycle"].as_str(), Some("Queued" | "Running"))
        && let Some(revision) = task["task_revision"].as_str()
    {
        let _ = client.call(
            "Task.Cancel",
            json!({"id":id,"expected_resource_version":revision}),
        );
    }
}

pub(super) fn wait_task(client: &Client, id: &str, cancelled: &AtomicBool) -> ApiResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if cancelled.load(Ordering::Acquire) {
            cancel_task(client, id);
            return Err(ApiError::new("Cancelled", "MCP request was cancelled").into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ApiError::new("DeadlineExceeded", "wait deadline reached").into());
        }
        let timeout_ms = left.as_millis().clamp(1, 250) as u64;
        let waited = client.call("Task.Wait", json!({"id":id,"timeout_ms":timeout_ms}));
        let task = match waited {
            Err(error) if error.code == "QueueFull" => {
                thread::sleep(left.min(Duration::from_millis(50)));
                client.call("Task.Get", json!({"id":id}))?
            }
            other => other?,
        };
        if task_wait_ready(&task) {
            return Ok(task);
        }
    }
}
