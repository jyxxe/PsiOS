//! Strict model lease authorization is tied to its issued provider chain.
use super::{inference, *};
use crate::db::{find, resolve, row};
use rusqlite::Connection;

pub(crate) const FORMAT: u32 = node::network::trial::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("provider_authority.sql");
const SERVE: &str = "model.serve";
const MAX_CHAIN: usize = 5;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    model_ref: Value,
    delegation_refs: Vec<Value>,
    grant_ref: Value,
}

impl Binding {
    pub(super) fn digest(&self) -> String {
        sha256(&canonical(&json!([
            "psios-model-provider-authority-v1",
            self
        ])))
    }

    pub(super) fn persist(&self, db: &Db<'_>, lease: &str) -> Result<()> {
        db.c.execute(
            "INSERT INTO model_provider_authorities(lease,root_grant,binding) VALUES(?1,?2,?3)",
            params![
                lease,
                self.grant_ref["resource_id"].as_str(),
                serde_json::to_value(self)?
            ],
        )?;
        Ok(())
    }
}

pub(super) fn select(db: &Db<'_>, model: &Doc) -> Result<Binding> {
    let actor = model.spec["provider_principal_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "pinned model provider is missing"))?;
    policy::active(db, actor)?;
    let mut delegation = policy::authorized_delegation(db, actor, SERVE, Some(&model.id))?;
    let grant = db.of(
        delegation.body["grant_id"].as_str().unwrap_or_default(),
        "Grant",
    )?;
    let mut delegation_refs = vec![delegation.reference()];
    while let Some(parent) = delegation.body.get("parent_ref") {
        require(
            delegation_refs.len() < MAX_CHAIN,
            "RecoveryRequired",
            "provider delegation path exceeds the issuance limit",
        )?;
        delegation = db.resolve(parent)?;
        delegation_refs.push(delegation.reference());
    }
    let binding = Binding {
        model_ref: model.reference(),
        delegation_refs,
        grant_ref: grant.reference(),
    };
    historical(db.c, model, &grant.id, &binding)?;
    Ok(binding)
}

fn covers(authority: &Value, target: &str) -> bool {
    authority["actions"]
        .as_array()
        .is_some_and(|actions| actions.iter().any(|action| action == SERVE))
        && authority.get("tool_targets").is_none_or(|targets| {
            targets
                .as_array()
                .is_some_and(|targets| targets.iter().any(|id| id == target))
        })
}

fn historical(
    conn: &Connection,
    model: &Doc,
    root_grant: &str,
    binding: &Binding,
) -> Result<Vec<Doc>> {
    let grant = resolve(conn, &binding.grant_ref)?;
    require(
        inference::strict(model)
            && model.kind == "Model"
            && model.reference() == binding.model_ref
            && (1..=MAX_CHAIN).contains(&binding.delegation_refs.len())
            && grant.id == root_grant
            && grant.kind == "Grant"
            && grant.spec["subject"] == grant.owner
            && grant.body["lifecycle"] == "Active"
            && covers(&grant.spec, &model.id),
        "RecoveryRequired",
        "strict model lease is outside its issued provider grant",
    )?;
    let chain = binding
        .delegation_refs
        .iter()
        .map(|reference| resolve(conn, reference))
        .collect::<Result<Vec<_>>>()?;
    require(
        model.spec["provider_principal_id"] == chain[0].owner,
        "RecoveryRequired",
        "strict model lease provider differs from its issuing delegation",
    )?;
    for (i, delegation) in chain.iter().enumerate() {
        let linked = if let Some(parent) = chain.get(i + 1) {
            delegation.body.get("parent_ref") == binding.delegation_refs.get(i + 1)
                && delegation.body["issuer"] == parent.owner
        } else {
            delegation.body.get("parent_ref").is_none()
                && delegation.owner == grant.owner
                && delegation.body["issuer"] == grant.spec["issuer"]
        };
        require(
            linked
                && delegation.kind == "Delegation"
                && delegation.body["lifecycle"] == "Active"
                && delegation.body["subject"] == delegation.owner
                && delegation.body["grant_id"] == grant.id
                && delegation.body["budget_ref"] == grant.spec["budget_ref"]
                && covers(&delegation.body, &model.id),
            "RecoveryRequired",
            "strict model lease has an invalid issuing delegation chain",
        )?;
    }
    Ok(chain)
}

fn epoch(conn: &Connection) -> Result<i64> {
    let value: String = conn.query_row(
        "SELECT value FROM meta WHERE key='model_provider_authority_epoch'",
        [],
        |r| r.get(0),
    )?;
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "model provider authority epoch is invalid",
            )
        })
}

