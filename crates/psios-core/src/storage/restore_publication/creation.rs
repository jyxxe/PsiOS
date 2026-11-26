//! Publication custody for new empty volumes, whose approved pool is not an exclusive source.
use super::*;
use crate::{broker::dependencies, db, label};
use rusqlite::Connection;
use rustix::fs::unlinkat;

const EPOCH: &str = "volume_create_custody_epoch";

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.c.execute_batch(
        "CREATE INDEX volume_create_unsettled_versions ON versions(id,rev)
         WHERE kind='Task' AND spec->>'task_type'='volume.create'
         AND body->>'outcome_knowledge' IN ('InFlight','Unknown');",
    )?;
    db.c.execute(
        "INSERT INTO meta(key,value) SELECT ?1,CAST(COALESCE(MAX(seq),0) AS TEXT) FROM versions",
        [EPOCH],
    )?;
    Ok(())
}

pub(super) fn cutover(conn: &Connection) -> Result<Option<i64>> {
    // The general restore verifier also opens authorities older than this format.
    let format: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if format < super::CREATE_FORMAT {
        return Ok(None);
    }
    let cutover = conn
        .query_row("SELECT value FROM meta WHERE key=?1", [EPOCH], |r| {
            r.get::<_, String>(0)
        })
        .optional()?;
    let latest: i64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM versions", [], |row| {
        row.get(0)
    })?;
    let cutover = cutover
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| (0..=latest).contains(value))
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "new volume custody epoch is missing or invalid",
            )
        })?;
    Ok(Some(cutover))
}

pub(super) fn binding(conn: &Connection, op: &Doc, pool: &Doc) -> Result<Value> {
    let task = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "new volume has no original Task"))?;
    let task = verification::needed(conn, task)?;
    let input = db::resolve(conn, &task.spec["input_ref"])?;
    let input = dependencies::tool_input(&input)?;
    let name = input["name"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "new volume has no original name"))?;
    require(
        task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == CREATE
            && input.as_object().is_some_and(|input| input.len() == 1)
            && label(name).is_ok()
            && pool.kind == "StoragePool"
            && pool.id == "local"
            && pool.body["allocation_enabled"] == true,
        "RecoveryRequired",
        "new volume does not match the original Task and approved local pool",
    )?;
    Ok(json!({"pool_ref":pool.reference(),"name":name}))
}

pub(super) fn start_effect(db: &Db<'_>, work: &Work) -> Result<Value> {
    let effect = db.of(&work.effect.id, "Effect")?;
    let operation = db.of(&work.op.id, "Operation")?;
    let task = db.of(&work.task.id, "Task")?;
    let run = db.of(&work.run.id, "Run")?;
    require(
        effect.reference() == work.effect.reference()
            && effect.body["phase"] == "PermitBound"
            && operation.body["effect_refs"] == json!([effect.reference()])
            && task.body["operation_refs"] == json!([operation.reference()])
            && run.body["operation_refs"] == json!([operation.reference()]),
        "Fenced",
        "new volume publication permit has already been used or replaced",
    )?;
    let mut body = effect.body.clone();
    body["phase"] = json!("EffectStarted");
    let started = db.status(&effect, body)?;
    let mut body = operation.body.clone();
    body["effect_refs"] = json!([started.reference()]);
    let op = db.status(&operation, body)?;
    for doc in [run, task] {
        let mut body = doc.body.clone();
        body["operation_refs"] = json!([op.reference()]);
        db.status(&doc, body)?;
    }
    Ok(started.reference())
}

pub(super) fn verify_started(conn: &Connection, op: &Doc, checkpoint: &Doc) -> Result<()> {
    let started = db::resolve(conn, &checkpoint.body["started_effect_ref"])?;
    let current = db::resolve(conn, &op.body["effect_refs"][0])?;
    let bound = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&started.id],
        db::row,
    )?;
    let mut body = bound.body.clone();
    body["phase"] = json!("EffectStarted");
    let ordered: bool = conn.query_row(
        "SELECT (SELECT seq FROM versions WHERE id=?1 AND rev=2)
              < (SELECT seq FROM versions WHERE id=?2 AND rev=1)",
        [&started.id, &checkpoint.id],
        |row| row.get(0),
    )?;
    require(
        bound.kind == "Effect"
            && bound.owner == op.owner
            && bound.body["phase"] == "PermitBound"
            && bound.body["operation_id"] == op.id
            && bound.body["action"] == CREATE
            && bound.body["outcome_knowledge"] == "InFlight"
            && bound.body["resource_ref"] == checkpoint.body["create_source"]["pool_ref"]
            && started.kind == "Effect"
            && started.owner == op.owner
            && started.rev == 2
            && started.spec == bound.spec
            && started.body == body
            && current.kind == "Effect"
            && current.owner == op.owner
            && current.id == started.id
            && current.spec == started.spec
            && current.rev >= started.rev
            && ordered,
        "RecoveryRequired",
        "new volume checkpoint has no exact started effect",
    )
}

pub(in crate::storage) fn check_empty(data: &File) -> Result<()> {
    let before = data.metadata()?;
    require(
        before.mode() & 0o077 == 0 && before.uid() == rustix::process::geteuid().as_raw(),
        "PreconditionFailed",
        "new volume data root does not have the private creation profile",
    )?;
    for entry in Dir::read_from(data).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        require(
            matches!(entry.file_name().to_bytes(), b"." | b".."),
            "PreconditionFailed",
            "new volume data root is not empty",
        )?;
    }
    require(
        super::super::same_source(&before, &data.metadata()?),
        "Conflict",
        "new volume data root changed during inspection",
    )
}

// Only unlink two known creation entries. In particular, rmdir itself rechecks
// emptiness if a file appears between the inspection and the syscall.
pub(super) fn dispose(parent: &File, name: &str, root: &File, receipt: &Value) -> Result<()> {
    check_wrapper(root)?;
    let marker = present(root, "identity")?;
    if marker {
        check_marker(root, receipt)?;
    }
    if present(root, "data")? {
        require(
            marker,
            "PreconditionFailed",
            "new volume identity marker is missing",
        )?;
        let data = open_data(root, receipt)?;
        check_empty(&data)?;
        unlinkat(root, "data", AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
        root.sync_all()?;
    }
    if marker {
        unlinkat(root, "identity", AtFlags::empty()).map_err(std::io::Error::from)?;
        root.sync_all()?;
    }
    require(
        private_identity(
            &child(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?,
            &identity(root)?,
        )?,
        "PreconditionFailed",
        "new volume quarantine changed during cleanup",
    )?;
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
    Ok(())
}
