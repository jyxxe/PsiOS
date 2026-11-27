//! A browser-facing local client. The Core checks each operation under the paired identity.

mod session;

use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    header::{self, HeaderMap, HeaderValue},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use psios_api::{ApiError, ApiResult, Client, ResourceRef, parse};
use serde::Deserialize;
use serde_json::{Value, json};
use session::{Session, now};
use std::{
    convert::Infallible,
    io::{self, Write},
    net::Ipv4Addr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::{JoinSet, spawn_blocking},
    time::timeout,
};

const CONNECTIONS: usize = 12;
const CORE_CALLS: usize = 4;
const BODY_LIMIT: usize = 4 * 1024;
const HEADER_WAIT: Duration = Duration::from_secs(3);
const BODY_WAIT: Duration = Duration::from_secs(2);
const CORE_WAIT: Duration = Duration::from_secs(16);
const PREVIEW_LIMIT: usize = 64 * 1024;
const DOWNLOAD_LIMIT: usize = 8 * 1024 * 1024;

type HttpResponse = Response<Full<Bytes>>;

struct Fault(StatusCode, &'static str, &'static str);

impl Fault {
    fn response(self) -> HttpResponse {
        json_response(self.0, json!({"error":{"code":self.1,"message":self.2}}))
    }
}

const UNAUTHENTICATED: Fault = Fault(
    StatusCode::UNAUTHORIZED,
    "AuthenticationRequired",
    "配对码或会话已经失效",
);
const INVALID: Fault = Fault(StatusCode::BAD_REQUEST, "InvalidArgument", "请求参数无效");
const UNAVAILABLE: Fault = Fault(
    StatusCode::SERVICE_UNAVAILABLE,
    "DependencyUnavailable",
    "暂时无法读取 PsiOS Core",
);

struct Shell {
    host: String,
    origin: String,
    principal: String,
    cancel_pending_tasks: bool,
    client: Client,
    session: Mutex<Session>,
    rpc_slots: Arc<Semaphore>,
}

