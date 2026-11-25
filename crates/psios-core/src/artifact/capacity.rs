//! Account for ordinary read-result payloads and their pending publication space.
use crate::{
    Error, Result,
    db::{Db, Doc},
    err, policy, require,
};
use psios_api::ApiError;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::io::{self, Write};

pub(crate) const FORMAT: u32 = crate::storage::file_move::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("capacity.sql");
pub(crate) const RESERVATION: i64 = 4 * 1024 * 1024;

fn charged(tool: &str) -> bool {
    policy::readonly(tool)
        && !matches!(
            tool,
            "service.inspect"
                | "service.observe"
                | "application.health"
                | "application.inspect_instance"
                | crate::apps::restore_image::INSPECT
                | "application.refresh"
                | "backup.inspect_publication"
                | "volume.inspect_publication"
                | "volume.inspect_rename"
        )
}

fn usage(db: &Db<'_>, owner: &str) -> Result<(i64, i64)> {
    Ok(db
        .c
        .query_row(
            "SELECT reserved_bytes,retained_bytes FROM read_result_usage WHERE owner=?1",
            [owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .unwrap_or_default())
}

pub(crate) fn principal(db: &Db<'_>, actor: &str, limit: i64) -> Result<Value> {
    let (reserved, retained) = usage(db, actor)?;
    Ok(json!({
        "reserved_bytes":reserved.to_string(),"retained_bytes":retained.to_string(),
        "limit_bytes":limit.to_string(),"reservation_per_task_bytes":RESERVATION.to_string(),
    }))
}

pub(crate) fn check_admission(db: &Db<'_>, actor: &str, tool: &str) -> Result<()> {
    if let Some(limit) = db.cfg.read_result_bytes_per_principal
        && charged(tool)
    {
        let (reserved, retained) = usage(db, actor)?;
        if reserved
            .saturating_add(retained)
            .saturating_add(RESERVATION)
            > limit
        {
            let mut quota = ApiError::new("QuotaExceeded", "read result capacity exhausted");
            quota.retryable = true;
            quota.retry_condition = "SameRequestAfterBackoff".into();
            return Err(Error(Box::new(quota)));
        }
    }
    Ok(())
}

fn insert(db: &Db<'_>, resource: &str, owner: &str, bytes: i64, kind: &str) -> Result<()> {
    db.c.execute(
        "INSERT INTO read_result_charges(resource,owner,bytes,kind) VALUES(?1,?2,?3,?4)",
        params![resource, owner, bytes, kind],
    )?;
    Ok(())
}

pub(crate) fn reserve(db: &Db<'_>, task: &Doc, tool: &str) -> Result<()> {
    if charged(tool) {
        let kind = if db.cfg.read_result_bytes_per_principal.is_some() {
            "Limited"
        } else {
            "Reserved"
        };
        insert(db, &task.id, &task.owner, RESERVATION, kind)?;
    }
    Ok(())
}

pub(crate) fn release(db: &Db<'_>, resource: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM read_result_charges WHERE resource=?1",
        [resource],
    )?;
    Ok(())
}

struct BoundedSize {
    remaining: u64,
    overflow: bool,
}

impl Write for BoundedSize {
    fn write(&mut self, block: &[u8]) -> io::Result<usize> {
        if let Some(remaining) = self.remaining.checked_sub(block.len() as u64) {
            self.remaining = remaining;
            Ok(block.len())
        } else {
            self.overflow = true;
            Err(io::Error::other("reserved artifact bytes exceeded"))
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn check_result(db: &Db<'_>, task: &Doc, receipt: &Value) -> Result<()> {
    let reservation: Option<i64> =
        db.c.query_row(
            "SELECT bytes FROM read_result_charges WHERE resource=?1 AND owner=?2 AND kind='Limited'",
            params![task.id, task.owner],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(remaining) = reservation {
        let mut bound = BoundedSize {
            remaining: u64::try_from(remaining)
                .map_err(|_| err("RecoveryRequired", "negative read result reservation"))?,
            overflow: false,
        };
        if let Err(error) = serde_json::to_writer(&mut bound, receipt) {
            if bound.overflow {
                return Err(err(
                    "QuotaExceeded",
                    "read result exceeds reserved payload bytes",
                ));
            }
            return Err(error.into());
        }
    }
    Ok(())
}

pub(crate) fn retain(db: &Db<'_>, task: &Doc, artifact: &Doc) -> Result<()> {
    if charged(task.spec["task_type"].as_str().unwrap_or_default()) {
        let (reserved, kind): (i64, String) = db.c.query_row(
            "DELETE FROM read_result_charges WHERE resource=?1 AND owner=?2 AND kind<>'Retained'
             RETURNING bytes,kind",
            params![task.id, task.owner],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?.ok_or_else(|| err("RecoveryRequired", "read result reservation missing"))?;
        let bytes = artifact.spec["size_bytes"]
            .as_str()
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or_else(|| err("RecoveryRequired", "read result size missing"))?;
        require(
            kind != "Limited" || bytes <= reserved,
            "RecoveryRequired",
            "published result exceeded its checked size",
        )?;
        insert(db, &artifact.id, &task.owner, bytes, "Retained")?;
    }
    Ok(())
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    // Existing admitted work keeps its right to settle even when a deployment
    // enables a limit below the volume of work it already accepted.
    let mut pending = db.c.prepare(
        "SELECT q.task,t.owner,t.spec->>'task_type' FROM queue q JOIN current t ON t.id=q.task
         WHERE q.phase<>'Done' AND t.kind='Task'",
    )?;
    for row in pending.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (task, owner, tool) = row?;
        if charged(&tool) {
            insert(db, &task, &owner, RESERVATION, "Reserved")?;
        }
    }

    // Read manifest sizes, not payloads. Unavailable, repairable chunks still
    // count against their existing allocation until explicitly evicted.
    let mut completed = db.c.prepare(
        "SELECT a.id,a.owner,t.spec->>'task_type',CAST(a.spec->>'size_bytes' AS INTEGER)
           FROM current a JOIN current t ON t.id=a.spec->>'task_id'
           WHERE a.kind='Artifact' AND a.spec->>'content_layout'='ChunkedJsonV1'
             AND t.kind='Task' AND a.owner=t.owner
             AND NOT EXISTS (
                SELECT 1 FROM current e, json_each(e.body->'artifact_refs') pin
                 WHERE e.id='result-eviction-'||t.id AND pin.value->>'resource_id'=a.id
             )",
    )?;
    for row in completed.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })? {
        let (artifact, owner, tool, bytes) = row?;
        if charged(&tool) {
            insert(db, &artifact, &owner, bytes, "Retained")?;
        }
    }
    Ok(())
}