fn witnessed(
    conn: &Connection,
    issued: &Doc,
    model: &Doc,
    lease: &str,
    new: bool,
    root: Option<String>,
    binding: Option<Value>,
) -> Result<Option<(Binding, Vec<Doc>)>> {
    let usage = resolve(conn, &issued.body["current_usage_ref"])?;
    require(
        issued.kind == "Session"
            && state(issued) == "ModelInFlight"
            && issued.body["current_lease"] == lease
            && issued.spec["model_ref"] == model.reference()
            && usage.kind == "ModelUsage"
            && usage.owner == issued.owner
            && usage.body["session_id"] == issued.id
            && usage.body["call_id"] == lease
            && usage.body["model_ref"] == model.reference(),
        "RecoveryRequired",
        "provider authority is outside the exact issued model lease",
    )?;
    match (new, root, binding) {
        (true, Some(root), Some(binding)) => {
            let binding: Binding = decode(&binding)?;
            require(
                usage.body["provider_authority_digest"] == binding.digest(),
                "RecoveryRequired",
                "model lease provider binding differs from its issued checkpoint",
            )?;
            let chain = historical(conn, model, &root, &binding)?;
            Ok(Some((binding, chain)))
        }
        (false, None, None) if usage.body.get("provider_authority_digest").is_none() => Ok(None),
        _ => Err(err(
            "RecoveryRequired",
            "strict model lease provider binding is missing or has the wrong format",
        )),
    }
}

pub(super) fn current(db: &Db<'_>, session: &Doc, model: &Doc, lease: &str) -> Result<bool> {
    if !inference::strict(model) {
        return Ok(true);
    }
    let issued: Option<(i64, i64, Option<String>, Option<Value>)> =
        db.c.query_row(
            "SELECT i.session_revision,v.seq,p.root_grant,p.binding FROM model_inference_leases i
         JOIN session_turns t USING(lease)
         JOIN versions v ON v.id=t.session AND v.rev=i.session_revision
         LEFT JOIN model_provider_authorities p USING(lease)
         WHERE i.lease=?1 AND i.model=?2 AND i.status='Pending' AND t.session=?3",
            params![lease, model.id, session.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (revision, sequence, root, binding) = issued.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "strict model issue checkpoint is missing",
        )
    })?;
    let issued = db.c.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![session.id, revision],
        row,
    )?;
    let Some((binding, chain)) = witnessed(
        db.c,
        &issued,
        model,
        lease,
        sequence > epoch(db.c)?,
        root,
        binding,
    )?
    else {
        // No authoritative provider identity was recorded for a pre-cutover lease.
        // Its contexts/outcomes stay fenced and its reserved slot expires normally.
        return Ok(false);
    };
    binding_current(db, model, &binding, &chain)
}

pub(super) fn issued_binding(
    conn: &Connection,
    issued: &Doc,
    model: &Doc,
    lease: &str,
) -> Result<Option<(Binding, Vec<Doc>)>> {
    let (sequence, root, binding) = conn.query_row(
        "SELECT v.seq,p.root_grant,p.binding FROM versions v
         LEFT JOIN model_provider_authorities p ON p.lease=?3
         WHERE v.id=?1 AND v.rev=?2",
        params![issued.id, issued.rev, lease],
        |row| Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?)),
    )?;
    witnessed(
        conn,
        issued,
        model,
        lease,
        sequence > epoch(conn)?,
        root,
        binding,
    )
}

pub(super) fn binding_current(
    db: &Db<'_>,
    model: &Doc,
    binding: &Binding,
    chain: &[Doc],
) -> Result<bool> {
    let leaf = &chain[0];
    if !find(db.c, &model.id)?
        .is_some_and(|d| d.reference() == model.reference() && d.body["lifecycle"] == "Active")
        || !find(
            db.c,
            binding.grant_ref["resource_id"]
                .as_str()
                .unwrap_or_default(),
        )?
        .is_some_and(|grant| grant.reference() == binding.grant_ref)
    {
        return Ok(false);
    }
    let authorized = policy::active(db, &leaf.owner)
        .and_then(|()| policy::delegation(db, &leaf.owner, &leaf.id, SERVE))
        .and_then(|current| {
            require(
                current.reference() == binding.delegation_refs[0],
                "PermissionDenied",
                "model provider delegation changed",
            )?;
            policy::tool_scope(db, &current.reference(), &model.id)
        });
    match authorized {
        Ok(()) => Ok(true),
        Err(error) if error.0.code == "PermissionDenied" => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn require_current(db: &Db<'_>, session: &Doc, model: &Doc, lease: &str) -> Result<()> {
    require(
        current(db, session, model, lease)?,
        "Fenced",
        "the model lease's issuing provider authority is no longer current",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_records(conn).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            error
        } else {
            err(
                "RecoveryRequired",
                format!("model provider authority verification: {}", error.0.message),
            )
        }
    })
}

fn verify_records(conn: &Connection) -> Result<()> {
    let epoch = epoch(conn)?;
    let mut statement = conn.prepare(
        "SELECT i.lease,t.session,i.session_revision,v.seq,p.root_grant,p.binding
         FROM model_inference_leases i JOIN session_turns t USING(lease)
         LEFT JOIN versions v ON v.id=t.session AND v.rev=i.session_revision
         LEFT JOIN model_provider_authorities p USING(lease)",
    )?;
    let mut rows = statement.query([])?;
    while let Some(record) = rows.next()? {
        let lease: String = record.get(0)?;
        let session: String = record.get(1)?;
        let revision: i64 = record.get(2)?;
        let sequence: i64 = record.get(3)?;
        let issued = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![session, revision],
            row,
        )?;
        let model = resolve(conn, &issued.spec["model_ref"])?;
        witnessed(
            conn,
            &issued,
            &model,
            &lease,
            sequence > epoch,
            record.get(4)?,
            record.get(5)?,
        )?;
    }
    Ok(())
}
