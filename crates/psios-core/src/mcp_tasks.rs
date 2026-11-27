//! Core-owned mapping from authenticated local MCP invocations to ordinary Tasks.
use crate::{
    Result, artifact, broker,
    db::{self, Db, Doc},
    decode, err, local_read_tools, policy, require,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use psios_api::{LocalReadBinding, ResourceRef, canonical, id};
use ring::hmac;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(crate) const FORMAT: u32 = crate::schedule::event_workflow_overlap::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("mcp_tasks.sql");
const ADAPTER: &str = "psios-mcp-stdio";
const PROTOCOL: &str = "2026-07-28";
const OWNER_LIMIT: u32 = 32;
const NODE_LIMIT: u32 = 512;
const MAX_RESULT: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    endpoint: String,
    delegation_id: String,
    invocation_key: String,
    binding: LocalReadBinding,
    ttl_seconds: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Access {
    endpoint: String,
    delegation_id: String,
    handle: String,
    bindings: Vec<LocalReadBinding>,
    input_responses: Option<Map<String, Value>>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    adapter: String,
    protocol: String,
    authority: String,
    tool: LocalReadBinding,
    delegation_ref: ResourceRef,
    target_ref: ResourceRef,
    task_ref: ResourceRef,
    operation_ref: ResourceRef,
}

struct Record {
    handle: String,
    owner: String,
    endpoint: String,
    submission_key: String,
    incarnation: String,
    boot: String,
    time_ns: String,
    boot_ms: i64,
    utc_ms: i64,
    ttl_ms: i64,
    spec: Binding,
}

fn read_row(row: &Row<'_>) -> rusqlite::Result<(RecordFields, Value)> {
    Ok((
        (
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
        ),
        row.get(10)?,
    ))
}

type RecordFields = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
);

fn record((fields, spec): (RecordFields, Value)) -> Result<Record> {
    let (
        handle,
        owner,
        endpoint,
        submission_key,
        incarnation,
        boot,
        time_ns,
        boot_ms,
        utc_ms,
        ttl_ms,
    ) = fields;
    Ok(Record {
        handle,
        owner,
        endpoint,
        submission_key,
        incarnation,
        boot,
        time_ns,
        boot_ms,
        utc_ms,
        ttl_ms,
        spec: decode(&spec)
            .map_err(|_| err("RecoveryRequired", "MCP handle binding is malformed"))?,
    })
}

