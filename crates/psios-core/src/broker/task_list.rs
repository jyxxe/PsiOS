//! Read exact Task revisions at a fixed snapshot in acceptance order.
use crate::{
    Result,
    db::{Db, row},
    decode, err,
    events::ScopedSnapshot,
    label, policy, require,
};
use psios_api::{TaskListQuery, canonical, sha256};
use rusqlite::{params, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};

const SCAN_LIMIT: usize = 128;
const OWNED_ORIGINS: &str = "SELECT id,seq FROM versions INDEXED BY versions_origin_owner_kind
     WHERE rev=1 AND kind='Task' AND seq>?1 AND seq<=?2 AND owner=?4 ORDER BY seq LIMIT ?3";
const ALL_ORIGINS: &str = "SELECT id,seq FROM versions INDEXED BY versions_origin_kind
     WHERE rev=1 AND kind='Task' AND seq>?1 AND seq<=?2 ORDER BY seq LIMIT ?3";
const AS_OF: &str = "SELECT * FROM versions INDEXED BY versions_event_id
    WHERE id=?1 AND seq<=?2 ORDER BY seq DESC LIMIT 1";

pub(crate) fn list(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "task.read")?;
    let p: TaskListQuery = decode(p)?;
    require(
        actor == policy::OWNER
            || p.owner_principal_id
                .as_deref()
                .is_none_or(|owner| owner == actor),
        "PermissionDenied",
        "task list owner is outside the authorized principal",
    )?;
    let owner = if actor == policy::OWNER {
        p.owner_principal_id.as_deref()
    } else {
        Some(actor)
    };
    for value in [owner, p.task_type.as_deref()].into_iter().flatten() {
        label(value)?;
    }
    let limit = p.limit.unwrap_or(32) as usize;
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "task list page size must be 1..64",
    )?;
    let lifecycle = json!(p.lifecycle);
    let scope = format!(
        "task.list/{}",
        sha256(&canonical(&json!([owner, p.task_type, lifecycle])))
    );
    let snapshot = ScopedSnapshot::open(db, actor, &scope, p.cursor.as_deref())?;
    let mut arguments = vec![
        SqlValue::Integer(snapshot.position),
        SqlValue::Integer(snapshot.high),
        SqlValue::Integer((SCAN_LIMIT + 1) as i64),
    ];
    let sql = if let Some(owner) = owner {
        arguments.push(owner.to_owned().into());
        OWNED_ORIGINS
    } else {
        ALL_ORIGINS
    };
    let mut origins = db.c.prepare_cached(sql)?;
    let mut origins = origins.query(params_from_iter(arguments))?;
    let mut as_of = db.c.prepare_cached(AS_OF)?;
    let mut items = Vec::with_capacity(limit);
    let mut position = snapshot.position;
    let mut next = None;
    for examined in 0..=SCAN_LIMIT {
        let Some(origin) = origins.next()? else { break };
        if examined == SCAN_LIMIT || items.len() == limit {
            next = Some(snapshot.sign(db, actor, &scope, position)?);
            break;
        }
        let id: String = origin.get("id")?;
        position = origin.get("seq")?;
        let task = as_of.query_row(params![id, snapshot.high], row)?;
        require(
            task.kind == "Task" && owner.is_none_or(|owner| task.owner == owner),
            "RecoveryRequired",
            "task history disagrees with its accepted identity",
        )?;
        let tool = task.spec["task_type"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "task history has no registered action"))?;
        if p.task_type.as_deref().is_some_and(|filter| tool != filter)
            || (!lifecycle.is_null() && task.body["lifecycle"] != lifecycle)
        {
            continue;
        }
        items.push(json!({
            "task_ref": task.reference(),
            "owner_principal_id": task.owner,
            "task_type": tool,
            "lifecycle": task.body["lifecycle"],
            "outcome_knowledge": task.body["outcome_knowledge"],
            "result_availability": task.body["result"]["availability"],
            "current_run_id": task.body["current_run_id"],
            "accepted_at": task.created,
            "updated_at": task.stamp,
        }));
    }
    Ok(json!({"items":items,"complete":next.is_none(),"next_cursor":next}))
}
