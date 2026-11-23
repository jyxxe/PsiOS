use crate::{
    db::{Db, Doc, credential, row},
    *,
};
use psios_api::{VERSION, canonical, id, sha256};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
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
    "node.inventory",
    "node.processes",
    "node.devices",
    "service.inspect",
    "service.start",
    "service.stop",
    "volume.files",
    "volume.mkdir",
    "volume.read_chunk",
    "volume.append",
    "volume.remove_entry",
    "application.stage_release",
    "application.select_release",
    "application.refresh",
    "secret.mac",
    "volume.create",
    "volume.activate",
    "volume.read",
    "volume.write",
    "volume.delete",
    "backup.create",
    "backup.delete",
    "backup.restore",
    "application.install",
    "application.start",
    "application.stop",
    "application.uninstall",
    "application.health",
    "application.logs",
];
pub const COMMON: &[&str] = &[
    "task.submit",
    "task.read",
    "resource.read",
    "event.read",
    "secret.manage",
    "session.run",
    "model.serve",
    "mail.send",
    "mail.receive",
];
pub fn readonly(tool: &str) -> bool {
    matches!(
        tool,
        "system.status"
            | "node.inventory"
            | "node.processes"
            | "node.devices"
            | "service.inspect"
            | "volume.files"
            | "volume.read_chunk"
            | "application.refresh"
            | "secret.mac"
            | "volume.read"
            | "application.health"
            | "application.logs"
    )
}
pub fn approval(tool: &str) -> bool {
    matches!(
        tool,
        "volume.write"
            | "volume.append"
            | "volume.remove_entry"
            | "volume.mkdir"
            | "application.stage_release"
            | "application.select_release"
            | "service.start"
            | "service.stop"
            | "volume.activate"
            | "volume.delete"
            | "backup.delete"
            | "application.install"
            | "application.start"
            | "application.uninstall"
    )
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
        COMMON.iter().chain(TOOLS).map(|s| s.to_string()).collect(),
        1_000_000,
        after(86400 * 3650),
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
            "availability": if tool.starts_with("application."){"Degraded"}else{"Ready"},
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
        body["actions"] = json!(COMMON.iter().chain(TOOLS).collect::<Vec<_>>());
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
pub fn visible(actor: &str, d: &Doc) -> bool {
    actor == OWNER || d.owner == actor || d.owner == SYSTEM
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
    find_grant(db, actor, action, None)
}
pub fn can_target(db: &Db<'_>, actor: &str, action: &str, target: &str) -> Result<()> {
    find_grant(db, actor, action, Some(target))
}
fn find_grant(db: &Db<'_>, actor: &str, action: &str, target: Option<&str>) -> Result<()> {
    for d in delegations(db, actor)? {
        let check = delegation(db, actor, &d.id, action).and_then(|d| match target {
            Some(resource) => tool_scope(db, &d.reference(), resource),
            None => Ok(()),
        });
        match check {
            Ok(()) => return Ok(()),
            Err(e) if e.0.code == "PermissionDenied" => {}
            Err(e) => return Err(e),
        }
    }
    Err(err(
        "PermissionDenied",
        "action and target require a matching active grant",
    ))
}
fn issue(
    db: &Db<'_>,
    subject: &str,
    delegation_id: &str,
    actions: Vec<String>,
    units: u32,
    until: String,
    allow_delegation: bool,
) -> Result<Value> {
    let budget = db.record(
        "Budget",
        subject,
        json!({"unit": "dispatched_run", "limit": units, "principal": subject}),
    )?;
    db.c.execute(
        "INSERT INTO budgets(id,maximum) VALUES(?1,?2)",
        params![budget.id, units],
    )?;
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
        p.model_calls <= 1_000_000
            && (1..=1_000_000).contains(&p.units)
            && (1..=86400 * 30).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid budget or TTL",
    )?;
    if let Some(ids) = &p.tool_targets {
        require(
            !ids.is_empty() && ids.len() <= 64,
            "InvalidArgument",
            "target scope must contain 1 to 64 resource IDs",
        )?;
        for id in ids {
            db.get(id)?;
        }
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
        p.units,
        after(p.ttl_seconds.into()),
        p.allow_delegation,
    )?;
    db.c.execute(
        "INSERT INTO model_budgets(budget,maximum) VALUES(?1,?2)",
        params![
            issued["budget_ref"]["resource_id"].as_str().unwrap(),
            p.model_calls
        ],
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
    require(
        g.owner != OWNER,
        "PermissionDenied",
        "bootstrap grant is reserved",
    )?;
    let mut b = g.body.clone();
    b["lifecycle"] = json!("Revoked");
    let revoked = db.status(&g, b)?;
    let generation = epoch(db)?;
    let flight:bool=db.c.query_row("SELECT
        EXISTS(SELECT 1 FROM queue WHERE grant_id=?1 AND phase IN ('Dispatched','Unknown'))
        OR EXISTS(SELECT 1 FROM current a JOIN current op ON op.id=a.body->>'start_operation_id'
           JOIN current t ON t.id=op.spec->>'task_id' JOIN current d ON d.id=t.spec->'delegation_ref'->>'resource_id'
           WHERE (a.kind='HostService' OR (a.kind='Application' AND a.body->>'lifecycle'='Running')) AND d.body->>'grant_id'=?1)
        OR EXISTS(SELECT 1 FROM session_turns st JOIN current s ON s.id=st.session
           JOIN current d ON d.id=s.spec->>'delegation_id' JOIN current m ON m.id=s.spec->'model_ref'->>'resource_id'
           WHERE st.receipt IS NULL AND julianday(st.expires)>julianday('now')
           AND (d.body->>'grant_id'=?1 OR (?3 AND m.spec->>'provider_principal_id'=?2)))",
        params![g.id,g.owner,g.spec["actions"].as_array().unwrap().iter().any(|a|a=="model.serve")],|r|r.get(0))?;
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
    Ok(db.c.query_row("SELECT b.maximum,b.reserved,b.spent,coalesce(m.maximum,0),coalesce(m.used,0)
        FROM budgets b LEFT JOIN model_budgets m ON m.budget=b.id WHERE b.id=?1",[&b.id],|r|{
        Ok(json!({"maximum":r.get::<_,u32>(0)?,"reserved":r.get::<_,u32>(1)?,"spent":r.get::<_,u32>(2)?,
            "model_calls_maximum":r.get::<_,u32>(3)?,"model_calls_used":r.get::<_,u32>(4)?}))})?)
}

pub fn intern_input(db: &Db<'_>, actor: &str, input: &Value) -> Result<Doc> {
    let digest = sha256(&canonical(&json!([actor, input])));
    let id = format!("input-{}", &digest[7..]);
    match db.get(&id) {
        Ok(d) => Ok(d),
        Err(e) if e.0.code == "NotFound" => {
            let (count,size):(i64,i64)=db.c.query_row("SELECT count(*),coalesce(sum(length(CAST(body AS BLOB))),0) FROM versions WHERE kind='Input' AND owner=?1",[actor],|r|Ok((r.get(0)?,r.get(1)?)))?;
            require(
                count < 4096 && size + canonical(input).len() as i64 <= 64 * 1024 * 1024,
                "QuotaExceeded",
                "retained input quota exceeded",
            )?;
            db.create("Input", &id, actor, json!({}), input.clone())
        }
        Err(e) => Err(e),
    }
}
