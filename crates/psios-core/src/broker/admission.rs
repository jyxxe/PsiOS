//! A full ordinary queue must leave room to verify its unresolved effects.
use crate::{
    Result, artifact,
    db::{Db, Doc},
    decode, err,
    host::Action,
    node::work_admission,
    policy, require,
};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = 46;
pub(crate) const SCHEMA: &str = include_str!("admission.sql");

struct Capacity {
    ordinary: u32,
    recovery: u32,
}

fn capacity(db: &Db<'_>) -> Result<Capacity> {
    Ok(db.c.query_row(
        "SELECT
            (SELECT count(*) FROM queue q WHERE q.phase<>'Done'
                AND NOT EXISTS(SELECT 1 FROM recovery_admissions r WHERE r.task=q.task)
                AND NOT EXISTS(SELECT 1 FROM oci_stop_cleanups c
                    WHERE q.phase='Unknown' AND c.cleanup=q.operation)),
            (SELECT count(*) FROM recovery_admissions)",
        [],
        |row| {
            Ok(Capacity {
                ordinary: row.get(0)?,
                recovery: row.get(1)?,
            })
        },
    )?)
}

pub(crate) fn status(db: &Db<'_>) -> Result<Value> {
    let used = capacity(db)?;
    Ok(json!({
        "ordinary": {"used":used.ordinary,"limit":db.cfg.queue_limit},
        "recovery": {"used":used.recovery,"limit":db.cfg.recovery_queue_limit},
    }))
}

