//! Scoped, immutable work-item history pages.
pub(crate) mod revision;

use super::{ITEM, READ, access_item, digest, invalid_state, maintenance, parent_id, project_id};
use crate::{
    Result,
    db::{Db, Doc, row},
    decode,
    events::ScopedSnapshot,
    label, require,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

// Revisions are appended in order. Bound the indexed revision seek before
// applying the snapshot ceiling so new writes cannot inflate the last page's scan.
const PAGE: &str = "SELECT * FROM (
    SELECT * FROM versions WHERE id=?1 AND rev>?2 ORDER BY rev LIMIT ?3
) WHERE seq<=?4 ORDER BY rev";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

pub(crate) fn list(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Request = decode(params)?;
    label(&request.id)?;
    let limit = request.limit.unwrap_or(32);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "work item history page size must be 1 to 64",
    )?;
    let (project, item) = access_item(db, actor, &request.id, READ)?;
    let scope = digest(&json!(["WorkItem.History", project.reference(), item.id]));
    let snapshot = ScopedSnapshot::open(db, actor, &scope, request.cursor.as_deref())?;
    let mut statement = db.c.prepare_cached(PAGE)?;
    let mut revisions = statement
        .query_map(
            params![
                item.id,
                snapshot.position,
                i64::from(limit + 1),
                snapshot.high
            ],
            row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = revisions.len() > limit as usize;
    revisions.truncate(limit as usize);
    let next_cursor = if more {
        revisions
            .last()
            .map(|last| snapshot.sign(db, actor, &scope, last.rev))
            .transpose()?
    } else {
        None
    };
    let items = revisions
        .iter()
        .map(|revision| {
            if revision.kind != ITEM
                || revision.owner != project.owner
                || project_id(revision)? != project.id
            {
                return Err(invalid_state());
            }
            Ok(json!({"reference":revision.reference(),"value":projection(revision)}))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({"items":items,"next_cursor":next_cursor}))
}

fn projection(item: &Doc) -> Value {
    let review = &item.body["review"];
    let review = if review.is_null() {
        Value::Null
    } else {
        json!({"reviewer_principal_id":review["reviewer_principal_id"],
            "accepted":review["accepted"],"reviewed_at":review["reviewed_at"]})
    };
    let mut value = json!({
        "schema_version":psios_api::VERSION,"project_id":item.spec["project_id"],
        "assignee_principal_id":item.spec["assignee_principal_id"],"lifecycle":item.body["lifecycle"],
        "updated_by_principal_id":item.body["updated_by_principal_id"],"updated_at":item.stamp,
        "assignment":maintenance::assignment_view(item),"abandonment":maintenance::abandonment_view(item),
        "review":review,"has_evidence":item.body.get("evidence_ref").is_some(),
    });
    if let Some(parent) = parent_id(item) {
        value["parent_id"] = json!(parent);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus};

    #[test]
    fn page_seek_does_not_scan_post_snapshot_revisions() {
        let db = Connection::open_in_memory().unwrap();
        for schema in [
            include_str!("../state.sql"),
            include_str!("../events/queries.sql"),
        ] {
            db.execute_batch(schema).unwrap();
        }
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<12000)
             INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             SELECT 'wanted',i,1,'WorkItem','owner','{}','{}','now','now','wanted-'||i,'test','local' FROM n
             UNION ALL SELECT 'other-'||i,1,1,'WorkItem','owner','{}','{}','now','now','other-'||i,'test','local' FROM n;",
        )
        .unwrap();
        let mut page = db.prepare(PAGE).unwrap();
        let revisions = page
            .query_map(params!["wanted", 1, 3, 2], |r| r.get::<_, i64>("rev"))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(revisions, [2]);
        let steps = page.get_status(StatementStatus::VmStep);
        assert!(steps < 256, "page took {steps} SQLite VM operations");
    }
}