fn valid_label(label: &str, min: usize) -> bool {
    (min..=96).contains(&label.len())
        && label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_handle(handle: &str) -> bool {
    handle
        .strip_prefix("mcp-task-")
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .is_some_and(|value| !value.is_nil() && format!("mcp-task-{value}") == handle)
}

fn unavailable() -> crate::Error {
    err(
        "PermissionDenied",
        "MCP task unavailable for this endpoint and credential",
    )
}

fn original_key(db: &Db<'_>, actor: &str, endpoint: &str, invocation: &str) -> Result<String> {
    // Leave a spent key in the existing permanent submission ledger after the
    // bounded public handle expires. Callers cannot reserve this key in advance.
    let key = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
    let tag = hmac::sign(
        &key,
        &canonical(&json!([
            "psios/mcp-task-key/1",
            ADAPTER,
            PROTOCOL,
            db.meta("authority")?,
            actor,
            endpoint,
            invocation
        ])),
    );
    Ok(format!("mcp-call-{}", URL_SAFE_NO_PAD.encode(tag.as_ref())))
}

fn origin(owner: &str, key: &str) -> String {
    String::from_utf8(canonical(
        &json!({"authority_id":owner,"origin_type":"Service","origin_id":key}),
    ))
    .expect("canonical JSON is UTF-8")
}

impl Record {
    fn live(&self, db: &Db<'_>, clock: &policy::deadline::Clock) -> bool {
        self.incarnation == db.incarnation
            && self.boot == clock.boot.boot_id
            && self.time_ns == clock.time_ns
            && (0..self.ttl_ms).contains(&clock.boot.millis.saturating_sub(self.boot_ms))
    }

    fn authorize(&self, db: &Db<'_>, actor: &str, delegation_id: &str) -> Result<Doc> {
        require(
            self.owner == actor
                && self.spec.authority == db.meta("authority")?
                && self.spec.delegation_ref.resource_id == delegation_id
                && self.live(db, &policy::deadline::Clock::sample()?)
                && local_read_tools::task_authority(db, actor, delegation_id)?,
            "PermissionDenied",
            "MCP task unavailable for this endpoint and credential",
        )?;
        let delegation = policy::delegation(db, actor, delegation_id, self.spec.tool.name())?;
        require(
            delegation.reference() == json!(self.spec.delegation_ref),
            "PermissionDenied",
            "MCP task delegation changed",
        )?;
        let target = &self.spec.target_ref.resource_id;
        let explicit_scope = self.spec.tool.target().is_some()
            || self
                .spec
                .tool
                .unbound_tool()
                .is_some_and(psios_api::LocalReadTool::requires_node_scope);
        if explicit_scope && !local_read_tools::explicitly_scoped(db, &delegation, target)? {
            return Err(unavailable());
        }
        policy::tool_scope(db, &delegation.reference(), target)?;
        Ok(delegation)
    }
}

pub(crate) fn collect(db: &Db<'_>) -> Result<usize> {
    let present: bool =
        db.c.query_row("SELECT EXISTS(SELECT 1 FROM mcp_task_handles)", [], |row| {
            row.get(0)
        })?;
    if !present {
        return Ok(0);
    }
    let clock = policy::deadline::Clock::sample()?;
    Ok(db.c.execute("DELETE FROM mcp_task_handles WHERE handle IN (
        SELECT handle FROM mcp_task_handles WHERE recovery_incarnation<>?1 OR boot_id<>?2 OR time_ns<>?3
        OR ?4<created_boot_ms OR ?4-created_boot_ms>=ttl_ms
        ORDER BY handle LIMIT 32)", params![db.incarnation, clock.boot.boot_id, clock.time_ns, clock.boot.millis])?)
}

pub(crate) fn retains_result(db: &Db<'_>, task_id: &str) -> Result<bool> {
    let Some(raw) =
        db.c.query_row(
            "SELECT * FROM mcp_task_handles WHERE json_extract(spec,'$.task_ref.resource_id')=?1",
            [task_id],
            read_row,
        )
        .optional()?
    else {
        return Ok(false);
    };
    let saved = record(raw)?;
    Ok(saved.live(db, &policy::deadline::Clock::sample()?))
}

pub(crate) fn start(db: &Db<'_>, actor: &str, value: &Value) -> Result<Value> {
    let request: Start = decode(value)?;
    local_read_tools::agent(db, actor)?;
    require(
        valid_label(&request.endpoint, 1)
            && valid_label(&request.invocation_key, 16)
            && (1..=600).contains(&request.ttl_seconds),
        "InvalidArgument",
        "invalid MCP task endpoint, invocation key or lifetime",
    )?;
    if let Some((target, _)) = request.binding.target() {
        crate::label(target)?;
    }
    crate::label(&request.delegation_id)?;
    let key = original_key(db, actor, &request.endpoint, &request.invocation_key)?;
    let previous =
        db.c.query_row(
            "SELECT * FROM mcp_task_handles WHERE owner=?1 AND endpoint=?2 AND submission_key=?3",
            params![actor, request.endpoint, key],
            read_row,
        )
        .optional()?;
    if let Some(previous) = previous {
        let saved = record(previous)?;
        saved.authorize(db, actor, &request.delegation_id)?;
        require(
            saved.spec.tool == request.binding,
            "IdempotencyConflict",
            "MCP invocation key is bound to another tool, target or input",
        )?;
        return snapshot(db, &saved, &current_task(db, &saved)?, false);
    }
    require(
        local_read_tools::task_authority(db, actor, &request.delegation_id)?,
        "PermissionDenied",
        "MCP task delegation unavailable",
    )?;
    let delegation = policy::delegation(db, actor, &request.delegation_id, request.binding.name())?;
    let resolved = local_read_tools::resolve_binding(db, actor, &delegation, &request.binding)?
        .ok_or_else(unavailable)?;
    let spent: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM submissions WHERE owner=?1 AND origin=?2 AND key=?3)",
        params![actor, origin(actor, &key), key],
        |row| row.get(0),
    )?;
    require(
        !spent,
        "IdempotencyKeyExpired",
        "MCP task handle expired; retain the original local Task history",
    )?;
    collect(db)?;
    let (node, owner): (u32, u32) = db.c.query_row(
        "SELECT (SELECT count(*) FROM mcp_task_handles),
        (SELECT count(*) FROM mcp_task_handles WHERE owner=?1)",
        [actor],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if node >= NODE_LIMIT || owner >= OWNER_LIMIT {
        let mut error = err("QuotaExceeded", "MCP task handles at capacity");
        error.0.retryable = true;
        error.0.retry_condition = "SameRequestAfterBackoff".into();
        return Err(error);
    }

    let clock = policy::deadline::Clock::sample()?;
    let at = Utc::now();
    let deadline =
        (at + chrono::Duration::seconds(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let input = request
        .binding
        .task_input(Some(&resolved.target_ref))
        .ok_or_else(unavailable)?;
    let staged = broker::prepare(
        db,
        actor,
        &json!({"tool":request.binding.name(),"input":input,
        "key":key,"delegation_id":request.delegation_id,"deadline_at":deadline}),
    )?;
    let accepted = broker::submit(db, actor, &staged, None)?;
    let task_id = accepted["task_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "MCP submission returned no original Task",
        )
    })?;
    let task = db.of(task_id, "Task")?;
    require(
        task.rev == 1 && accepted["deduplicated"] == false,
        "RecoveryRequired",
        "MCP task admission did not create an original task",
    )?;
    let saved = Record {
        handle: id("mcp-task"),
        owner: actor.into(),
        endpoint: request.endpoint,
        submission_key: key,
        incarnation: db.incarnation.clone(),
        boot: clock.boot.boot_id,
        time_ns: clock.time_ns,
        boot_ms: clock.boot.millis,
        utc_ms: acceptance_time(&accepted)?,
        ttl_ms: i64::from(request.ttl_seconds) * 1000,
        spec: Binding {
            adapter: ADAPTER.into(),
            protocol: PROTOCOL.into(),
            authority: db.meta("authority")?,
            tool: request.binding,
            delegation_ref: decode(&delegation.reference())?,
            target_ref: resolved.target_ref,
            task_ref: decode(&task.reference())?,
            operation_ref: decode(&task.body["operation_refs"][0])?,
        },
    };
    db.c.execute(
        "INSERT INTO mcp_task_handles VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            saved.handle,
            saved.owner,
            saved.endpoint,
            saved.submission_key,
            saved.incarnation,
            saved.boot,
            saved.time_ns,
            saved.boot_ms,
            saved.utc_ms,
            saved.ttl_ms,
            json!(saved.spec)
        ],
    )?;
    snapshot(db, &saved, &task, false)
}

