//! Reclaim drafts whose exact original authority no longer permits submission.
use super::{check_request, field, remove, require_transaction};
use crate::{Result, db::Db, policy, require};
use rusqlite::{Row, params};
use serde_json::Value;

const BATCH: usize = 32;

struct Key {
    actor: String,
    origin: String,
    key: String,
}

struct Candidate {
    key: Key,
    request: Value,
    input_id: String,
}

fn candidate(row: &Row<'_>) -> rusqlite::Result<Candidate> {
    Ok(Candidate {
        key: Key {
            actor: row.get(0)?,
            origin: row.get(1)?,
            key: row.get(2)?,
        },
        request: row.get(3)?,
        input_id: row.get(4)?,
    })
}

#[derive(Default)]
pub(crate) struct Sweep {
    epoch: Option<String>,
    after: Option<Key>,
}

pub(super) fn authorized(db: &Db<'_>, actor: &str, request: &Value) -> Result<bool> {
    let reference = &request["delegation_ref"];
    let delegation_id = field(reference, "resource_id")?;
    let tool = field(request, "task_type")?;
    let authorization = policy::active(db, actor)
        .and_then(|()| policy::delegation(db, actor, delegation_id, "task.submit"))
        .and_then(|_| policy::delegation(db, actor, delegation_id, tool));
    match authorization {
        Ok(live) => Ok(live.reference() == *reference),
        Err(error) if error.0.code == "PermissionDenied" => Ok(false),
        Err(error) => Err(error),
    }
}

fn candidates(db: &Db<'_>, after: Option<&Key>) -> Result<Vec<Candidate>> {
    let limit = BATCH as i64;
    if let Some(after) = after {
        Ok(db
            .c
            .prepare_cached(
                "SELECT owner,origin,key,request,input_id FROM task_preparations
             WHERE (owner,origin,key)>(?1,?2,?3)
             ORDER BY owner,origin,key LIMIT ?4",
            )?
            .query_map(
                params![after.actor, after.origin, after.key, limit],
                candidate,
            )?
            .collect::<rusqlite::Result<_>>()?)
    } else {
        Ok(db
            .c
            .prepare_cached(
                "SELECT owner,origin,key,request,input_id FROM task_preparations
             ORDER BY owner,origin,key LIMIT ?1",
            )?
            .query_map([limit], candidate)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// The process-local cursor is safe to advance only after this transaction commits.
/// Finish the current pass even if the epoch changes, then revisit earlier keys.
pub(crate) fn advance(db: &Db<'_>, previous: &Sweep) -> Result<(Sweep, bool)> {
    require_transaction(db)?;
    let epoch = db.meta("epoch")?;
    if previous.after.is_none() && previous.epoch.as_deref() == Some(epoch.as_str()) {
        return Ok((
            Sweep {
                epoch: Some(epoch),
                after: None,
            },
            false,
        ));
    }
    let pass_epoch = previous
        .after
        .as_ref()
        .and(previous.epoch.as_deref())
        .unwrap_or(&epoch);
    let mut after = None;
    let rows = candidates(db, previous.after.as_ref())?;
    let page_full = rows.len() == BATCH;
    for row in rows {
        let key = row.key;
        check_request(&key.actor, &key.origin, &row.request)?;
        require(
            row.request["idempotency_key"] == key.key
                && row.request["input_ref"]["resource_id"] == row.input_id,
            "RecoveryRequired",
            "staged Task request is outside its retained key or input",
        )?;
        if !authorized(db, &key.actor, &row.request)? {
            remove(db, &key.actor, &key.origin, &key.key, &row.input_id)?;
        }
        after = Some(key);
    }
    let more = page_full || pass_epoch != epoch;
    Ok((
        Sweep {
            epoch: Some(pass_epoch.to_owned()),
            after: after.filter(|_| page_full),
        },
        more,
    ))
}
