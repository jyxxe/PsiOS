//! A browser-facing local client. The Core checks each operation under the paired identity.

mod files;
mod resolver;
mod session;
pub(super) use files::VolumeAccess;

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
use serde::{Deserialize, Serialize};
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
const CHAT_BODY_LIMIT: usize = 12 * 1024;
const CHAT_TEXT_BYTES: usize = 4 * 1024;
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
    options: Options,
    client: Client,
    session: Mutex<Session>,
    rpc_slots: Arc<Semaphore>,
}

pub(super) struct Options {
    pub cancel_pending_tasks: bool,
    pub send_chat: bool,
    pub review_work_items: bool,
    pub manage_agent_sessions: bool,
    pub files: Option<VolumeAccess>,
    pub resolver_delegation: Option<String>,
    pub application_actions_delegation: Option<String>,
}

#[derive(PartialEq, Eq)]
enum Endpoint {
    Pair,
    Logout,
    ObserveEnd,
    Node,
    NodeResolver,
    Tasks,
    Applications,
    ApplicationReleases,
    ApplicationAction,
    VolumeDirectory,
    VolumePreview,
    Conversations,
    ConversationMessages,
    AgentSessions,
    AgentSession,
    ControlAgentSession,
    Projects,
    ProjectWorkItems,
    WorkItemRevision,
    WorkItemEvidence,
    ReviewWorkItem,
    PrepareChatMessage,
    SendChatMessage,
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
struct ApplicationReleases {
    id: String,
    cursor: Option<String>,
}

#[derive(Deserialize)]
enum ApplicationAction {
    Start,
    Stop,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplicationActionRequest {
    reference: ResourceRef,
    action: ApplicationAction,
    key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryPage {
    after_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectWorkItems {
    project_id: String,
    after_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkItemRevision {
    reference: ResourceRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewWorkItem {
    reference: ResourceRef,
    accept: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationMessages {
    id: String,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatDraft {
    id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedChatMessage {
    prepared: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ById {
    id: String,
}

#[derive(Deserialize, Serialize)]
struct AgentSessionStatus {
    reference: ResourceRef,
    session: AgentSessionState,
}

#[derive(Deserialize, Serialize)]
struct AgentSessionState {
    lifecycle: String,
    turn: u64,
    max_turns: u64,
    model_ref: ResourceRef,
    skill_ref: ResourceRef,
    last_task: Option<String>,
    pause_reason: Option<String>,
    outcome_knowledge: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentSessionAction {
    Pause,
    Resume,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionControl {
    reference: ResourceRef,
    action: AgentSessionAction,
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
    NodeResolver(String),
    Applications(Page),
    ApplicationReleases(ApplicationReleases),
    ApplicationAction(ApplicationActionRequest, String),
    VolumeDirectory(files::Directory, VolumeAccess),
    VolumePreview(files::Preview, VolumeAccess),
    Conversations(DirectoryPage),
    ConversationMessages(ConversationMessages),
    AgentSessions(Page),
    AgentSession(ById),
    ControlAgentSession(AgentSessionControl),
    Projects(DirectoryPage),
    ProjectWorkItems(ProjectWorkItems),
    WorkItemRevision(WorkItemRevision),
    WorkItemEvidence(WorkItemRevision),
    ReviewWorkItem(ReviewWorkItem),
    PrepareChatMessage(ChatDraft),
    SendChatMessage(PreparedChatMessage),
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
            Self::NodeResolver(delegation) => Ok(json_response(
                StatusCode::OK,
                resolver::observe(client, &delegation, &check)?,
            )),
            Self::VolumeDirectory(directory, access) => Ok(json_response(
                StatusCode::OK,
                directory.read(access, client, &check)?,
            )),
            Self::VolumePreview(preview, access) => Ok(json_response(
                StatusCode::OK,
                preview.read(access, client, &check)?,
            )),
            Self::Applications(page) => {
                let snapshot = client.call(
                    "Resource.List",
                    json!({"kind":"Application","limit":8,"cursor":page.cursor}),
                )?;
                let references = snapshot["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "application snapshot is malformed")
                })?;
                let mut items = Vec::with_capacity(references.len());
                for reference in references {
                    check()?;
                    let app = client.call("Resource.Resolve", json!({"reference":reference}))?;
                    if app["reference"] != *reference || app["resource"]["kind"] != "Application" {
                        return Err(ApiError::new(
                            "RecoveryRequired",
                            "application reference did not resolve exactly",
                        )
                        .into());
                    }
                    // The resource envelope also contains execution configuration.
                    // Send only the catalog fields used by the embedded page.
                    items.push(json!({
                        "reference":reference,
                        "name":app["resource"]["spec"]["name"],
                        "lifecycle":app["resource"]["status"]["lifecycle"],
                    }));
                }
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "items": items,
                        "next_cursor": snapshot["next_cursor"],
                    }),
                ))
            }
            Self::AgentSessions(page) => {
                let snapshot = client.call(
                    "Resource.List",
                    json!({"kind":"Session","limit":8,"cursor":page.cursor}),
                )?;
                let references = snapshot["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "agent session snapshot is malformed")
                })?;
                let mut items = Vec::with_capacity(references.len());
                for reference in references {
                    check()?;
                    let id = reference["resource_id"].as_str().ok_or_else(|| {
                        ApiError::new("RecoveryRequired", "agent session reference is malformed")
                    })?;
                    items.push(session_status(client, id)?);
                }
                Ok(json_response(
                    StatusCode::OK,
                    json!({"items":items,"next_cursor":snapshot["next_cursor"]}),
                ))
            }
            Self::AgentSession(request) => {
                let status = session_status(client, &request.id)?;
                check()?;
                let usage = client.session_model_usage(&request.id)?;
                if status.reference != usage.session_ref {
                    return Err(ApiError::new(
                        "Conflict",
                        "agent session changed while reading model usage",
                    )
                    .into());
                }
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "status":status,"charged_calls":usage.charged_calls,
                        "lease_time":usage.lease_time,"provider_reported":usage.provider_reported,
                    }),
                ))
            }
            Self::ControlAgentSession(request) => {
                let reference = &request.reference;
                let status = session_status(client, &reference.resource_id)?;
                if status.reference != *reference {
                    return Err(ApiError::new(
                        "Conflict",
                        "agent session changed before the requested action",
                    )
                    .into());
                }
                let mut params = json!({"id":reference.resource_id,"expected_resource_version":reference.version});
                let method = match request.action {
                    AgentSessionAction::Pause if status.session.lifecycle == "AwaitingModel" => {
                        params["reason"] = json!("Requested from PsiOS Shell");
                        "Session.Pause"
                    }
                    AgentSessionAction::Resume
                        if status.session.lifecycle == "Paused"
                            && status.session.pause_reason.as_deref() == Some("OperatorPaused")
                            && status.session.outcome_knowledge.as_deref() != Some("Unknown") =>
                    {
                        "Session.Resume"
                    }
                    _ => {
                        return Err(ApiError::new(
                            "PreconditionFailed",
                            "agent session has no eligible model boundary",
                        )
                        .into());
                    }
                };
                check()?;
                let updated =
                    agent_session_status(client.call(method, params)?, &reference.resource_id)?;
                Ok(json_response(
                    StatusCode::OK,
                    json!({"reference":updated.reference}),
                ))
            }
            Self::ApplicationReleases(request) => {
                let page = client.call(
                    "Application.ListReleases",
                    json!({"application_id":request.id,"cursor":request.cursor,"limit":8}),
                )?;
                let releases = page["items"].as_array().ok_or_else(|| {
                    ApiError::new(
                        "RecoveryRequired",
                        "application release snapshot is malformed",
                    )
                })?;
                let items = releases
                    .iter()
                    .map(|release| {
                        json!({
                            "release_ref": release["release_ref"],
                            "release_version": release["release_version"],
                            "package_format": release["package_format"],
                            "state": release["state"],
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "application_ref": page["application_ref"],
                        "application_lifecycle": page["application_lifecycle"],
                        "selected_release_ref": page["selected_release_ref"],
                        "items": items,
                        "next_cursor": page["next_cursor"],
                    }),
                ))
            }
            Self::ApplicationAction(request, delegation) => {
                let reference = json!(request.reference);
                let resolved = client.call("Resource.Resolve", json!({"reference": reference}))?;
                if resolved["reference"] != reference
                    || resolved["resource"]["kind"] != "Application"
                {
                    return Err(ApiError::new(
                        "PreconditionFailed",
                        "expected an exact application reference",
                    )
                    .into());
                }
                let tool = match request.action {
                    ApplicationAction::Start => "application.start",
                    ApplicationAction::Stop => "application.stop",
                };
                check()?;
                let prepared = client.call("Task.Prepare", json!({
                    "tool": tool,
                    "input": {"id": request.reference.resource_id, "expected_resource_version": request.reference.version},
                    "key": request.key,
                    "delegation_id": delegation,
                }))?;
                check()?;
                let accepted = client.call("Task.Submit", prepared)?;
                let id = accepted["task_id"].as_str().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "application task receipt is malformed")
                })?;
                Ok(json_response(StatusCode::ACCEPTED, json!({"task_id": id})))
            }
            Self::Conversations(request) => {
                let page = client.call(
                    "Conversation.List",
                    json!({"after_id":request.after_id,"limit":12}),
                )?;
                let conversations = page["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "conversation directory is malformed")
                })?;
                let items = conversations
                    .iter()
                    .map(|conversation| {
                        json!({
                            "id":conversation["id"],
                            "kind":conversation["kind"],
                            "name":conversation["name"],
                            "lifecycle":conversation["lifecycle"],
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "items":items,
                        "next_after_id":page["next_after_id"],
                    }),
                ))
            }
            Self::ConversationMessages(request) => {
                let page = client.call(
                    "Message.List",
                    json!({"id":request.id,"cursor":request.cursor,"limit":16}),
                )?;
                let messages = page["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "conversation history is malformed")
                })?;
                let items = messages
                    .iter()
                    .map(|entry| {
                        let message = &entry["message"];
                        let text =
                            entry["content"]["parts"].as_array().and_then(|parts| {
                                match parts.as_slice() {
                                    [part] if part["kind"] == "text" => part["text"].as_str(),
                                    _ => None,
                                }
                            });
                        json!({
                            "from":message["sender_principal_id"],
                            "accepted_at":message["accepted_at"],
                            "text":text,
                        })
                    })
                    .collect::<Vec<_>>();
                let next = (page["has_more"].as_bool() == Some(true)).then(|| &page["cursor"]);
                Ok(json_response(
                    StatusCode::OK,
                    json!({"items":items,"next_cursor":next}),
                ))
            }
            Self::Projects(request) => {
                let page = client.call(
                    "Project.List",
                    json!({"after_id":request.after_id,"limit":8}),
                )?;
                let projects = page["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "project directory is malformed")
                })?;
                let items = projects
                    .iter()
                    .map(|project| {
                        json!({
                            "id": project["reference"]["resource_id"],
                            "title": project["value"]["title"],
                            "role": project["value"]["role"],
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json_response(
                    StatusCode::OK,
                    json!({"items":items,"next_after_id":page["next_after_id"]}),
                ))
            }
            Self::ProjectWorkItems(request) => {
                let page = client.call(
                    "WorkItem.List",
                    json!({
                        "project_id":request.project_id,"after_id":request.after_id,"limit":12,
                    }),
                )?;
                check()?;
                let project = client.call("Project.Get", json!({"id":request.project_id}))?;
                let work_items = page["items"].as_array().ok_or_else(|| {
                    ApiError::new("RecoveryRequired", "work item page is malformed")
                })?;
                let items = work_items.iter().map(|item| {
                    let value = &item["value"];
                    json!({
                        "id": item["reference"]["resource_id"],
                        "reference": item["reference"],
                        "title": value["title"],
                        "lifecycle": value["lifecycle"],
                        "assignee": value["assignee_principal_id"],
                        "priority": value["priority"].as_str().unwrap_or("Normal"),
                        "blocked": value["blockers"].as_array().is_some_and(|items| !items.is_empty()),
                    })
                }).collect::<Vec<_>>();
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "title":project["value"]["title"],"items":items,"next_after_id":page["next_after_id"],
                    }),
                ))
            }
            Self::WorkItemRevision(request) => {
                let item = exact_work_item(client, &request.reference)?;
                let evidence = item.get("evidence").map(|proof| {
                    json!({
                        "task_ref": proof["task_ref"],
                        "artifact_ref": proof["artifact_ref"],
                        "submitted_at": proof["submitted_at"],
                    })
                });
                let review = item["review"].as_object().map(|review| {
                    json!({
                        "accepted": review.get("accepted"),
                        "reviewer": review.get("reviewer_principal_id"),
                        "reviewed_at": review.get("reviewed_at"),
                    })
                });
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "reference": request.reference,
                        "title": item["title"],
                        "description": item["description"],
                        "lifecycle": item["lifecycle"],
                        "assignee": item["assignee_principal_id"],
                        "priority": item["priority"].as_str().unwrap_or("Normal"),
                        "has_evidence": item["has_evidence"],
                        "evidence": evidence,
                        "review": review,
                    }),
                ))
            }
            Self::WorkItemEvidence(request) => {
                let artifact = evidence_artifact(client, &request.reference)?;
                bounded_artifact_response(client, &artifact, true, &check, || {
                    if evidence_artifact(client, &request.reference)? != artifact {
                        return Err(ApiError::new("Conflict", "work item evidence changed").into());
                    }
                    Ok(())
                })
            }
            Self::ReviewWorkItem(request) => {
                let item = exact_work_item(client, &request.reference)?;
                if item["lifecycle"] != "InReview" {
                    return Err(ApiError::new(
                        "PreconditionFailed",
                        "work item has no pending review",
                    )
                    .into());
                }
                if !item["evidence"].is_object() {
                    return Err(ApiError::new(
                        "PermissionDenied",
                        "work item evidence is unavailable",
                    )
                    .into());
                }
                check()?;
                let reviewed = client.review_work_item(
                    &request.reference.resource_id,
                    &request.reference.version,
                    request.accept,
                )?;
                Ok(json_response(
                    StatusCode::OK,
                    json!({"reference":reviewed.reference}),
                ))
            }
            Self::PrepareChatMessage(draft) => {
                if draft.text.is_empty() || draft.text.len() > CHAT_TEXT_BYTES {
                    return Err(ApiError::new(
                        "InvalidArgument",
                        "message must contain 1 to 4096 UTF-8 bytes",
                    )
                    .into());
                }
                let window = client.call(
                    "Message.OpenSendWindow",
                    json!({"id":draft.id,"ttl_seconds":120}),
                )?;
                let namespace = window["namespace"].as_str().ok_or_else(|| {
                    ApiError::new(
                        "RecoveryRequired",
                        "Core returned no message send namespace",
                    )
                })?;
                check()?;
                let key = format!("{namespace}:{}", uuid::Uuid::new_v4().simple());
                let prepared = client.call(
                    "Message.Prepare",
                    json!({"conversation_id":draft.id,"client_send_id":key,"text":draft.text}),
                )?;
                Ok(json_response(StatusCode::OK, json!({"prepared":prepared})))
            }
            Self::SendChatMessage(request) => {
                let receipt = client.call("Message.Send", request.prepared)?;
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "message_id":receipt["message_id"],
                        "accepted_at":receipt["accepted_at"],
                        "deduplicated":receipt["deduplicated"],
                    }),
                ))
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
                bounded_artifact_response(
                    client,
                    &artifact.reference,
                    artifact.download,
                    &check,
                    || artifact.check_membership(client),
                )
            }
        }
    }
}

