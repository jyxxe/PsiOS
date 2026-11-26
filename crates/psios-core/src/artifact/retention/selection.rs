//! Bounded discovery of current, potentially reclaimable Task results.

use super::{CandidateMetadata, candidate_metadata};
use crate::{Result, db::Db, decode, err, events::ScopedSnapshot, label, policy, require};
use chrono::{DateTime, Utc};
use psios_api::{
    ResultEvictionCandidateSkips, ResultEvictionCandidates, ResultEvictionCandidatesRequest,
    ResultEvictionPlanItem, canonical, sha256,
};
use rusqlite::{Row, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::batch::FORMAT + 1;
const SCAN_LIMIT: u32 = 256;
const PAGE_LIMIT: u32 = 16;

macro_rules! indexed_candidates {
    () => {
        "kind='Task' AND body->>'lifecycle' IN ('Succeeded','Cancelled')
         AND body->>'outcome_knowledge'='Known' AND body->'result'->>'availability'='Available'"
    };
}

pub(crate) const SCHEMA: &str = concat!(
    "CREATE INDEX result_eviction_candidates_seq ON versions(seq) WHERE ",
    indexed_candidates!(),
    "; CREATE INDEX result_eviction_candidates_owner_seq ON versions(owner,seq) WHERE ",
    indexed_candidates!(),
    "; CREATE INDEX result_eviction_observation_hold ON versions(json_extract(body,'$.evidence.observation_ref.resource_id'))
        WHERE kind='RecoveryEvidence';"
);

const GLOBAL_PAGE: &str = concat!(
    "SELECT seq,id,rev,owner FROM versions INDEXED BY result_eviction_candidates_seq WHERE ",
    indexed_candidates!(),
    " AND seq>?1 AND seq<=?2 ORDER BY seq LIMIT ?3"
);

const OWNER_PAGE: &str = concat!(
    "SELECT seq,id,rev,owner FROM versions INDEXED BY result_eviction_candidates_owner_seq WHERE ",
    indexed_candidates!(),
    " AND seq>?1 AND seq<=?2 AND owner=?4 ORDER BY seq LIMIT ?3"
);

struct IndexedVersion {
    position: i64,
    id: String,
    revision: i64,
    owner: String,
}

fn indexed_version(row: &Row<'_>) -> rusqlite::Result<IndexedVersion> {
    Ok(IndexedVersion {
        position: row.get("seq")?,
        id: row.get("id")?,
        revision: row.get("rev")?,
        owner: row.get("owner")?,
    })
}

fn timestamp(value: &str) -> Option<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(value).ok()?;
    (parsed.timestamp_subsec_nanos() < 1_000_000_000).then(|| parsed.to_utc())
}

