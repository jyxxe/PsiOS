use crate::{
    db::{Db, Doc, credential, row},
    *,
};
use psios_api::{VERSION, canonical, id, sha256};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
pub(crate) mod deadline;
mod delegations;
mod secrets;
pub use delegations::{delegate, delegation, revoke_delegation, tool_scope};
pub use secrets::{mac, secret_put, secret_revoke, secret_rotate};

pub const OWNER: &str = "local-owner";
pub const SYSTEM: &str = "system";
pub const RUNTIME_PROFILE: &str = "runtime-local-v2";
pub const PROFILE: &str = "profile-local";
pub const TOOLS: &[&str] = &[
    "system.status",
    agent::START_SESSION,
    workflow::START_TOOL,
    project::agent_tool::TOOL,
    project::assigned::TOOL,
    "node.inventory",
    node::pressure::TOOL,
    "node.interfaces",
    "node.routes",
    "node.route_lookup",
    node::network::probe::TOOL,
    node::network::probe::state::TOOL,
    node::network::trial::TOOL,
    node::network::trial::INSPECT,
    "node.routing_rules",
    "node.processes",
    node::process::TOOL,
    "node.devices",
    node::block::TOOL,
    "node.mounts",
    "service.inspect",
    node::service_state::TOOL,
    node::journal::TOOL,
    node::journal::SINCE_TOOL,
    "service.start",
    "service.stop",
    node::RESTART_TOOL,
    "volume.files",
    "volume.usage",
    storage::file_integrity::TOOL,
    storage::file_move::TOOL,
    storage::file_move::INSPECT,
    "volume.mkdir",
    "volume.read_chunk",
    "volume.append",
    "volume.remove_entry",
    "application.stage_release",
    apps::inventory::TOOL,
    apps::artifact_install::STAGE_TOOL,
    "application.select_release",
    apps::release_retirement::TOOL,
    "application.refresh",
    "secret.mac",
    "volume.create",
    "volume.activate",
    "volume.read",
    "volume.write",
    "volume.delete",
    "backup.create",
    storage::publication::INSPECT,
    "backup.verify",
    "backup.delete",
    "backup.restore",
    storage::restore_publication::INSPECT,
    "application.install",
    apps::artifact_install::TOOL,
    apps::capture::TOOL,
    apps::restore_image::TOOL,
    apps::restore_image::INSPECT,
    artifact::session_output::TOOL,
    "application.start",
    "application.stop",
    apps::stop::TOOL,
    apps::restart::TOOL,
    apps::restart::INSPECT,
    "application.uninstall",
    "application.health",
    "application.logs",
];
pub(crate) const PROJECT_READ: &str = "project.read";
pub(crate) const PROJECT_MANAGE: &str = "project.manage";
pub(crate) const PROJECT_CONTRIBUTE: &str = "project.contribute";
pub(crate) const PROJECT_REVIEW: &str = "project.review";

pub const COMMON: &[&str] = &[
    "task.submit",
    "task.read",
    broker::result_access::ACTION,
    schedule::ACTION,
    workflow::ACTION,
    chat::INVITE,
    chat::READ,
    chat::SEND,
    PROJECT_READ,
    PROJECT_MANAGE,
    PROJECT_CONTRIBUTE,
    PROJECT_REVIEW,
    "resource.read",
    artifact::ingress::UPLOAD,
    artifact::ingress::READ,
    artifact::ingress::WITHDRAW,
    storage::export::ACTION,
    "event.read",
    "secret.manage",
    "session.run",
    agent::fallback::ACTION,
    "model.serve",
    "mail.send",
    "mail.receive",
];
pub fn readonly(tool: &str) -> bool {
    matches!(
        tool,
        "system.status"
            | project::assigned::TOOL
            | "node.inventory"
            | node::pressure::TOOL
            | "node.interfaces"
            | "node.routes"
            | "node.route_lookup"
            | node::network::probe::TOOL
            | node::network::probe::state::TOOL
            | node::network::trial::INSPECT
            | "node.routing_rules"
            | "node.processes"
            | node::process::TOOL
            | "node.devices"
            | node::block::TOOL
            | "node.mounts"
            | "service.inspect"
            | node::service_state::TOOL
            | node::journal::TOOL
            | node::journal::SINCE_TOOL
            | "volume.files"
            | "volume.usage"
            | storage::file_integrity::TOOL
            | storage::file_move::INSPECT
            | "volume.read_chunk"
            | "application.refresh"
            | apps::inventory::TOOL
            | "secret.mac"
            | "backup.verify"
            | storage::publication::INSPECT
            | storage::restore_publication::INSPECT
            | "volume.read"
            | "application.health"
            | apps::restart::INSPECT
            | apps::restore_image::INSPECT
            | "application.logs"
    )
}
pub fn approval(tool: &str) -> bool {
    matches!(
        tool,
        "volume.write"
            | "volume.append"
            | "volume.remove_entry"
            | storage::file_move::TOOL
            | "volume.mkdir"
            | "application.stage_release"
            | apps::artifact_install::STAGE_TOOL
            | "application.select_release"
            | apps::release_retirement::TOOL
            | "service.start"
            | "service.stop"
            | node::RESTART_TOOL
            | node::network::trial::TOOL
            | "volume.activate"
            | "volume.delete"
            | "backup.delete"
            | "application.install"
            | apps::artifact_install::TOOL
            | apps::capture::TOOL
            | apps::restore_image::TOOL
            | "application.start"
            | apps::restart::TOOL
            | apps::stop::TOOL
            | "application.uninstall"
    )
}

