use crate::{
    ById, Result, artifact,
    db::{Db, Doc},
    decode, err, policy, require,
};
use psios_api::{ResourceRef, VERSION};
use serde_json::{Value, json};

pub(crate) const ACTION: &str = "task.results.read";
pub(crate) const FORMAT: u32 = 45;
pub(crate) const SCHEMA: &str = include_str!("result_access.sql");

fn reads_owned(db: &Db<'_>, actor: &str, doc: &Doc) -> Result<bool> {
    if actor == doc.owner || actor == policy::OWNER {
        match policy::can(db, actor, "task.read") {
            Ok(()) => return Ok(true),
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

pub(super) fn producer(db: &Db<'_>, actor: &str, id: &str) -> Result<Doc> {
    if actor == policy::OWNER {
        return db.of(id, "Task");
    }
    db.find(id)?
        .filter(|doc| doc.kind == "Task")
        .ok_or_else(|| {
            err(
                "PermissionDenied",
                "task result is outside authorized scope",
            )
        })
}

/// Select the authorization at admission. Own-result access returns no extra grant.
pub(super) fn authorize(db: &Db<'_>, actor: &str, task: &Doc) -> Result<Option<Doc>> {
    if reads_owned(db, actor, task)? {
        return Ok(None);
    }
    policy::authorized_delegation(db, actor, ACTION, Some(&task.id)).map(Some)
}

/// Recheck the same authority before approval, claim, and dispatch.
pub(super) fn check(
    db: &Db<'_>,
    actor: &str,
    task: &Doc,
    read_delegation: Option<&str>,
) -> Result<()> {
    if let Some(id) = read_delegation {
        let delegation = policy::delegation(db, actor, id, ACTION)?;
        return policy::tool_scope(db, &delegation.reference(), &task.id);
    }
    policy::owned(actor, task)?;
    policy::can(db, actor, "task.read")
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: ById = decode(params)?;
    let task = producer(db, actor, &p.id)?;
    let shared = authorize(db, actor, &task)?.is_some();
    // Condition messages may include plan identifiers or tool diagnostics.
    let conditions: Vec<_> = task.body["conditions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| json!({"type":c["type"],"status":c["status"],"reason":c["reason"]}))
        .collect();
    if shared {
        audit_read(db, actor, "Task.Results", &task.id)?;
    }
    Ok(json!({
        "schema_version": VERSION,
        "task_id": task.id,
        "task_revision": task.version(),
        "lifecycle": task.body["lifecycle"],
        "outcome_knowledge": task.body["outcome_knowledge"],
        "result": task.body["result"],
        "conditions": conditions,
    }))
}

pub(crate) fn resolve_artifact(db: &Db<'_>, actor: &str, reference: &Value) -> Result<(Doc, bool)> {
    let read_owned = || -> Result<(Doc, bool)> {
        policy::can(db, actor, "task.read")?;
        let artifact = db.resolve(reference)?;
        require(
            artifact.kind == "Artifact",
            "InvalidArgument",
            "expected result artifact",
        )?;
        policy::owned(actor, &artifact)?;
        Ok((artifact, false))
    };
    if actor == policy::OWNER {
        return read_owned();
    }
    let r: ResourceRef = decode(reference)?;
    let denied = || {
        err(
            "PermissionDenied",
            "artifact is outside authorized result scope",
        )
    };
    let artifact = db.find(&r.resource_id)?.ok_or_else(denied)?;
    if reads_owned(db, actor, &artifact)? {
        require(
            artifact.kind == "Artifact",
            "InvalidArgument",
            "expected result artifact",
        )?;
        return Ok((db.resolve(reference)?, false));
    }
    let task_id = artifact.spec["task_id"].as_str().ok_or_else(denied)?;
    let task = producer(db, actor, task_id)?;
    policy::can_target(db, actor, ACTION, &task.id)?;
    require(
        artifact::is_task_output(&artifact, &task)
            && artifact.reference() == *reference
            && task.body["result"]["artifact_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(reference)),
        "PermissionDenied",
        "artifact is outside authorized result scope",
    )?;
    Ok((artifact, true))
}

pub(crate) fn audit_read(db: &Db<'_>, actor: &str, method: &str, id: &str) -> Result<()> {
    db.cfg.check_admission_space()?;
    db.audit(actor, method, Some(id), "Allowed")
}