#[derive(PartialEq, Eq)]
enum Endpoint {
    Pair,
    Logout,
    ObserveEnd,
    Node,
    Tasks,
    Task,
    TaskHistory,
    TaskArtifact,
    CancelPendingTask,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Pair {
    code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskHistory {
    id: String,
    through_version: String,
    after_version: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskArtifact {
    task_id: String,
    reference: ResourceRef,
    #[serde(default)]
    download: bool,
}

impl TaskArtifact {
    fn check_membership(&self, client: &Client) -> ApiResult<()> {
        let task = client.call("Task.Get", json!({"id": self.task_id}))?;
        let reference = json!(self.reference);
        if task["result"]["artifact_refs"]
            .as_array()
            .is_some_and(|items| items.contains(&reference))
        {
            return Ok(());
        }
        Err(ApiError::new("NotFound", "artifact unavailable in this Task result").into())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelPendingTask {
    id: String,
    expected_resource_version: String,
}

enum CoreRequest {
    Call(&'static str, Value),
    Artifact(TaskArtifact),
    CancelPending(CancelPendingTask),
}

impl CoreRequest {
    fn execute(
        self,
        client: &Client,
        check: impl Fn() -> ApiResult<()>,
    ) -> ApiResult<HttpResponse> {
        check()?;
        match self {
            Self::Call(method, params) => {
                Ok(json_response(StatusCode::OK, client.call(method, params)?))
            }
            Self::CancelPending(request) => {
                let task = client.call("Task.Get", json!({"id":request.id}))?;
                if task["task_revision"] != request.expected_resource_version {
                    return Err(ApiError::new("Conflict", "task revision changed").into());
                }
                if !matches!(
                    task["lifecycle"].as_str(),
                    Some("Queued" | "WaitingApproval")
                ) {
                    return Err(
                        ApiError::new("PreconditionFailed", "task is no longer pending").into(),
                    );
                }
                check()?;
                // Core compares the same revision inside its cancellation transaction.
                let result = client.call(
                    "Task.Cancel",
                    json!({"id":request.id,
                    "expected_resource_version":request.expected_resource_version}),
                )?;
                Ok(json_response(StatusCode::OK, result))
            }
            Self::Artifact(artifact) => {
                artifact.check_membership(client)?;
                let limit = if artifact.download {
                    DOWNLOAD_LIMIT
                } else {
                    PREVIEW_LIMIT
                };
                let bytes = client.artifact_bytes_checked(&artifact.reference, limit, &check)?;
                check()?;
                artifact.check_membership(client)?;
                if artifact.download {
                    let mut file = response(
                        StatusCode::OK,
                        "application/octet-stream",
                        Bytes::from(bytes),
                    );
                    file.headers_mut().insert(
                        header::CONTENT_DISPOSITION,
                        HeaderValue::from_static("attachment; filename=\"psios-artifact.bin\""),
                    );
                    return Ok(file);
                }
                let content = String::from_utf8(bytes).map_err(|_| {
                    ApiError::new("RecoveryRequired", "verified artifact is not UTF-8")
                })?;
                Ok(json_response(
                    StatusCode::OK,
                    json!({"reference":artifact.reference,"content_json":content}),
                ))
            }
        }
    }
}

fn response(status: StatusCode, content_type: &'static str, body: Bytes) -> HttpResponse {
    let mut result = Response::new(Full::new(body));
    *result.status_mut() = status;
    for (key, val) in [
        ("content-type", content_type),
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("cross-origin-resource-policy", "same-origin"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; form-action 'none'; base-uri 'none'",
        ),
    ] {
        result
            .headers_mut()
            .insert(key, HeaderValue::from_static(val));
    }
    result
}

fn json_response(status: StatusCode, body: Value) -> HttpResponse {
    response(
        status,
        "application/json; charset=utf-8",
        Bytes::from(body.to_string()),
    )
}

fn single<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn file(path: &str) -> Option<HttpResponse> {
    let (mime, contents) = match path {
        "/" => (
            "text/html; charset=utf-8",
            include_bytes!("index.html").as_slice(),
        ),
        "/shell.css" => (
            "text/css; charset=utf-8",
            include_bytes!("shell.css").as_slice(),
        ),
        "/shell.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell.js").as_slice(),
        ),
        _ => return None,
    };
    Some(response(StatusCode::OK, mime, Bytes::from_static(contents)))
}

async fn body(request: Request<Incoming>) -> Result<Value, Fault> {
    if single(request.headers(), header::CONTENT_TYPE.as_str()) != Some("application/json") {
        return Err(Fault(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "InvalidArgument",
            "需要 JSON 请求",
        ));
    }
    let limited = Limited::new(request.into_body(), BODY_LIMIT);
    let bytes = timeout(BODY_WAIT, limited.collect())
        .await
        .map_err(|_| {
            Fault(
                StatusCode::REQUEST_TIMEOUT,
                "DeadlineExceeded",
                "请求读取超时",
            )
        })?
        .map_err(|error| {
            if error.is::<http_body_util::LengthLimitError>() {
                Fault(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "InvalidArgument",
                    "请求超过大小限制",
                )
            } else {
                INVALID
            }
        })?
        .to_bytes();
    parse(&bytes).map_err(|_| INVALID)
}

fn rpc(endpoint: Endpoint, data: Value) -> Result<CoreRequest, Fault> {
    match endpoint {
        Endpoint::Node => {
            let _: Empty = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call("Node.Get", json!({})))
        }
        Endpoint::Tasks => {
            let page: Page = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call(
                "Resource.List",
                json!({"kind":"Task","limit":20,"cursor":page.cursor}),
            ))
        }
        Endpoint::Task => {
            let task: Task = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call("Task.Get", json!({"id":task.id})))
        }
        Endpoint::TaskHistory => {
            let page: TaskHistory = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call(
                "Task.History",
                json!({"id":page.id,"through_version":page.through_version,
                    "after_version":page.after_version,"limit":3}),
            ))
        }
        Endpoint::CancelPendingTask => Ok(CoreRequest::CancelPending(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::TaskArtifact => Ok(CoreRequest::Artifact(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        _ => Err(INVALID),
    }
}

fn core_error(error: &psios_api::ApiError) -> HttpResponse {
    let status = match error.code.as_str() {
        "AuthenticationRequired" => StatusCode::UNAUTHORIZED,
        "PermissionDenied" => StatusCode::FORBIDDEN,
        "NotFound" => StatusCode::NOT_FOUND,
        "PreconditionFailed" | "CursorExpired" | "Conflict" => StatusCode::CONFLICT,
        "InvalidArgument" => StatusCode::BAD_REQUEST,
        "QuotaExceeded" => StatusCode::PAYLOAD_TOO_LARGE,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    json_response(status, json!({"error":error}))
}

impl Shell {
    fn authorized(&self, token: &str) -> bool {
        self.session
            .lock()
            .is_ok_and(|mut s| s.authorize(token, now()))
    }

    async fn request(
        self: Arc<Self>,
        request: Request<Incoming>,
        observing: &AtomicBool,
    ) -> Result<HttpResponse, Fault> {
        let host = single(request.headers(), header::HOST.as_str());
        if host != Some(self.host.as_str())
            || request.uri().authority().is_some()
            || request.uri().query().is_some()
        {
            return Err(Fault(
                StatusCode::FORBIDDEN,
                "PermissionDenied",
                "不接受此地址",
            ));
        }
        if request.method() == Method::GET {
            return file(request.uri().path()).ok_or(Fault(
                StatusCode::NOT_FOUND,
                "NotFound",
                "页面不存在",
            ));
        }
        if request.method() != Method::POST {
            return Err(Fault(
                StatusCode::METHOD_NOT_ALLOWED,
                "InvalidArgument",
                "不支持此请求方法",
            ));
        }
        let same_origin =
            single(request.headers(), header::ORIGIN.as_str()) == Some(self.origin.as_str());
        let fetch_site = single(request.headers(), "sec-fetch-site");
        if !same_origin
            || (request.headers().contains_key("sec-fetch-site")
                && fetch_site != Some("same-origin"))
        {
            return Err(Fault(
                StatusCode::FORBIDDEN,
                "PermissionDenied",
                "不接受此请求来源",
            ));
        }
        let endpoint = match request.uri().path() {
            "/api/session" => Endpoint::Pair,
            "/api/logout" => Endpoint::Logout,
            "/api/session-end" => Endpoint::ObserveEnd,
            "/api/node" => Endpoint::Node,
            "/api/tasks" => Endpoint::Tasks,
            "/api/task" => Endpoint::Task,
            "/api/cancel-pending-task" if self.cancel_pending_tasks => Endpoint::CancelPendingTask,
            "/api/task-history" => Endpoint::TaskHistory,
            "/api/task-artifact" => Endpoint::TaskArtifact,
            _ => return Err(Fault(StatusCode::NOT_FOUND, "NotFound", "操作不存在")),
        };
        if endpoint == Endpoint::Pair {
            let pair: Pair = serde_json::from_value(body(request).await?).map_err(|_| INVALID)?;
            let token = session::secret().map_err(|_| UNAVAILABLE)?;
            let mut session = self.session.lock().map_err(|_| UNAVAILABLE)?;
            if !session.pair(&pair.code, &token, now()) {
                return Err(UNAUTHENTICATED);
            }
            return Ok(json_response(
                StatusCode::OK,
                json!({"token":token,"principal_id":self.principal,"cancel_pending_tasks":self.cancel_pending_tasks}),
            ));
        }

        let token = single(request.headers(), header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(UNAUTHENTICATED)?
            .to_owned();
        if endpoint == Endpoint::ObserveEnd {
            let _: Empty = serde_json::from_value(body(request).await?).map_err(|_| INVALID)?;
            let end = {
                let mut session = self.session.lock().map_err(|_| UNAVAILABLE)?;
                if !session.accepts(&token, now()) {
                    return Err(UNAUTHENTICATED);
                }
                session.observe_end().ok_or(Fault(
                    StatusCode::CONFLICT,
                    "Conflict",
                    "此会话已有结束通知连接",
                ))?
            };
            observing.store(true, Ordering::Release);
            end.await.map_err(|_| UNAVAILABLE)?;
            return Ok(json_response(StatusCode::OK, json!({"ended": true})));
        }
        if !self.authorized(&token) {
            return Err(UNAUTHENTICATED);
        }
        let data = body(request).await?;
        if endpoint == Endpoint::Logout {
            let _: Empty = serde_json::from_value(data).map_err(|_| INVALID)?;
            let mut session = self.session.lock().map_err(|_| UNAVAILABLE)?;
            if !session.authorize(&token, now()) {
                return Err(UNAUTHENTICATED);
            }
            session.close();
            return Ok(json_response(StatusCode::OK, json!({})));
        }
        let operation = rpc(endpoint, data)?;
        let slot = self.rpc_slots.clone().try_acquire_owned().map_err(|_| {
            Fault(
                StatusCode::TOO_MANY_REQUESTS,
                "QueueFull",
                "请求过多，请稍后重试",
            )
        })?;
        let context = self.clone();
        // Dropping the HTTP future (including on timeout) releases the receiver.
        // An issued Core call may finish. Check before issuing each subsequent call.
        let (consumer, active_request) = oneshot::channel::<()>();
        let work = spawn_blocking(move || {
            let _slot = slot;
            operation.execute(&context.client, || {
                if consumer.is_closed() {
                    return Err(ApiError::new("Cancelled", "Shell request ended").into());
                }
                if !context.session.lock().is_ok_and(|mut s| !s.closed(now())) {
                    return Err(
                        ApiError::new("AuthenticationRequired", "Shell session ended").into(),
                    );
                }
                Ok(())
            })
        });
        let result = timeout(CORE_WAIT, work)
            .await
            .map_err(|_| UNAVAILABLE)?
            .map_err(|_| UNAVAILABLE)?;
        drop(active_request);
        if !self.authorized(&token) {
            return Err(UNAUTHENTICATED);
        }
        Ok(match result {
            Ok(response) => response,
            Err(error) => {
                if error.code == "AuthenticationRequired"
                    && let Ok(mut session) = self.session.lock()
                {
                    session.close();
                }
                core_error(&error)
            }
        })
    }
}

pub(super) fn run(
    client: Client,
    cancel_pending_tasks: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = client.call("Principal.WhoAmI", json!({}))?;
    let principal = identity["principal_id"]
        .as_str()
        .ok_or("Core did not report a principal")?
        .to_owned();
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(CORE_CALLS)
        .build()?;
    let result = runtime.block_on(serve(client, principal, cancel_pending_tasks, stop));
    runtime.shutdown_timeout(Duration::from_secs(1));
    result.map_err(Into::into)
}

async fn serve(
    client: Client,
    principal: String,
    cancel_pending_tasks: bool,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let code = session::secret()?;
    let shell = Arc::new(Shell {
        host,
        origin: origin.clone(),
        principal,
        cancel_pending_tasks,
        client,
        session: Mutex::new(Session::new(&code, now())?),
        rpc_slots: Arc::new(Semaphore::new(CORE_CALLS)),
    });
    {
        let mut out = io::stdout().lock();
        writeln!(out, "{origin}")?;
        writeln!(out, "Pairing code: {code}")?;
        out.flush()?;
    }
    drop(code);

    let mut connections = JoinSet::new();
    while !stop.load(Ordering::Acquire) && shell.session.lock().is_ok_and(|mut s| !s.closed(now()))
    {
        while connections.try_join_next().is_some() {}
        let Ok(accepted) = timeout(Duration::from_millis(200), listener.accept()).await else {
            continue;
        };
        let (stream, _) = accepted?;
        if connections.len() >= CONNECTIONS {
            continue;
        }
        let shell = shell.clone();
        connections.spawn(async move {
            let observing = Arc::new(AtomicBool::new(false));
            let request_observing = observing.clone();
            let service = service_fn(move |request| {
                let observing = request_observing.clone();
                let shell = shell.clone();
                async move {
                    Ok::<_, Infallible>(
                        shell
                            .request(request, &observing)
                            .await
                            .unwrap_or_else(Fault::response),
                    )
                }
            });
            let mut protocol = http1::Builder::new();
            protocol
                .keep_alive(false)
                .max_buf_size(8192)
                .max_headers(32)
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_WAIT);
            let connection = protocol.serve_connection(TokioIo::new(stream), service);
            tokio::pin!(connection);
            // Only the authenticated, single passive observer may outlive a
            // normal HTTP request. Session closure wakes it during shutdown.
            if timeout(Duration::from_secs(24), connection.as_mut())
                .await
                .is_err()
                && observing.load(Ordering::Acquire)
            {
                let _ = connection.await;
            }
        });
    }
    drop(listener);
    if let Ok(mut session) = shell.session.lock() {
        session.close();
    }
    let _ = timeout(Duration::from_secs(1), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    connections.abort_all();
    Ok(())
}