// New inspection scopes require explicit grants, including for the owner CLI.
fn bootstrap_actions() -> Vec<String> {
    COMMON
        .iter()
        .chain(TOOLS)
        .filter(|a| {
            !matches!(
                **a,
                "node.interfaces"
                    | "node.routes"
                    | "node.route_lookup"
                    | node::network::probe::TOOL
                    | node::network::probe::state::TOOL
                    | node::network::trial::TOOL
                    | node::network::trial::INSPECT
                    | node::journal::TOOL
                    | node::journal::SINCE_TOOL
                    | node::service_state::TOOL
                    | node::RESTART_TOOL
                    | "node.routing_rules"
                    | "node.mounts"
                    | node::pressure::TOOL
                    | node::process::TOOL
                    | artifact::ingress::UPLOAD
                    | artifact::ingress::READ
                    | artifact::ingress::WITHDRAW
                    | artifact::session_output::TOOL
                    | apps::artifact_install::TOOL
                    | apps::capture::TOOL
                    | apps::restore_image::TOOL
                    | apps::restore_image::INSPECT
                    | apps::artifact_install::STAGE_TOOL
                    | apps::inventory::TOOL
                    | apps::release_retirement::TOOL
                    | node::block::TOOL
                    | "backup.verify"
                    | storage::file_integrity::TOOL
                    | storage::file_move::TOOL
                    | storage::file_move::INSPECT
                    | storage::export::ACTION
                    | storage::publication::INSPECT
                    | storage::restore_publication::INSPECT
                    | apps::restart::TOOL
                    | apps::stop::TOOL
                    | apps::restart::INSPECT
                    | broker::result_access::ACTION
                    | agent::START_SESSION
                    | agent::fallback::ACTION
                    | workflow::START_TOOL
                    | schedule::ACTION
                    | workflow::ACTION
                    | chat::INVITE
                    | chat::READ
                    | chat::SEND
                    | PROJECT_READ
                    | PROJECT_MANAGE
                    | PROJECT_CONTRIBUTE
                    | PROJECT_REVIEW
                    | project::agent_tool::TOOL
                    | project::assigned::TOOL
            )
        })
        .map(|a| a.to_string())
        .collect()
}
pub fn bootstrap(db: &Db<'_>, token: &str) -> Result<()> {
    let principal =
        json!({"principal_type": "Human", "name": "Local owner", "lifecycle": "Active"});
    db.create("Principal", OWNER, OWNER, principal.clone(), principal)?;
    db.c.execute(
        "INSERT INTO credentials VALUES(?1,?2)",
        params![sha256(token.as_bytes()), OWNER],
    )?;
    let profile = json!({
        "transport": "psios.local.v1",
        "fault_scope": ["ProcessCrash"],
        "network": "none",
        "storage": "private-directory",
        "writer": "single-local-worker",
        "queue_limit": db.cfg.queue_limit,
        "approval_policy": "single-use local owner decision",
        "unknown_effect": "hold for reconciliation"
    });
    let profile = db
        .create("Profile", PROFILE, SYSTEM, profile.clone(), profile)?
        .reference();
    let pool = json!({
        "pool_id": "local",
        "allocation_enabled": true,
        "backend": "directory",
        "failure_domain": "local-filesystem",
        "quota": "Unsupported",
        "health": "Local"
    });
    db.create("StoragePool", "local", SYSTEM, pool.clone(), pool)?;
    apps::profiles(db)?;
    install_tools(db, &profile)?;
    issue(
        db,
        OWNER,
        "owner-delegation",
        bootstrap_actions(),
        DispatchBudget {
            units: 1_000_000,
            task_concurrency: None,
        },
        86400 * 3650,
        true,
    )?;
    db.audit(OWNER, "Core.Init", None, "Allowed")
}
fn install_tools(db: &Db<'_>, profile: &Value) -> Result<()> {
    for tool in TOOLS {
        let exists: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE id=?1)",
            [format!("tool-{tool}")],
            |r| r.get(0),
        )?;
        if exists {
            continue;
        }
        let body = json!({
            "schema_version": VERSION,
            "capability_id": tool,
            "capability_version": VERSION,
            "authority_id": db.meta("authority")?,
            "actions": [tool],
            "profile_ref": profile,
            "revocation_capabilities": ["GateOnly"],
            "effect_limitations": [if readonly(tool){"Returned bytes cannot be recalled"}else{"An already dispatched effect requires a receipt"}],
            "availability": if tool.starts_with("application.") && *tool != apps::inventory::TOOL {"Degraded"}else{"Ready"},
            "implementation_status": "ImplementedUnverified"
        });
        db.create(
            "Capability",
            &format!("tool-{tool}"),
            SYSTEM,
            json!({
                "effect_class": if readonly(tool){"ReadOnly"}else{"NonRetryableEffect"},
                "requires_approval": approval(tool),
                "result_query": "Task.Get"
            }),
            body,
        )?;
    }
    Ok(())
}
pub fn upgrade_tool_catalog(db: &Db<'_>, version: u32) -> Result<()> {
    install_tools(db, &db.get(RUNTIME_PROFILE)?.reference())?;
    db.c.pragma_update(None, "user_version", version)?;
    db.audit(SYSTEM, "Core.Upgrade", None, &format!("Version{version}"))
}
pub fn upgrade(db: &Db<'_>) -> Result<()> {
    let profile = db.get(PROFILE)?;
    let mut profile_body = profile.body.clone();
    profile_body["writer"] =
        json!("single transactional authority with bounded parallel host workers");
    profile_body["worker_limit"] = json!(16);
    let profile = db.create(
        "Profile",
        RUNTIME_PROFILE,
        SYSTEM,
        profile_body.clone(),
        profile_body,
    )?;
    install_tools(db, &profile.reference())?;
    let d = db.of("owner-delegation", "Delegation")?;
    let g = db.of(d.body["grant_id"].as_str().unwrap(), "Grant")?;
    for doc in [&g, &d] {
        let mut body = doc.body.clone();
        body["actions"] = json!(bootstrap_actions());
        db.update(doc, body.clone(), body)?;
    }
    db.c.execute(
        "INSERT INTO model_budgets(budget,maximum) VALUES(?1,1000000)",
        [d.body["budget_ref"]["resource_id"].as_str().unwrap()],
    )?;
    epoch(db)?;
    Ok(())
}
pub fn authenticate(db: &Db<'_>, token: &str) -> Result<String> {
    require(
        token.len() == 64,
        "AuthenticationRequired",
        "valid local credential required",
    )?;
    db.c.query_row(
        "SELECT principal FROM credentials WHERE digest=?1",
        [sha256(token.as_bytes())],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| err("AuthenticationRequired", "valid local credential required"))
}
pub fn admin(actor: &str) -> Result<()> {
    require(
        actor == OWNER,
        "PermissionDenied",
        "local owner authorization required",
    )
}
pub(crate) const PRIVATE_RESOURCE_KINDS: &[&str] = &[
    apps::log_tail::PRIVATE_KIND,
    "Project",
    "Milestone",
    "WorkItem",
    "WorkItemEvidence",
    crate::artifact::retention::delayed::KIND,
];