fn current_task(db: &Db<'_>, record: &Record) -> Result<Doc> {
    verify_binding(db.c, &db.meta("authority")?, record).map_err(|_| {
        err(
            "RecoveryRequired",
            "MCP handle lost its exact original Task submission",
        )
    })?;
    let task = db.of(&record.spec.task_ref.resource_id, "Task")?;
    require(
        task.owner == record.owner
            && task.spec["task_type"] == record.spec.tool.name()
            && task.spec["delegation_ref"] == json!(record.spec.delegation_ref)
            && task.spec["idempotency_key"] == record.submission_key
            && task.body["operation_refs"][0]["resource_id"]
                == record.spec.operation_ref.resource_id,
        "RecoveryRequired",
        "MCP handle no longer identifies its original Task",
    )?;
    if let Some(id) = task.body["current_run_id"].as_str() {
        let run = db.of(id, "Run")?;
        require(
            run.owner == task.owner
                && run.body["task_id"] == task.id
                && run.body["operation_refs"][0]["resource_id"]
                    == record.spec.operation_ref.resource_id,
            "RecoveryRequired",
            "MCP task lost its original Run and Operation lineage",
        )?;
    }
    Ok(task)
}

fn snapshot(db: &Db<'_>, record: &Record, task: &Doc, details: bool) -> Result<Value> {
    let lifecycle = task.body["lifecycle"].as_str().unwrap_or_default();
    let knowledge = &task.body["outcome_knowledge"];
    let status = if knowledge == "Unknown" {
        "working"
    } else {
        match lifecycle {
            "Succeeded" | "Failed" => "completed",
            "Cancelled" => "cancelled",
            _ => "working",
        }
    };
    let mut view = json!({"taskId":record.handle,"status":status,"createdAt":task.created,
        "lastUpdatedAt":task.stamp,"ttlMs":record.ttl_ms,"pollIntervalMs":250});
    if knowledge == "Unknown" {
        view["statusMessage"] = json!("Original PsiOS Task outcome is Unknown");
    }
    if details && status == "completed" {
        let result = if lifecycle == "Succeeded" && knowledge == "Known" {
            let artifacts = task.body["result"]["artifact_refs"].as_array();
            if let Some([reference]) = artifacts.map(Vec::as_slice)
                && task.body["result"]["availability"] == "Available"
            {
                let (artifact, _) =
                    broker::result_access::resolve_artifact(db, &record.owner, reference)?;
                require(
                    artifact::is_task_output(&artifact, task)
                        && artifact.spec["operation_id"] == record.spec.operation_ref.resource_id,
                    "RecoveryRequired",
                    "MCP task result lost its original Run or Operation lineage",
                )?;
                let value = artifact::read(db, &artifact, Some(MAX_RESULT))?;
                json!({"task_id":task.id,"result":value})
            } else {
                let code = if task.body["result"]["availability"] == "Evicted" {
                    "ResultEvicted"
                } else {
                    "ResultUnavailable"
                };
                json!({"task_id":task.id,"code":code})
            }
        } else {
            json!({"task_id":task.id,"code":task.body["conditions"][0]["type"].as_str().unwrap_or("ResultUnavailable")})
        };
        view["tool_result"] = result;
    }
    Ok(view)
}

