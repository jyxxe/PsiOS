use super::*;
fn ancestors(db: &Db<'_>, leaf: &Doc) -> Result<Vec<Doc>> {
    let mut path = Vec::new();
    let mut next = leaf.body.get("parent_ref").cloned();
    while let Some(r) = next {
        require(
            path.len() < 4,
            "PermissionDenied",
            "delegation chain exceeds profile",
        )?;
        let parent = db.of(r["resource_id"].as_str().unwrap(), "Delegation")?;
        require(
            parent.reference() == r && parent.body["grant_id"] == leaf.body["grant_id"],
            "PermissionDenied",
            "parent delegation changed",
        )?;
        next = parent.body.get("parent_ref").cloned();
        path.push(parent);
    }
    Ok(path)
}

pub(crate) fn submission_window(
    db: &Db<'_>,
    leaf: &Doc,
    grant: &Doc,
    clock: &deadline::Clock,
    sampled_utc_ms: i64,
) -> Result<i64> {
    let mut cutoff =
        clock.bounded_utc_ms(db, &grant.id, &grant.spec["valid_until"], sampled_utc_ms)?;
    for delegation in std::iter::once(leaf).chain(ancestors(db, leaf)?.iter()) {
        cutoff = cutoff.min(clock.bounded_utc_ms(
            db,
            &delegation.id,
            &delegation.body["valid_until"],
            sampled_utc_ms,
        )?);
    }
    Ok(cutoff)
}

pub fn delegation(db: &Db<'_>, actor: &str, id: &str, action: &str) -> Result<Doc> {
    let d = db.of(id, "Delegation")?;
    require(
        d.owner == actor,
        "PermissionDenied",
        "delegation audience mismatch",
    )?;
    let clock = deadline::Clock::sample()?;
    require(
        d.body["lifecycle"] == "Active"
            && !clock.expired(db, &d.id, d.body["valid_until"].as_str().unwrap())?
            && d.body["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == action),
        "PermissionDenied",
        "delegation expired, revoked or action outside delegated scope",
    )?;
    for parent in ancestors(db, &d)? {
        active(db, &parent.owner)?;
        require(
            parent.body["lifecycle"] == "Active"
                && !clock.expired(db, &parent.id, parent.body["valid_until"].as_str().unwrap())?
                && parent.body["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a == action),
            "PermissionDenied",
            "an upstream delegation is closed or lacks this action",
        )?;
    }
    let g = db.of(d.body["grant_id"].as_str().unwrap(), "Grant")?;
    require(
        g.body["lifecycle"] == "Active"
            && !clock.expired(db, &g.id, g.spec["valid_until"].as_str().unwrap())?
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
pub fn tool_scope(db: &Db<'_>, reference: &Value, target: &str) -> Result<()> {
    let d = db.resolve(reference)?;
    let g = db.of(
        d.body["grant_id"]
            .as_str()
            .ok_or_else(|| err("PermissionDenied", "missing grant"))?,
        "Grant",
    )?;
    require(
        [&g.spec, &d.body].iter().all(|scope| {
            scope.get("tool_targets").is_none_or(|v| {
                v.as_array()
                    .is_some_and(|ids| ids.iter().any(|i| i == target))
            })
        }),
        "PermissionDenied",
        "tool target outside grant",
    )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    parent_id: String,
    principal_id: String,
    actions: Vec<String>,
    ttl_seconds: u32,
    #[serde(default)]
    allow_delegation: bool,
    tool_targets: Option<Vec<String>>,
}
pub fn delegate(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Delegate = decode(p)?;
    active(db, &p.principal_id)?;
    require(
        !p.actions.is_empty() && p.actions.len() <= 32 && (1..=86400).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid delegation bounds",
    )?;
    let parent = db.of(&p.parent_id, "Delegation")?;
    require(
        parent.body["redelegation"] == true && parent.owner == actor && p.principal_id != actor,
        "PermissionDenied",
        "delegation cannot be extended",
    )?;
    let ancestors = ancestors(db, &parent)?;
    require(
        !ancestors.iter().any(|a| a.owner == p.principal_id),
        "PermissionDenied",
        "delegation would create a principal cycle",
    )?;
    let grant = db.of(parent.body["grant_id"].as_str().unwrap(), "Grant")?;
    let depth = ancestors.len() as u64 + 1;
    let maximum = grant.spec["max_delegation_depth"].as_u64();
    require(
        depth <= maximum.unwrap_or(1) && (!p.allow_delegation || depth < maximum.unwrap_or(1)),
        "PermissionDenied",
        "remaining delegation depth exhausted",
    )?;
    for action in &p.actions {
        let shared = matches!(
            action.as_str(),
            "system.status" | "task.submit" | "task.read" | "resource.read" | "event.read"
        );
        let extended = matches!(
            action.as_str(),
            "session.run" | "mail.send" | broker::result_access::ACTION | agent::fallback::ACTION
        ) || TOOLS.contains(&action.as_str());
        require(
            shared || (maximum.is_some() && extended),
            "PermissionDenied",
            "action outside delegable profile",
        )?;
        delegation(db, actor, &parent.id, action)?;
    }
    let clock = deadline::Clock::sample()?;
    let until =
        after(p.ttl_seconds.into()).min(parent.body["valid_until"].as_str().unwrap().to_owned());
    let mut body = json!({"issuer":actor,"subject":p.principal_id,"parent_ref":parent.reference(),"grant_id":grant.id,
        "actions":p.actions,"budget_ref":parent.body["budget_ref"],"redelegation":p.allow_delegation,"depth":depth,"valid_until":until,"lifecycle":"Active"});
    if let Some(ids) = p.tool_targets {
        require(
            !ids.is_empty() && ids.len() <= 64,
            "InvalidArgument",
            "invalid delegation target count",
        )?;
        for target in &ids {
            db.get(target)?;
            tool_scope(db, &parent.reference(), target)?;
        }
        body["tool_targets"] = json!(ids);
    } else if let Some(targets) = parent
        .body
        .get("tool_targets")
        .or(grant.spec.get("tool_targets"))
    {
        body["tool_targets"] = targets.clone();
    }
    let child = db.record("Delegation", &p.principal_id, body)?;
    clock.register(db, &child.id, p.ttl_seconds)?;
    epoch(db)?;
    Ok(child.reference())
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
