use crate::{
    db::{Db, Doc, credential, row},
    *,
};
use psios_api::{VERSION, canonical, id, sha256};
use ring::{
    aead, hmac,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use zeroize::Zeroizing;

pub const OWNER: &str = "local-owner";
pub const SYSTEM: &str = "system";
pub const PROFILE: &str = "profile-local";
pub const TOOLS: &[&str] = &[
    "system.status",
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
];
pub fn readonly(tool: &str) -> bool {
    matches!(
        tool,
        "system.status" | "secret.mac" | "volume.read" | "application.health" | "application.logs"
    )
}
pub fn approval(tool: &str) -> bool {
    matches!(
        tool,
        "volume.write"
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
    for tool in TOOLS {
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
pub fn delegation(db: &Db<'_>, actor: &str, id: &str, action: &str) -> Result<Doc> {
    let d = db.of(id, "Delegation")?;
    require(
        d.owner == actor,
        "PermissionDenied",
        "delegation audience mismatch",
    )?;
    require(
        d.body["lifecycle"] == "Active"
            && !expired(d.body["valid_until"].as_str().unwrap())?
            && d.body["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == action),
        "PermissionDenied",
        "delegation expired, revoked or action outside delegated scope",
    )?;
    if let Some(parent) = d.body.get("parent_ref") {
        let version = db.resolve(parent)?;
        let current = db.get(&version.id)?;
        active(db, &current.owner)?;
        require(
            current.version() == version.version() && current.body["lifecycle"] == "Active",
            "PermissionDenied",
            "parent delegation no longer current",
        )?;
    }
    let g = db.of(d.body["grant_id"].as_str().unwrap(), "Grant")?;
    require(
        g.body["lifecycle"] == "Active"
            && !expired(g.spec["valid_until"].as_str().unwrap())?
            && g.spec["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == action),
        "PermissionDenied",
        "grant expired, revoked or action outside scope",
    )?;
    Ok(d)
}
fn delegations(db: &Db<'_>, actor: &str) -> Result<Vec<Doc>> {
    Ok(db
        .c
        .prepare("SELECT * FROM current WHERE kind='Delegation' AND owner=?1 ORDER BY id")?
        .query_map([actor], row)?
        .collect::<rusqlite::Result<_>>()?)
}
pub fn can(db: &Db<'_>, actor: &str, action: &str) -> Result<()> {
    for d in delegations(db, actor)? {
        match delegation(db, actor, &d.id, action) {
            Ok(_) => return Ok(()),
            Err(e) if e.0.code == "PermissionDenied" => {}
            Err(e) => return Err(e),
        }
    }
    Err(err("PermissionDenied", "action requires an active grant"))
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
        (1..=1_000_000).contains(&p.units) && (1..=86400 * 30).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid budget or TTL",
    )?;
    issue(
        db,
        &p.principal_id,
        &id("delegation"),
        p.actions,
        p.units,
        after(p.ttl_seconds.into()),
        p.allow_delegation,
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    parent_id: String,
    principal_id: String,
    actions: Vec<String>,
    ttl_seconds: u32,
}
pub fn delegate(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Delegate = decode(p)?;
    active(db, &p.principal_id)?;
    require(
        !p.actions.is_empty() && p.actions.len() <= 5 && (1..=86400).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid delegation bounds",
    )?;
    let parent = db.of(&p.parent_id, "Delegation")?;
    require(
        parent.body["redelegation"] == true && parent.owner == actor && p.principal_id != actor,
        "PermissionDenied",
        "delegation cannot be extended",
    )?;
    for action in &p.actions {
        require(
            matches!(
                action.as_str(),
                "system.status" | "task.submit" | "task.read" | "resource.read" | "event.read"
            ),
            "PermissionDenied",
            "child delegation restricted to local status tasks",
        )?;
        delegation(db, actor, &parent.id, action)?;
    }
    let until =
        after(p.ttl_seconds.into()).min(parent.body["valid_until"].as_str().unwrap().to_owned());
    let body = json!({
        "issuer": actor,
        "subject": p.principal_id,
        "parent_ref": parent.reference(),
        "grant_id": parent.body["grant_id"],
        "actions": p.actions,
        "budget_ref": parent.body["budget_ref"],
        "redelegation": false,
        "valid_until": until,
        "lifecycle": "Active"
    });
    let d = db.record("Delegation", &p.principal_id, body)?;
    epoch(db)?;
    Ok(d.reference())
}
pub fn revoke_delegation(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Conditional = decode(p)?;
    let d = db.of(&p.id, "Delegation")?;
    require(
        d.id != "owner-delegation" && (actor == OWNER || d.body["issuer"] == actor),
        "PermissionDenied",
        "delegation issuer required",
    )?;
    d.cas(&p.expected_resource_version)?;
    let mut b = d.body.clone();
    b["lifecycle"] = json!("Revoked");
    epoch(db)?;
    Ok(db.status(&d, b)?.reference())
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
    let flight:bool=db.c.query_row("SELECT EXISTS(SELECT 1 FROM queue WHERE grant_id=?1 AND phase IN ('Dispatched','Unknown')) OR EXISTS(
        SELECT 1 FROM current a JOIN queue q ON q.operation=a.body->>'start_operation_id'
        WHERE a.kind='Application' AND a.body->>'lifecycle'='Running' AND q.grant_id=?1)",[&g.id],|r|r.get(0))?;
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
    Ok(db.c.query_row("SELECT maximum,reserved,spent FROM budgets WHERE id=?1",[&b.id],|r|{
        Ok(json!({"maximum":r.get::<_,u32>(0)?,"reserved":r.get::<_,u32>(1)?,"spent":r.get::<_,u32>(2)?}))})?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretPut {
    name: String,
    value: String,
}
fn vault(db: &Db<'_>) -> Result<aead::LessSafeKey> {
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &db.keys[..32])
            .map_err(|_| err("RecoveryRequired", "vault key unavailable"))?,
    ))
}
pub fn secret_put(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let s: SecretPut = decode(p)?;
    label(&s.name)?;
    require(
        (1..=8192).contains(&s.value.len()),
        "InvalidArgument",
        "secret size outside limit",
    )?;
    let d = db.record(
        "Secret",
        actor,
        json!({"name": s.name, "lifecycle": "Active", "use": "secret.mac"}),
    )?;
    let mut nonce = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| err("InternalError", "entropy unavailable"))?;
    let mut bytes = Zeroizing::new(s.value.into_bytes());
    vault(db)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(d.id.as_bytes()),
            &mut *bytes,
        )
        .map_err(|_| err("InternalError", "vault encryption failed"))?;
    db.c.execute(
        "INSERT INTO secrets VALUES(?1,?2,?3)",
        params![d.id, nonce.as_slice(), bytes.as_slice()],
    )?;
    Ok(d.reference())
}
pub fn secret_revoke(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let p: Conditional = decode(p)?;
    let d = db.of(&p.id, "Secret")?;
    owned(actor, &d)?;
    d.cas(&p.expected_resource_version)?;
    db.c.execute("DELETE FROM secrets WHERE id=?1", [&d.id])?;
    let mut b = d.body.clone();
    b["lifecycle"] = json!("Revoked");
    epoch(db)?;
    Ok(db.status(&d, b)?.reference())
}
pub fn mac(db: &Db<'_>, actor: &str, secret: &str, message: &str) -> Result<Value> {
    let d = db.of(secret, "Secret")?;
    owned(actor, &d)?;
    require(
        d.body["lifecycle"] == "Active",
        "PermissionDenied",
        "secret revoked",
    )?;
    let (n, c): (Vec<u8>, Vec<u8>) = db.c.query_row(
        "SELECT nonce,ciphertext FROM secrets WHERE id=?1",
        [secret],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut bytes = Zeroizing::new(c);
    let nonce = aead::Nonce::try_assume_unique_for_key(&n)
        .map_err(|_| err("RecoveryRequired", "invalid vault nonce"))?;
    let plain = vault(db)?
        .open_in_place(nonce, aead::Aad::from(secret.as_bytes()), &mut bytes)
        .map_err(|_| err("RecoveryRequired", "vault authentication failed"))?;
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, plain),
        message.as_bytes(),
    );
    use base64::Engine;
    Ok(
        json!({"algorithm": "HMAC-SHA256", "tag": base64::engine::general_purpose::STANDARD.encode(tag)}),
    )
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