fn session_status(client: &Client, id: &str) -> ApiResult<AgentSessionStatus> {
    agent_session_status(client.call("Session.Get", json!({"id":id}))?, id)
}

fn agent_session_status(record: Value, id: &str) -> ApiResult<AgentSessionStatus> {
    let invalid = || {
        ApiError::new(
            "RecoveryRequired",
            "agent session returned an invalid record",
        )
    };
    let status: AgentSessionStatus = serde_json::from_value(record).map_err(|_| invalid())?;
    if status.reference.resource_id != id {
        return Err(invalid().into());
    }
    Ok(status)
}

fn exact_work_item(client: &Client, reference: &ResourceRef) -> ApiResult<Value> {
    let reference = json!(reference);
    let mut record = client.call("WorkItem.RevisionGet", json!({"reference":reference}))?;
    if record["reference"] != reference {
        return Err(ApiError::new(
            "RecoveryRequired",
            "work item reference did not resolve exactly",
        )
        .into());
    }
    Ok(record["value"].take())
}

fn evidence_artifact(client: &Client, reference: &ResourceRef) -> ApiResult<ResourceRef> {
    let item = exact_work_item(client, reference)?;
    let artifact = item["evidence"]
        .get("artifact_ref")
        .cloned()
        .ok_or_else(|| ApiError::new("NotFound", "work item evidence is unavailable"))?;
    serde_json::from_value(artifact).map_err(|_| {
        ApiError::new(
            "RecoveryRequired",
            "work item evidence has a malformed artifact reference",
        )
        .into()
    })
}

