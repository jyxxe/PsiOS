//! Live discovery of a contributor's own ready work.
use super::{CONTRIBUTE, ITEM, MAX_ITEMS, access, assigned, invalid_state, item_view, project_id};
use crate::{
    Result,
    db::{Db, Doc},
    decode, label, require,
};
use psios_api::AssignedWorkRequest;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) use psios_api::ASSIGNED_WORK_TOOL as TOOL;
pub(crate) const FORMAT: u32 = crate::artifact::retention::selection::FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE INDEX project_work_items_assignee
    ON versions((spec->>'project_id'),(spec->>'assignee_principal_id'),id,rev) WHERE kind='WorkItem';";
const PAGE_LIMIT: u32 = 16;
const PAGE_BYTES: usize = 64 * 1024;

// Seek by distinct ID and then check the current head independently. An
// assignment may have many historical revisions or may have moved away.
const NEXT_ID: &str = "SELECT id FROM versions INDEXED BY project_work_items_assignee
    WHERE kind='WorkItem' AND spec->>'project_id'=?1 AND spec->>'assignee_principal_id'=?2
      AND id>?3 ORDER BY id LIMIT 1";

#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct Input(AssignedWorkRequest);

impl Input {
    fn validate_common(&self) -> Result<()> {
        let request = &self.0;
        label(&request.project_id)?;
        require(
            (1..=PAGE_LIMIT).contains(&request.limit.unwrap_or(PAGE_LIMIT)),
            "InvalidArgument",
            "assigned work page size must be 1 to 16",
        )?;
        if let Some(version) = &request.expected_project_version {
            require(
                version
                    .strip_prefix('r')
                    .and_then(|number| number.parse::<i64>().ok())
                    .is_some_and(|number| number > 0 && *version == format!("r{number}")),
                "InvalidArgument",
                "assigned work requires a valid Project revision",
            )?;
        }
        if let Some(after) = &request.after_id {
            require(
                after.strip_prefix("work-item-").is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                }),
                "InvalidArgument",
                "invalid assigned work page position",
            )?;
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_common()?;
        require(
            self.0.expected_project_version.is_some(),
            "InvalidArgument",
            "assigned work tool requires an exact Project revision",
        )
    }

    fn project(&self, db: &Db<'_>, actor: &str) -> Result<Doc> {
        let project = access(db, actor, &self.0.project_id, CONTRIBUTE)?;
        if let Some(version) = &self.0.expected_project_version {
            project.cas(version)?;
        }
        Ok(project)
    }

    pub(crate) fn target(&self, db: &Db<'_>, actor: &str) -> Result<Doc> {
        self.validate()?;
        self.project(db, actor)
    }

    pub(crate) fn list(&self, db: &Db<'_>, actor: &str) -> Result<Value> {
        let project = self.target(db, actor)?;
        self.page(db, actor, &project)
    }

    fn page(&self, db: &Db<'_>, actor: &str, project: &Doc) -> Result<Value> {
        let limit = self.0.limit.unwrap_or(PAGE_LIMIT) as usize;
        let mut query = db.c.prepare_cached(NEXT_ID)?;
        let mut position = self.0.after_id.clone().unwrap_or_default();
        let mut items = Vec::with_capacity(limit);
        let mut last_returned = None;
        let mut next_after_id = None;
        let mut items_bytes = 0;
        for scanned in 0..=MAX_ITEMS {
            let next: Option<String> = query
                .query_row(params![project.id, actor, position], |row| row.get(0))
                .optional()?;
            let Some(id) = next else { break };
            require(
                scanned < MAX_ITEMS,
                "RecoveryRequired",
                "assigned work exceeds its project item limit",
            )?;
            let item = db.find(&id)?.ok_or_else(invalid_state)?;
            require(
                item.kind == ITEM
                    && item.owner == project.owner
                    && project_id(&item)? == project.id,
                "RecoveryRequired",
                "assigned work is outside its project authority",
            )?;
            position = id;
            if assigned(&item)? != actor || item.body["lifecycle"] != "Ready" {
                continue;
            }
            let record = item_view(db, &item, actor, false)?;
            let blockers = record["value"]["blockers"]
                .as_array()
                .ok_or_else(invalid_state)?;
            if !self.0.include_blocked && !blockers.is_empty() {
                continue;
            }

            let next_size =
                items_bytes + usize::from(!items.is_empty()) + serde_json::to_vec(&record)?.len();
            let envelope = serde_json::to_vec(&json!({"items":[],"next_after_id":item.id}))?.len();
            if items.len() == limit || next_size + envelope > PAGE_BYTES {
                require(
                    last_returned.is_some(),
                    "QuotaExceeded",
                    "assigned work item exceeds page byte limit",
                )?;
                next_after_id = last_returned;
                break;
            }
            items_bytes = next_size;
            last_returned = Some(item.id);
            items.push(record);
        }
        Ok(json!({"items":items,"next_after_id":next_after_id}))
    }
}

pub(crate) fn list(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let input: Input = decode(params)?;
    input.validate_common()?;
    let project = input.project(db, actor)?;
    input.page(db, actor, &project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus};

    #[test]
    fn next_assigned_id_seeks_over_revision_history() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(include_str!("../state.sql")).unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db.execute_batch(
            "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<12000),
             fixtures(id,rev,project,assignee) AS (
                 SELECT 'one',i,'project','alice' FROM n
                 UNION ALL SELECT 'third',1,'project','alice'
                 UNION ALL SELECT 'second',1,'other','alice'
                 UNION ALL SELECT 'second-b',1,'project','bob'
             ) INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             SELECT id,rev,1,'WorkItem','owner',json_object('project_id',project,'assignee_principal_id',assignee),
                    '{}','now','now',id||'-'||rev,'test','local' FROM fixtures;",
        ).unwrap();
        let mut seek = db.prepare(NEXT_ID).unwrap();
        let get = |row: &rusqlite::Row<'_>| row.get::<_, String>(0);
        let first = seek
            .query_row(params!["project", "alice", ""], get)
            .unwrap();
        let next = seek
            .query_row(params!["project", "alice", &first], get)
            .unwrap();
        assert_eq!((first.as_str(), next.as_str()), ("one", "third"));
        assert!(
            seek.query_row(params!["project", "alice", &next], get)
                .optional()
                .unwrap()
                .is_none()
        );
        let steps = seek.get_status(StatementStatus::VmStep);
        assert!(steps < 200, "keyset seek took {steps} SQLite VM operations");
    }
}