fn principal_ordinary(db: &Db<'_>, actor: &str) -> Result<u32> {
    Ok(db.c.query_row(
        "SELECT count(*) FROM queue q CROSS JOIN submissions s ON s.task=q.task
         WHERE q.phase<>'Done' AND s.owner=?1
           AND NOT EXISTS(SELECT 1 FROM recovery_admissions r WHERE r.task=q.task)
           AND NOT EXISTS(SELECT 1 FROM oci_stop_cleanups c
               WHERE q.phase='Unknown' AND c.cleanup=q.operation)",
        [actor],
        |row| row.get(0),
    )?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalView {
    #[serde(default)]
    include_preparations: bool,
}

pub(crate) fn principal(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let params: PrincipalView = decode(params)?;
    policy::can(db, actor, "task.submit")?;
    let limit = db.cfg.principal_queue_limit.unwrap_or(db.cfg.queue_limit);
    let mut view = json!({
        "principal_id":actor,
        "ordinary":{"used":principal_ordinary(db, actor)?,"limit":limit},
    });
    if params.include_preparations {
        view["preparations"] = super::preparation::usage(db, actor)?;
    }
    if let Some(byte_limit) = db.cfg.read_result_bytes_per_principal {
        view["read_results"] = artifact::capacity::principal(db, actor, byte_limit)?;
    }
    Ok(view)
}

fn held_operation(db: &Db<'_>, target: &Doc) -> Result<Option<String>> {
    Ok(db
        .c
        .query_row(
            "SELECT holder FROM holds WHERE volume=?1",
            [&target.id],
            |row| row.get(0),
        )
        .optional()?)
}

fn linked_operation(db: &Db<'_>, action: &Action, target: &Doc) -> Result<Option<Doc>> {
    let (operation, eligible_effects): (Option<String>, &[&str]) = match action {
        Action::InspectBackupPublication(_) => (Some(target.id.clone()), &["backup.create"]),
        Action::InspectRestoredVolume(_) => (Some(target.id.clone()), &["backup.restore"]),
        Action::InspectReleaseImageRestore(_) => {
            (Some(target.id.clone()), &[crate::apps::restore_image::TOOL])
        }
        Action::InspectFileRename(_) => {
            (Some(target.id.clone()), &[crate::storage::file_move::TOOL])
        }
        Action::InspectNetworkTrial(_) => (
            Some(target.id.clone()),
            &[crate::node::network::trial::TOOL],
        ),
        Action::Health(_) => (
            held_operation(db, target)?,
            &["application.start", "application.stop"],
        ),
        Action::ContinueOciStop(_) => (held_operation(db, target)?, &["application.stop"]),
        Action::InspectOciInstance(_) => {
            (held_operation(db, target)?, &[crate::apps::restart::TOOL])
        }
        Action::ServiceInspect(_) => (
            held_operation(db, target)?,
            &["service.start", "service.stop", crate::node::RESTART_TOOL],
        ),
        _ => return Ok(None),
    };
    let Some(id) = operation else {
        return Ok(None);
    };
    let source: Option<String> =
        db.c.query_row(
            "SELECT task FROM queue WHERE phase='Unknown' AND operation=?1",
            [&id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(source) = source else {
        return Ok(None);
    };
    let task = db.of(&source, "Task")?;
    if !task.spec["task_type"]
        .as_str()
        .is_some_and(|tool| eligible_effects.contains(&tool))
    {
        return Ok(None);
    }
    let op = db.of(&id, "Operation")?;
    if let Action::ContinueOciStop(request) = action
        && request.operation_ref != op.reference()
    {
        return Ok(None);
    }
    require(
        op.body["outcome_knowledge"] == "Unknown" && op.spec["task_id"] == task.id,
        "RecoveryRequired",
        "open effect disagrees with its durable queue record",
    )?;
    if target.kind != "Operation" {
        let plan = db.resolve(&op.body["plan_ref"])?;
        if plan.body["allowed_effects"][0]["resource_ref"] != target.reference() {
            return Ok(None);
        }
    }
    Ok(Some(op))
}

/// Call after authorizing the actual tool and each resource in its audience.
pub(super) fn select(
    db: &Db<'_>,
    actor: &str,
    action: &Action,
    target: &Doc,
    has_dependencies: bool,
    recovery_only: bool,
) -> Result<Option<Doc>> {
    if recovery_only {
        if has_dependencies {
            return Err(work_admission::closed());
        }
        let operation = linked_operation(db, action, target)?.ok_or_else(work_admission::closed)?;
        require(
            capacity(db)?.recovery < db.cfg.recovery_queue_limit,
            "QueueFull",
            "recovery queue full",
        )?;
        return reserve(db, operation).map(Some);
    }
    let used = capacity(db)?;
    let exhausted = if used.ordinary >= db.cfg.queue_limit {
        "durable queue full"
    } else if let Some(maximum) = db.cfg.principal_queue_limit
        && principal_ordinary(db, actor)? >= maximum
    {
        "submitting principal's durable queue is full"
    } else {
        return Ok(None);
    };
    require(
        !has_dependencies && used.recovery < db.cfg.recovery_queue_limit,
        "QueueFull",
        exhausted,
    )?;
    let operation =
        linked_operation(db, action, target)?.ok_or_else(|| err("QueueFull", exhausted))?;
    reserve(db, operation).map(Some)
}

fn reserve(db: &Db<'_>, operation: Doc) -> Result<Doc> {
    let already: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM recovery_admissions WHERE operation=?1)",
        [&operation.id],
        |row| row.get(0),
    )?;
    require(
        !already,
        "QueueFull",
        "the unresolved operation has a pending recovery task",
    )?;
    Ok(operation)
}

pub(super) fn bind(db: &Db<'_>, task: &Doc, operation: &Doc) -> Result<()> {
    db.c.execute(
        "INSERT INTO recovery_admissions(task,operation,operation_revision) VALUES(?1,?2,?3)",
        params![task.id, operation.id, operation.rev],
    )?;
    db.create(
        "RecoveryAdmission",
        &format!("recovery-admission-{}", task.id),
        policy::OWNER,
        json!({"task_id":task.id,"operation_id":operation.id,"principal_id":task.owner}),
        json!({"admitted_task_ref":task.reference(),"unresolved_operation_ref":operation.reference()}),
    )?;
    Ok(())
}

pub(super) fn check(db: &Db<'_>, task: &str, action: &Action, target: &Doc) -> Result<()> {
    let expected: Option<(String, i64)> =
        db.c.query_row(
            "SELECT operation,operation_revision FROM recovery_admissions WHERE task=?1",
            [task],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((id, revision)) = expected {
        require(
            linked_operation(db, action, target)?
                .is_some_and(|op| op.id == id && op.rev == revision),
            "PreconditionFailed",
            "reserved recovery admission no longer matches an unresolved effect",
        )?;
    }
    Ok(())
}

pub(super) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute("DELETE FROM recovery_admissions WHERE task=?1", [task])?;
    Ok(())
}