pub(crate) fn access(db: &Db<'_>, actor: &str, method: &str, value: &Value) -> Result<Value> {
    let request: Access = decode(value)?;
    local_read_tools::agent(db, actor)?;
    let mut names = HashSet::new();
    require(
        valid_label(&request.endpoint, 1)
            && valid_handle(&request.handle)
            && (1..=4).contains(&request.bindings.len())
            && request
                .bindings
                .iter()
                .all(|binding| names.insert(binding.name())),
        "InvalidArgument",
        "invalid MCP task handle or binding",
    )?;
    let update = method == "McpTask.Update";
    require(
        update == request.input_responses.is_some(),
        "InvalidArgument",
        "unexpected MCP task input responses",
    )?;
    if let Some(responses) = &request.input_responses {
        require(
            responses.len() <= 16
                && responses.keys().all(|key| crate::label(key).is_ok())
                && responses.values().all(Value::is_object)
                && canonical(&json!(responses)).len() <= MAX_RESULT,
            "InvalidArgument",
            "invalid MCP task input responses",
        )?;
    }
    let saved = record(
        db.c.query_row(
            "SELECT * FROM mcp_task_handles WHERE handle=?1 AND owner=?2 AND endpoint=?3",
            params![request.handle, actor, request.endpoint],
            read_row,
        )
        .optional()?
        .ok_or_else(unavailable)?,
    )?;
    let delegation = saved.authorize(db, actor, &request.delegation_id)?;
    require(
        request.bindings.contains(&saved.spec.tool),
        "PermissionDenied",
        "MCP tool is outside this adapter's configuration",
    )?;
    let task = current_task(db, &saved)?;
    if method == "McpTask.Cancel" {
        broker::cancel(
            db,
            actor,
            &json!({"id":task.id,"expected_resource_version":task.version()}),
        )?;
        db.audit(actor, method, Some(&task.id), "Allowed")?;
        return Ok(json!({}));
    }
    if update {
        // This read-only profile issues no client inputs. Unmatched input
        // keys are ignored as specified, after checking the original scope.
        return Ok(json!({}));
    }
    if saved.spec.tool.target().is_some() && task.body["lifecycle"] == "Succeeded" {
        let resolved = local_read_tools::resolve_binding(db, actor, &delegation, &saved.spec.tool)?
            .ok_or_else(unavailable)?;
        require(
            resolved.target_ref == saved.spec.target_ref,
            "PreconditionFailed",
            "MCP task result target revision changed",
        )?;
    }
    snapshot(db, &saved, &task, true)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |row| {
            row.get(0)
        })?;
    let mut seen = HashSet::new();
    let mut counts = std::collections::HashMap::<String, u32>::new();
    let mut statement = conn.prepare("SELECT * FROM mcp_task_handles ORDER BY handle LIMIT ?1")?;
    let rows = statement.query_map([NODE_LIMIT + 1], read_row)?;
    for raw in rows {
        let saved = record(raw?)?;
        *counts.entry(saved.owner.clone()).or_default() += 1;
        verify_binding(conn, &authority, &saved).map_err(|_| {
            err(
                "RecoveryRequired",
                "MCP handle lost its exact original Task submission",
            )
        })?;
        require(
            seen.insert(saved.spec.task_ref.resource_id)
                && seen.len() <= NODE_LIMIT as usize
                && counts[&saved.owner] <= OWNER_LIMIT,
            "RecoveryRequired",
            "MCP task handle history exceeds its scope limits",
        )?;
    }
    Ok(())
}