pub(crate) fn list(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let request: ResultEvictionCandidatesRequest = decode(input)?;
    require(
        request.updated_before.len() <= 64,
        "InvalidArgument",
        "updated_before must be a bounded RFC 3339 timestamp",
    )?;
    let cutoff = timestamp(&request.updated_before).ok_or_else(|| {
        err(
            "InvalidArgument",
            "updated_before must be a POSIX RFC 3339 timestamp",
        )
    })?;
    let limit = request.limit.unwrap_or(PAGE_LIMIT);
    require(
        (1..=PAGE_LIMIT).contains(&limit),
        "InvalidArgument",
        "result eviction candidate page size must be 1 to 16",
    )?;
    if let Some(principal) = &request.principal_id {
        label(principal)?;
    }
    let scope = sha256(&canonical(&json!([
        "Task.ResultEvictionCandidates",
        cutoff.timestamp(),
        cutoff.timestamp_subsec_nanos(),
        request.principal_id
    ])));
    let snapshot = ScopedSnapshot::open(db, actor, &scope, request.cursor.as_deref())?;
    // Keep age and live eligibility outside the indexed seek. Otherwise LIMIT
    // would bound matches but could scan arbitrary history to fill a page.
    let scanned_rows = if let Some(principal) = request.principal_id {
        db.c.prepare_cached(OWNER_PAGE)?
            .query_map(
                params![snapshot.position, snapshot.high, SCAN_LIMIT, principal],
                indexed_version,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        db.c.prepare_cached(GLOBAL_PAGE)?
            .query_map(
                params![snapshot.position, snapshot.high, SCAN_LIMIT],
                indexed_version,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut page = ResultEvictionCandidates {
        items: Vec::with_capacity(limit as usize),
        next_cursor: None,
        scanned: 0,
        skipped: ResultEvictionCandidateSkips {
            pinned: 0,
            ineligible: 0,
            changed: 0,
        },
    };
    let mut position = snapshot.position;
    for version in &scanned_rows {
        position = version.position;
        page.scanned += 1;
        let task = db
            .find(&version.id)?
            .ok_or_else(|| err("RecoveryRequired", "indexed result has no current Task"))?;
        require(
            version.revision > 0
                && task.kind == "Task"
                && task.owner == version.owner
                && task.rev >= version.revision,
            "RecoveryRequired",
            "indexed result disagrees with its current Task authority",
        )?;
        if task.rev != version.revision {
            page.skipped.changed += 1;
            continue;
        }
        let updated = timestamp(&task.stamp)
            .ok_or_else(|| err("RecoveryRequired", "Task has an invalid update timestamp"))?;
        if updated >= cutoff {
            page.skipped.ineligible += 1;
            continue;
        }
        match candidate_metadata(db, &task)? {
            CandidateMetadata::Pinned => page.skipped.pinned += 1,
            CandidateMetadata::Ineligible => page.skipped.ineligible += 1,
            CandidateMetadata::Ready(bytes) => {
                let content_bytes = u64::try_from(bytes)
                    .map_err(|_| err("RecoveryRequired", "Task result byte count overflow"))?;
                page.items.push(ResultEvictionPlanItem {
                    task_ref: decode(&task.reference())?,
                    content_bytes,
                });
                if page.items.len() == limit as usize {
                    break;
                }
            }
        }
    }
    // A full seek can have more history even if every inspected row was skipped.
    let scan_full = scanned_rows.len() == SCAN_LIMIT as usize && position < snapshot.high;
    if (page.scanned as usize) < scanned_rows.len() || scan_full {
        page.next_cursor = Some(snapshot.sign(db, actor, &scope, position)?);
    }
    Ok(serde_json::to_value(page)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus};

    #[test]
    fn partial_indexes_bound_global_and_owner_snapshot_seeks() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../state.sql")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            r#"WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<12000),
            eligible(body) AS (VALUES('{"lifecycle":"Succeeded","outcome_knowledge":"Known","result":{"availability":"Available"}}')),
            fixtures AS (
                SELECT 'running-'||i AS id,'wanted' AS owner,'{"lifecycle":"Running"}' AS body FROM n
                UNION ALL SELECT 'foreign-'||i,'outside',body FROM n, eligible WHERE i<=1000
                UNION ALL SELECT 'wanted-'||i,'wanted',body FROM n, eligible WHERE i<=300
            )
            INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
            SELECT id,1,1,'Task',owner,'{}',body,'now','now',id,'test','local' FROM fixtures;"#,
        ).unwrap();
        let high = 13_000 + 300;
        let mut global = conn.prepare(GLOBAL_PAGE).unwrap();
        let all = global
            .query_map(params![0, high, SCAN_LIMIT], indexed_version)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(all.len(), SCAN_LIMIT as usize);
        assert!(all.iter().all(|row| row.owner == "outside"));
        assert_eq!(global.get_status(StatementStatus::Sort), 0);
        assert!(global.get_status(StatementStatus::VmStep) < 5_000);

        let mut owner = conn.prepare(OWNER_PAGE).unwrap();
        let first = owner
            .query_map(params![0, high, SCAN_LIMIT, "wanted"], indexed_version)
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(first.len(), SCAN_LIMIT as usize);
        assert!(first.iter().all(|row| row.owner == "wanted"));
        assert_eq!(first[0].id, "wanted-1");
        assert_eq!(owner.get_status(StatementStatus::Sort), 0);
        assert!(owner.get_status(StatementStatus::VmStep) < 5_000);

        let resume = first.last().unwrap().position;
        let next = owner
            .query_map(
                params![resume, high - 1, SCAN_LIMIT, "wanted"],
                indexed_version,
            )
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(next.len(), 43);
        assert_eq!(next.first().unwrap().id, "wanted-257");
        assert_eq!(next.last().unwrap().id, "wanted-299");
    }
}