fn bounded_artifact_response(
    client: &Client,
    reference: &ResourceRef,
    download: bool,
    check: &impl Fn() -> ApiResult<()>,
    confirm: impl FnOnce() -> ApiResult<()>,
) -> ApiResult<HttpResponse> {
    let limit = if download {
        DOWNLOAD_LIMIT
    } else {
        PREVIEW_LIMIT
    };
    let bytes = client.artifact_bytes_checked(reference, limit, check)?;
    check()?;
    confirm()?;
    if download {
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
    let content = String::from_utf8(bytes)
        .map_err(|_| ApiError::new("RecoveryRequired", "verified artifact is not UTF-8"))?;
    Ok(json_response(
        StatusCode::OK,
        json!({"reference":reference,"content_json":content}),
    ))
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
        "/shell-chat.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell-chat.js").as_slice(),
        ),
        "/shell-projects.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell-projects.js").as_slice(),
        ),
        "/shell-sessions.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell-sessions.js").as_slice(),
        ),
        "/shell-files.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell-files.js").as_slice(),
        ),
        "/shell.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("shell.js").as_slice(),
        ),
        _ => return None,
    };
    Some(response(StatusCode::OK, mime, Bytes::from_static(contents)))
}

async fn body(request: Request<Incoming>, limit: usize) -> Result<Value, Fault> {
    if single(request.headers(), header::CONTENT_TYPE.as_str()) != Some("application/json") {
        return Err(Fault(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "InvalidArgument",
            "需要 JSON 请求",
        ));
    }
    let limited = Limited::new(request.into_body(), limit);
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

fn rpc(endpoint: Endpoint, data: Value, options: &Options) -> Result<CoreRequest, Fault> {
    match endpoint {
        Endpoint::Node => {
            let _: Empty = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call("Node.Get", json!({})))
        }
        Endpoint::NodeResolver => {
            let _: Empty = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::NodeResolver(
                options.resolver_delegation.clone().ok_or(INVALID)?,
            ))
        }
        Endpoint::Tasks => {
            let page: Page = serde_json::from_value(data).map_err(|_| INVALID)?;
            Ok(CoreRequest::Call(
                "Resource.List",
                json!({"kind":"Task","limit":20,"cursor":page.cursor}),
            ))
        }
        Endpoint::Applications => Ok(CoreRequest::Applications(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ApplicationReleases => Ok(CoreRequest::ApplicationReleases(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ApplicationAction => {
            let request: ApplicationActionRequest =
                serde_json::from_value(data).map_err(|_| INVALID)?;
            if !uuid::Uuid::parse_str(&request.key)
                .is_ok_and(|key| key.hyphenated().to_string() == request.key)
            {
                return Err(INVALID);
            }
            Ok(CoreRequest::ApplicationAction(
                request,
                options
                    .application_actions_delegation
                    .clone()
                    .ok_or(INVALID)?,
            ))
        }
        Endpoint::VolumeDirectory => Ok(CoreRequest::VolumeDirectory(
            serde_json::from_value(data).map_err(|_| INVALID)?,
            options.files.clone().ok_or(INVALID)?,
        )),
        Endpoint::VolumePreview => Ok(CoreRequest::VolumePreview(
            serde_json::from_value(data).map_err(|_| INVALID)?,
            options.files.clone().ok_or(INVALID)?,
        )),
        Endpoint::Conversations => Ok(CoreRequest::Conversations(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ConversationMessages => Ok(CoreRequest::ConversationMessages(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::Projects => Ok(CoreRequest::Projects(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ProjectWorkItems => Ok(CoreRequest::ProjectWorkItems(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::WorkItemRevision => Ok(CoreRequest::WorkItemRevision(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::WorkItemEvidence => Ok(CoreRequest::WorkItemEvidence(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::AgentSessions => Ok(CoreRequest::AgentSessions(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::AgentSession => Ok(CoreRequest::AgentSession(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ControlAgentSession => Ok(CoreRequest::ControlAgentSession(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::ReviewWorkItem => Ok(CoreRequest::ReviewWorkItem(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::PrepareChatMessage => Ok(CoreRequest::PrepareChatMessage(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::SendChatMessage => Ok(CoreRequest::SendChatMessage(
            serde_json::from_value(data).map_err(|_| INVALID)?,
        )),
        Endpoint::Task => {
            let task: ById = serde_json::from_value(data).map_err(|_| INVALID)?;
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
            "/api/node-resolver" if self.options.resolver_delegation.is_some() => {
                Endpoint::NodeResolver
            }
            "/api/tasks" => Endpoint::Tasks,
            "/api/applications" => Endpoint::Applications,
            "/api/application-releases" => Endpoint::ApplicationReleases,
            "/api/application-action" if self.options.application_actions_delegation.is_some() => {
                Endpoint::ApplicationAction
            }
            "/api/volume-directory" if self.options.files.is_some() => Endpoint::VolumeDirectory,
            "/api/volume-preview"
                if self
                    .options
                    .files
                    .as_ref()
                    .is_some_and(|files| files.preview) =>
            {
                Endpoint::VolumePreview
            }
            "/api/conversations" => Endpoint::Conversations,
            "/api/conversation-messages" => Endpoint::ConversationMessages,
            "/api/agent-sessions" => Endpoint::AgentSessions,
            "/api/agent-session" => Endpoint::AgentSession,
            "/api/control-agent-session" if self.options.manage_agent_sessions => {
                Endpoint::ControlAgentSession
            }
            "/api/projects" => Endpoint::Projects,
            "/api/project-work-items" => Endpoint::ProjectWorkItems,
            "/api/work-item-revision" => Endpoint::WorkItemRevision,
            "/api/work-item-evidence" => Endpoint::WorkItemEvidence,
            "/api/review-work-item" if self.options.review_work_items => Endpoint::ReviewWorkItem,
            "/api/chat-prepare" if self.options.send_chat => Endpoint::PrepareChatMessage,
            "/api/chat-send" if self.options.send_chat => Endpoint::SendChatMessage,
            "/api/task" => Endpoint::Task,
            "/api/cancel-pending-task" if self.options.cancel_pending_tasks => {
                Endpoint::CancelPendingTask
            }
            "/api/task-history" => Endpoint::TaskHistory,
            "/api/task-artifact" => Endpoint::TaskArtifact,
            _ => return Err(Fault(StatusCode::NOT_FOUND, "NotFound", "操作不存在")),
        };
        if endpoint == Endpoint::Pair {
            let pair: Pair =
                serde_json::from_value(body(request, BODY_LIMIT).await?).map_err(|_| INVALID)?;
            let token = session::secret().map_err(|_| UNAVAILABLE)?;
            let mut session = self.session.lock().map_err(|_| UNAVAILABLE)?;
            if !session.pair(&pair.code, &token, now()) {
                return Err(UNAUTHENTICATED);
            }
            let mut reply = json!({
                "token":token,"principal_id":self.principal,"cancel_pending_tasks":self.options.cancel_pending_tasks,
                "send_chat":self.options.send_chat,"review_work_items":self.options.review_work_items,
                "manage_agent_sessions":self.options.manage_agent_sessions,
                "observe_resolver":self.options.resolver_delegation.is_some(),
            });
            if self.options.application_actions_delegation.is_some() {
                reply["manage_applications"] = json!(true);
            }
            if let Some(files) = &self.options.files {
                reply["browse_files"] = json!(true);
                if files.preview {
                    reply["preview_files"] = json!(true);
                }
            }
            return Ok(json_response(StatusCode::OK, reply));
        }

        let token = single(request.headers(), header::AUTHORIZATION.as_str())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(UNAUTHENTICATED)?
            .to_owned();
        if endpoint == Endpoint::ObserveEnd {
            let _: Empty =
                serde_json::from_value(body(request, BODY_LIMIT).await?).map_err(|_| INVALID)?;
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
        let limit = if matches!(
            endpoint,
            Endpoint::PrepareChatMessage | Endpoint::SendChatMessage
        ) {
            CHAT_BODY_LIMIT
        } else {
            BODY_LIMIT
        };
        let data = body(request, limit).await?;
        if endpoint == Endpoint::Logout {
            let _: Empty = serde_json::from_value(data).map_err(|_| INVALID)?;
            let mut session = self.session.lock().map_err(|_| UNAVAILABLE)?;
            if !session.authorize(&token, now()) {
                return Err(UNAUTHENTICATED);
            }
            session.close();
            return Ok(json_response(StatusCode::OK, json!({})));
        }
        let operation = rpc(endpoint, data, &self.options)?;
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

pub(super) fn run(client: Client, options: Options) -> Result<(), Box<dyn std::error::Error>> {
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
    let result = runtime.block_on(serve(client, principal, options, stop));
    runtime.shutdown_timeout(Duration::from_secs(1));
    result.map_err(Into::into)
}

async fn serve(
    client: Client,
    principal: String,
    options: Options,
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
        options,
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