fn verify_binding(conn: &Connection, authority: &str, saved: &Record) -> Result<()> {
    let spec = &saved.spec;
    let task = db::resolve(conn, &json!(spec.task_ref))?;
    let operation = db::resolve(conn, &json!(spec.operation_ref))?;
    let plan = db::resolve(conn, &operation.body["plan_ref"])?;
    let delegation = db::resolve(conn, &json!(spec.delegation_ref))?;
    let target = db::resolve(conn, &json!(spec.target_ref))?;
    let input = db::resolve(conn, &task.spec["input_ref"])?;
    let digest = task.spec["request_digest"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "MCP task request digest unavailable"))?;
    let receipt: Value = conn.query_row(
        "SELECT receipt FROM submissions
        WHERE owner=?1 AND origin=?2 AND key=?3 AND task=?4 AND digest=?5 AND scope=?3",
        params![
            saved.owner,
            origin(&saved.owner, &saved.submission_key),
            saved.submission_key,
            task.id,
            digest
        ],
        |row| row.get(0),
    )?;
    let submitted_ms = acceptance_time(&receipt)?;
    let key_valid = saved
        .submission_key
        .strip_prefix("mcp-call-")
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .is_some_and(|decoded| decoded.len() == 32);
    require(
        valid_handle(&saved.handle)
            && valid_label(&saved.endpoint, 1)
            && key_valid
            && !saved.incarnation.is_empty()
            && !saved.boot.is_empty()
            && !saved.time_ns.is_empty()
            && saved.boot_ms >= 0
            && (1000..=600000).contains(&saved.ttl_ms)
            && saved.utc_ms == submitted_ms
            && spec.authority == authority
            && spec.adapter == ADAPTER
            && spec.protocol == PROTOCOL
            && task.kind == "Task"
            && task.rev == 1
            && task.owner == saved.owner
            && task.body["lifecycle"] == "Queued"
            && task.spec["task_type"] == spec.tool.name()
            && task.spec["delegation_ref"] == json!(spec.delegation_ref)
            && task.body["operation_refs"] == json!([spec.operation_ref])
            && operation.kind == "Operation"
            && operation.rev == 1
            && operation.owner == saved.owner
            && operation.spec["task_id"] == task.id
            && plan.kind == "OperationPlan"
            && plan.owner == saved.owner
            && plan.body["allowed_effects"][0]
                == json!({"action":spec.tool.name(),"resource_ref":spec.target_ref,"scope_ref":spec.delegation_ref})
            && plan.body["purpose_ref"] == task.spec["input_ref"]
            && input.kind == "Input"
            && input.owner == saved.owner
            && spec
                .tool
                .task_input(Some(&spec.target_ref))
                .is_some_and(|value| value == input.body)
            && spec
                .tool
                .target()
                .is_none_or(|(id, kind)| target.id == id && target.kind == kind)
            && (!spec
                .tool
                .unbound_tool()
                .is_some_and(psios_api::LocalReadTool::requires_node_scope)
                || target.kind == "Node" && target.id == authority)
            && delegation.owner == saved.owner
            && delegation.kind == "Delegation"
            && receipt["task_revision"] == spec.task_ref.version
            && receipt["idempotency_key"] == saved.submission_key,
        "RecoveryRequired",
        "invalid MCP task binding",
    )
}

fn acceptance_time(receipt: &Value) -> Result<i64> {
    receipt["accepted_at"]
        .as_str()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.timestamp_millis())
        .ok_or_else(|| err("RecoveryRequired", "invalid MCP task acceptance time"))
}