pub fn visible(actor: &str, d: &Doc) -> bool {
    !PRIVATE_RESOURCE_KINDS.contains(&d.kind.as_str())
        && (actor == OWNER || d.owner == actor || d.owner == SYSTEM)
}
pub fn owned(actor: &str, d: &Doc) -> Result<()> {
    require(
        actor == d.owner || actor == OWNER,
        "PermissionDenied",
        "resource outside authorized scope",
    )
}
fn epoch(db: &Db<'_>) -> Result<String> {
    db.c.execute(
        "UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='epoch'",
        [],
    )?;
    db.meta("epoch")
}
fn delegations(db: &Db<'_>, actor: &str) -> Result<Vec<Doc>> {
    Ok(db
        .c
        .prepare("SELECT * FROM current WHERE kind='Delegation' AND owner=?1 ORDER BY id")?
        .query_map([actor], row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn can(db: &Db<'_>, actor: &str, action: &str) -> Result<()> {
    authorized_delegation(db, actor, action, None).map(|_| ())
}
pub fn can_target(db: &Db<'_>, actor: &str, action: &str, target: &str) -> Result<()> {
    authorized_delegation(db, actor, action, Some(target)).map(|_| ())
}
pub(crate) fn authorized_delegation(
    db: &Db<'_>,
    actor: &str,
    action: &str,
    target: Option<&str>,
) -> Result<Doc> {
    for d in delegations(db, actor)? {
        let check = delegation(db, actor, &d.id, action).and_then(|d| {
            if let Some(resource) = target {
                tool_scope(db, &d.reference(), resource)?;
            }
            Ok(d)
        });
        match check {
            Ok(d) => return Ok(d),
            Err(e) if e.0.code == "PermissionDenied" => {}
            Err(e) => return Err(e),
        }
    }
    Err(err(
        "PermissionDenied",
        "action and target require a matching active grant",
    ))
}
struct DispatchBudget {
    units: u32,
    task_concurrency: Option<u32>,
}

fn issue(
    db: &Db<'_>,
    subject: &str,
    delegation_id: &str,
    actions: Vec<String>,
    limits: DispatchBudget,
    ttl_seconds: u32,
    allow_delegation: bool,
) -> Result<Value> {
    let clock = deadline::Clock::sample()?;
    let until = after(ttl_seconds.into());
    let mut spec = json!({"unit": "dispatched_run", "limit": limits.units, "principal": subject});
    if let Some(maximum) = limits.task_concurrency {
        spec["task_concurrency"] = json!(maximum);
    }
    let budget = db.record("Budget", subject, spec)?;
    db.c.execute(
        "INSERT INTO budgets(id,maximum) VALUES(?1,?2)",
        params![budget.id, limits.units],
    )?;
    if let Some(maximum) = limits.task_concurrency {
        broker::concurrency::register(db, &budget.id, maximum)?;
    }
    let grant = db.record(
        "Grant",
        subject,
        json!({
            "issuer": OWNER,
            "subject": subject,
            "actions": actions,
            "scope": "owned",
            "valid_until": until,
            "lifecycle": "Active",
            "budget_ref": budget.reference()
        }),
    )?;
    clock.register(db, &grant.id, ttl_seconds)?;
    let body = json!({
        "grant_id": grant.id,
        "issuer": OWNER,
        "subject": subject,
        "actions": actions,
        "budget_ref": budget.reference(),
        "redelegation": allow_delegation,
        "valid_until": until,
        "lifecycle": "Active"
    });
    let d = db.create("Delegation", delegation_id, subject, body.clone(), body)?;
    clock.register(db, &d.id, ttl_seconds)?;
    epoch(db)?;
    Ok(json!({
        "grant_ref": grant.reference(),
        "delegation_ref": d.reference(),
        "budget_ref": budget.reference()
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    name: String,
    peer_uid: Option<u32>,
}
pub fn register(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    admin(actor)?;
    let p: Register = decode(p)?;
    label(&p.name)?;
    let principal = id("principal");
    let token = credential();
    let body = json!({"principal_type": "Agent", "name": p.name, "peer_uid": p.peer_uid, "lifecycle": "Active"});
    db.create("Principal", &principal, &principal, body.clone(), body)?;
    let agent = db.record(
        "Agent",
        &principal,
        json!({"name": p.name, "principal_id": principal, "interface": "psios.local.v1"}),
    )?;
    db.c.execute(
        "INSERT INTO credentials VALUES(?1,?2)",
        params![sha256(token.as_bytes()), principal],
    )?;
    Ok(json!({"agent_ref": agent.reference(), "principal_id": principal, "token": token}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantIssue {
    principal_id: String,
    actions: Vec<String>,
    units: u32,
    #[serde(default)]
    model_calls: u32,
    model_lease_ms: Option<u32>,
    task_concurrency: Option<u32>,
    max_delegation_depth: Option<u32>,
    tool_targets: Option<Vec<String>>,
    ttl_seconds: u32,
    #[serde(default)]
    allow_delegation: bool,
}
pub fn grant(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    admin(actor)?;
    let p: GrantIssue = decode(p)?;
    let principal = db.of(&p.principal_id, "Principal")?;
    require(
        principal.body["lifecycle"] == "Active",
        "PermissionDenied",
        "principal disabled",
    )?;
    require(
        !p.actions.is_empty()
            && p.actions.len() <= 64
            && p.actions
                .iter()
                .all(|a| COMMON.contains(&a.as_str()) || TOOLS.contains(&a.as_str())),
        "UnsupportedCapability",
        "unregistered grant action",
    )?;
    require(
        principal.id == OWNER
            || p.actions.iter().all(|action| {
                !matches!(
                    action.as_str(),
                    apps::capture::TOOL
                        | apps::restore_image::TOOL
                        | apps::restore_image::INSPECT
                        | apps::stop::TOOL
                )
            }),
        "PermissionDenied",
        "OCI image custody and stop recovery require the local owner",
    )?;
    require(
        p.model_calls <= 1_000_000
            && p.model_lease_ms
                .is_none_or(|limit| limit <= agent::model_time::MAX_BUDGET_MS)
            && p.task_concurrency.is_none_or(|limit| {
                (1..=broker::concurrency::MAX_TASKS_PER_BUDGET).contains(&limit)
            })
            && (1..=1_000_000).contains(&p.units)
            && (1..=86400 * 30).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid budget or TTL",
    )?;
    let reads_results = p
        .actions
        .iter()
        .any(|action| action == broker::result_access::ACTION);
    let has_project_action = p.actions.iter().any(|action| {
        matches!(
            action.as_str(),
            PROJECT_READ | PROJECT_MANAGE | PROJECT_CONTRIBUTE | PROJECT_REVIEW
        )
    });
    let submits_evidence = p
        .actions
        .iter()
        .any(|action| action == project::agent_tool::TOOL);
    let discovers_work = p
        .actions
        .iter()
        .any(|action| action == project::assigned::TOOL);
    require(
        !has_project_action || (!reads_results && p.tool_targets.is_some()),
        "InvalidArgument",
        "project actions require exact Project targets and a separate Task result grant",
    )?;
    require(
        !(reads_results
            || submits_evidence
            || discovers_work
            || p.actions.iter().any(|action| {
                action == storage::export::ACTION
                    || action == artifact::ingress::UPLOAD
                    || action == artifact::ingress::READ
                    || action == artifact::ingress::WITHDRAW
                    || action == artifact::session_output::TOOL
                    || action == apps::artifact_install::TOOL
                    || action == apps::capture::TOOL
                    || action == apps::restore_image::TOOL
                    || action == apps::restore_image::INSPECT
                    || action == apps::artifact_install::STAGE_TOOL
                    || action == apps::inventory::TOOL
                    || action == apps::release_retirement::TOOL
                    || action == node::journal::TOOL
                    || action == node::journal::SINCE_TOOL
                    || action == node::process::TOOL
                    || action == node::RESTART_TOOL
                    || action == node::service_state::TOOL
                    || action == node::network::probe::state::TOOL
                    || action == storage::restore_publication::INSPECT
                    || action == storage::file_integrity::TOOL
                    || action == storage::file_move::TOOL
                    || action == storage::file_move::INSPECT
                    || action == apps::restart::TOOL
                    || action == apps::stop::TOOL
                    || action == apps::restart::INSPECT
                    || action == agent::fallback::ACTION
            }))
            || p.tool_targets.is_some(),
        "InvalidArgument",
        "this grant action requires an explicit target scope",
    )?;
    if let Some(ids) = &p.tool_targets {
        require(
            !ids.is_empty() && ids.len() <= 64,
            "InvalidArgument",
            "target scope must contain 1 to 64 resource IDs",
        )?;
        let mut includes_work_item = false;
        let mut includes_project = false;
        for id in ids {
            if reads_results {
                db.of(id, "Task")?;
            } else if has_project_action {
                db.of(id, "Project")?;
                includes_project = true;
            } else {
                let target = db.get(id)?;
                includes_work_item |= target.kind == "WorkItem";
                includes_project |= target.kind == "Project";
            }
        }
        require(
            !submits_evidence || includes_work_item,
            "InvalidArgument",
            "evidence submission requires an exact WorkItem in the grant target scope",
        )?;
        require(
            !discovers_work || includes_project,
            "InvalidArgument",
            "assigned work discovery requires an exact Project in the grant target scope",
        )?;
    }
    require(
        p.max_delegation_depth
            .is_none_or(|n| p.allow_delegation && (1..=4).contains(&n)),
        "InvalidArgument",
        "invalid opt-in delegation depth",
    )?;
    let mut issued = issue(
        db,
        &p.principal_id,
        &id("delegation"),
        p.actions,
        DispatchBudget {
            units: p.units,
            task_concurrency: p.task_concurrency,
        },
        p.ttl_seconds,
        p.allow_delegation,
    )?;
    let budget = issued["budget_ref"]["resource_id"].as_str().unwrap();
    db.c.execute(
        "INSERT INTO model_budgets(budget,maximum,maximum_lease_ms) VALUES(?1,?2,?3)",
        params![budget, p.model_calls, p.model_lease_ms,],
    )?;
    if p.tool_targets.is_some() || p.max_delegation_depth.is_some() {
        let g = db.resolve(&issued["grant_ref"])?;
        let mut b = g.body.clone();
        if let Some(ids) = p.tool_targets {
            b["tool_targets"] = json!(ids);
        }
        if let Some(n) = p.max_delegation_depth {
            b["max_delegation_depth"] = json!(n);
        }
        issued["grant_ref"] = db.update(&g, b.clone(), b)?.reference();
    }
    Ok(issued)
}
pub fn revoke(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    admin(actor)?;
    let p: Conditional = decode(p)?;
    let g = db.of(&p.id, "Grant")?;
    g.cas(&p.expected_resource_version)?;
    let bootstrap = db.of("owner-delegation", "Delegation")?;
    require(
        bootstrap.body["grant_id"] != g.id,
        "PermissionDenied",
        "bootstrap grant is reserved",
    )?;
    let mut b = g.body.clone();
    b["lifecycle"] = json!("Revoked");
    let revoked = db.status(&g, b)?;
    let generation = epoch(db)?;
    let host_flight: bool=db.c.query_row("SELECT
        EXISTS(SELECT 1 FROM queue WHERE grant_id=?1 AND phase IN ('Dispatched','WaitingInput','Unknown'))
        OR EXISTS(SELECT 1 FROM task_dependencies input JOIN queue q ON q.task=input.task
           JOIN current d ON d.id=input.read_delegation WHERE q.phase IN ('Dispatched','Unknown') AND d.body->>'grant_id'=?1)
        OR EXISTS(SELECT 1 FROM current a JOIN current op ON op.id=a.body->>'start_operation_id'
           JOIN current t ON t.id=op.spec->>'task_id' JOIN current d ON d.id=t.spec->'delegation_ref'->>'resource_id'
           WHERE (a.kind='HostService' OR (a.kind='Application' AND a.body->>'lifecycle'='Running')) AND d.body->>'grant_id'=?1)",
        [&g.id], |r| r.get(0))?;
    let flight = host_flight || agent::pending_for_grant(db, &g)?;
    let rev = id("revocation");
    let body = json!({
        "schema_version": VERSION,
        "revocation_id": rev,
        "accepted_at": now(),
        "authorization_epoch": generation,
        "status": if flight{"Partial"}else{"Complete"},
        "targets": [{"resource_id":g.id,"required":true,"capabilities":["GateOnly"],"enforcement_status":"Enforced",
        "evidence_refs":[revoked.reference()],"residual_effects":if flight{vec!["An issued effect or running workload remains outside the admission gate"]}else{vec![]}}]
    });
    Ok(db
        .create(
            "Revocation",
            &rev,
            &g.owner,
            json!({"grant_id": g.id}),
            body,
        )?
        .body)
}
pub fn disable(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    admin(actor)?;
    let p: Conditional = decode(p)?;
    let d = db.of(&p.id, "Principal")?;
    d.cas(&p.expected_resource_version)?;
    require(
        d.id != OWNER,
        "PermissionDenied",
        "bootstrap principal is reserved",
    )?;
    let mut body = d.body.clone();
    body["lifecycle"] = json!("Revoked");
    db.c.execute("DELETE FROM credentials WHERE principal=?1", [&d.id])?;
    chat::principal_revoked(db, &d.id)?;
    epoch(db)?;
    Ok(db.status(&d, body)?.reference())
}
pub fn active(db: &Db<'_>, actor: &str) -> Result<()> {
    require(
        db.of(actor, "Principal")?.body["lifecycle"] == "Active",
        "PermissionDenied",
        "principal revoked",
    )
}
pub fn discover(db: &Db<'_>, actor: &str) -> Result<Value> {
    let mut out = Vec::new();
    for tool in TOOLS {
        match can(db, actor, tool) {
            Ok(()) => out.push(db.get(&format!("tool-{tool}"))?.body),
            Err(e) if e.0.code == "PermissionDenied" => {}
            Err(e) => return Err(e),
        }
    }
    Ok(json!(out))
}
pub fn who(db: &Db<'_>, actor: &str) -> Result<Value> {
    Ok(json!({
        "principal_id": actor,
        "authority_id": db.meta("authority")?,
        "authorization_epoch": db.meta("epoch")?,
        "delegations": delegations(db,actor)?.iter().map(Doc::reference).collect::<Vec<_>>()
    }))
}
pub fn budget(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: ById = decode(p)?;
    let b = db.of(&p.id, "Budget")?;
    owned(actor, &b)?;
    let mut budget = db.c.query_row("SELECT b.maximum,b.reserved,b.spent,coalesce(m.maximum,0),coalesce(m.used,0),m.maximum_lease_ms,m.charged_lease_ms
        FROM budgets b LEFT JOIN model_budgets m ON m.budget=b.id WHERE b.id=?1",[&b.id],|r|{
        let mut budget = json!({"maximum":r.get::<_,u32>(0)?,"reserved":r.get::<_,u32>(1)?,"spent":r.get::<_,u32>(2)?,
            "model_calls_maximum":r.get::<_,u32>(3)?,"model_calls_used":r.get::<_,u32>(4)?});
        if let Some(limit) = r.get::<_,Option<u32>>(5)? {
            let charged: u32 = r.get(6)?;
            budget["model_lease_ms_maximum"] = json!(limit);
            budget["model_lease_ms_charged"] = json!(charged);
            budget["model_lease_ms_remaining"] = json!(limit - charged);
        }
        Ok(budget)
    })?;
    if let Some(slots) = broker::concurrency::usage(db, &b.id)? {
        budget["task_concurrency_maximum"] = json!(slots.maximum);
        budget["task_concurrency_occupied"] = json!(slots.occupied);
        budget["task_concurrency_unknown"] = json!(slots.unknown);
        budget["task_concurrency_remaining"] = json!(slots.maximum.saturating_sub(slots.occupied));
    }
    Ok(budget)
}

pub fn intern_input(db: &Db<'_>, actor: &str, input: &Value) -> Result<Doc> {
    intern_input_kind(db, actor, "Input", input)
}

pub(crate) fn intern_input_kind(
    db: &Db<'_>,
    actor: &str,
    kind: &str,
    input: &Value,
) -> Result<Doc> {
    let material = if kind == "Input" {
        json!([actor, input])
    } else {
        json!([actor, kind, input])
    };
    let digest = sha256(&canonical(&material));
    let id = format!("input-{}", &digest[7..]);
    match db.get(&id) {
        Ok(d) => Ok(d),
        Err(e) if e.0.code == "NotFound" => {
            db.cfg.check_admission_space()?;
            let (count,size):(i64,i64)=db.c.query_row("SELECT count(*),coalesce(sum(length(CAST(body AS BLOB))),0) FROM versions WHERE kind IN ('Input','TaskInputManifest') AND owner=?1",[actor],|r|Ok((r.get(0)?,r.get(1)?)))?;
            require(
                count < 4096 && size + canonical(input).len() as i64 <= 64 * 1024 * 1024,
                "QuotaExceeded",
                "retained input quota exceeded",
            )?;
            db.create(kind, &id, actor, json!({}), input.clone())
        }
        Err(e) => Err(e),
    }
}
