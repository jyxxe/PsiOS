//! Keep each authorized event stream ordered and bound its work to the query scope.
use super::Cursor;
use crate::policy;
use rusqlite::types::Value;

pub(crate) const FORMAT: u32 = crate::apps::restart::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("queries.sql");
pub(crate) const SNAPSHOT_FORMAT: u32 = crate::artifact::capacity::FORMAT + 1;
pub(crate) const SNAPSHOT_SCHEMA: &str = include_str!("snapshots.sql");

#[derive(Clone, Copy)]
pub(super) enum View {
    Snapshot,
    Changes,
}

pub(super) fn selection(cursor: &Cursor, view: View, limit: u32) -> (String, Vec<Value>) {
    let origins = matches!(view, View::Snapshot) && cursor.orders_by_creation();
    let mut arguments = vec![cursor.position.into(), i64::from(limit).into()];
    let private_kinds = policy::PRIVATE_RESOURCE_KINDS.join("','");
    let mut conditions = format!("v.seq > ?1 AND v.kind NOT IN ('{private_kinds}')");
    let resource = cursor.resource.as_ref().map(|id| {
        arguments.push(id.clone().into());
        let position = arguments.len();
        conditions.push_str(&format!(" AND v.id = ?{position}"));
        position
    });
    let kind = !cursor.kind.is_empty();
    if kind {
        arguments.push(cursor.kind.clone().into());
        conditions.push_str(&format!(" AND v.kind = ?{}", arguments.len()));
    }
    let high = if matches!(view, View::Snapshot) {
        arguments.push(cursor.high.into());
        let high = arguments.len();
        if origins {
            conditions.push_str(&format!(" AND v.rev = 1 AND v.seq <= ?{high}"));
        } else if let Some(resource) = resource {
            conditions.push_str(&format!(
                " AND v.seq = (SELECT max(w.seq) FROM versions w INDEXED BY versions_event_id
                               WHERE w.id = ?{resource} AND w.seq <= ?{high})"
            ));
        } else {
            conditions.push_str(&format!(
                " AND v.seq <= ?{high}
                  AND NOT EXISTS (SELECT 1 FROM versions w
                                  WHERE w.id = v.id AND w.seq <= ?{high} AND w.rev > v.rev)"
            ));
        }
        Some(high)
    } else {
        None
    };
    let own = cursor.actor != policy::OWNER;
    let index = if resource.is_some() {
        " INDEXED BY versions_event_id".to_owned()
    } else {
        let family = if origins { "origin" } else { "event" };
        match (own, kind) {
            (true, true) => format!(" INDEXED BY versions_{family}_owner_kind"),
            (true, false) => format!(" INDEXED BY versions_{family}_owner"),
            (false, true) => format!(" INDEXED BY versions_{family}_kind"),
            (false, false) if origins => " INDEXED BY versions_origin_seq".into(),
            (false, false) => String::new(),
        }
    };
    let columns = if origins {
        "v.id, v.seq"
    } else {
        "v.*, v.seq AS page_position"
    };
    let stream = |owner: &str| {
        format!(
            "SELECT {columns} FROM versions v{index} WHERE {conditions}{owner} ORDER BY v.seq LIMIT ?2"
        )
    };
    let sql = if own {
        arguments.push(cursor.actor.clone().into());
        let actor = stream(&format!(" AND v.owner = ?{}", arguments.len()));
        if cursor.actor == policy::SYSTEM {
            actor
        } else {
            let system = stream(" AND v.owner = 'system'");
            format!(
                "WITH owned AS ({actor}), shared AS ({system})
                 SELECT * FROM owned UNION ALL SELECT * FROM shared ORDER BY seq LIMIT ?2"
            )
        }
    } else {
        stream("")
    };
    // Ownership and kind are immutable for a resource. Page over first
    // revisions, then seek each selected resource at the snapshot boundary.
    if let Some(high) = high.filter(|_| origins) {
        (
            format!(
                "WITH page AS MATERIALIZED ({sql})
                 SELECT v.*, p.seq AS page_position FROM page p
                   JOIN versions v ON v.seq = (
                     SELECT max(w.seq) FROM versions w INDEXED BY versions_event_id
                      WHERE w.id=p.id AND w.seq <= ?{high})
                 ORDER BY p.seq"
            ),
            arguments,
        )
    } else {
        (sql, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus, params, params_from_iter};

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../state.sql")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(SNAPSHOT_SCHEMA).unwrap();
        conn
    }

    fn cursor(actor: &str, kind: &str, resource: Option<&str>) -> Cursor {
        Cursor {
            mode: "watch".into(),
            actor: actor.into(),
            epoch: "1".into(),
            incarnation: "local".into(),
            kind: kind.into(),
            resource: resource.map(str::to_owned),
            snapshot: "fixture".into(),
            high: 20000,
            position: 0,
            until: 2000,
        }
    }

    fn insert(conn: &Connection, id: &str, kind: &str, owner: &str) -> i64 {
        conn.execute(
            "INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             VALUES(?1,1,1,?2,?3,'{}','{}','now','now',?1,'fixture','local')",
            params![id,kind,owner],
        ).unwrap();
        conn.last_insert_rowid()
    }

    fn page(conn: &Connection, cursor: &Cursor, view: View, limit: u32) -> (Vec<(i64, i64)>, i32) {
        let (sql, values) = selection(cursor, view, limit);
        let mut stmt = conn.prepare(&sql).unwrap();
        let entries = stmt
            .query_map(params_from_iter(values), |row| {
                Ok((row.get("page_position")?, row.get("seq")?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (entries, stmt.get_status(StatementStatus::VmStep))
    }

    fn read(conn: &Connection, cursor: &Cursor, view: View, limit: u32) -> (Vec<i64>, i32) {
        let (entries, steps) = page(conn, cursor, view, limit);
        (entries.into_iter().map(|(_, seq)| seq).collect(), steps)
    }

    #[test]
    fn generic_events_and_snapshots_exclude_private_project_authority() {
        let conn = db();
        let public = insert(&conn, "public", "Task", policy::SYSTEM);
        for kind in ["Project", "WorkItem", "WorkItemEvidence"] {
            insert(&conn, kind, kind, policy::OWNER);
            assert!(
                read(
                    &conn,
                    &cursor(policy::OWNER, kind, Some(kind)),
                    View::Changes,
                    8
                )
                .0
                .is_empty()
            );
        }
        let scope = cursor(policy::OWNER, "", None);
        for view in [View::Changes, View::Snapshot] {
            assert_eq!(read(&conn, &scope, view, 8).0, [public]);
        }
    }

    #[test]
    fn exact_snapshot_and_watch_seek_within_a_large_resource_history() {
        let conn = db();
        conn.execute_batch(
            "WITH RECURSIVE n(rev) AS (VALUES(1) UNION ALL SELECT rev+1 FROM n WHERE rev<8000)
             INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             SELECT 'volume-exact',rev,1,'Volume','agent','{}','{}','now','now','exact-'||rev,'fixture','local' FROM n"
        ).unwrap();
        let mut scope = cursor("agent", "Volume", Some("volume-exact"));
        scope.high = 7990;
        let (before, snapshot_steps) = read(&conn, &scope, View::Snapshot, 2);
        assert_eq!(before, [7990]);
        scope.position = 7998;
        let (events, watch_steps) = read(&conn, &scope, View::Changes, 2);
        assert_eq!(events, [7999, 8000]);
        assert!(
            snapshot_steps < 300 && watch_steps < 300,
            "{snapshot_steps}, {watch_steps}"
        );
        scope.actor = "outsider".into();
        assert!(read(&conn, &scope, View::Changes, 2).0.is_empty());
    }

    #[test]
    fn own_and_system_pages_skip_wrong_owners_and_kinds_in_source_order() {
        let conn = db();
        conn.execute_batch(
            "WITH RECURSIVE n(rev) AS (VALUES(1) UNION ALL SELECT rev+1 FROM n WHERE rev<8000)
             INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             SELECT 'noise-'||rev,1,1,CASE rev%2 WHEN 0 THEN 'Task' ELSE 'Input' END,
                 CASE rev%2 WHEN 0 THEN 'foreign' ELSE 'agent' END,
                 '{}','{}','now','now','noise-'||rev,'fixture','local' FROM n"
        ).unwrap();
        let public = insert(&conn, "task-public", "Task", policy::SYSTEM);
        let alone = insert(&conn, "task-owned", "Task", "agent");
        let last = insert(&conn, "task-later", "Task", policy::SYSTEM);
        let mut scope = cursor("agent", "Task", None);
        let (page, steps) = read(&conn, &scope, View::Changes, 2);
        assert_eq!(page, [public, alone]);
        assert!(steps < 300, "{steps}");
        scope.position = alone;
        assert_eq!(read(&conn, &scope, View::Changes, 2).0, [last]);
        scope.position = 0;
        scope.high = alone;
        assert_eq!(read(&conn, &scope, View::Snapshot, 4).0, [public, alone]);
        scope.actor = policy::SYSTEM.into();
        assert_eq!(read(&conn, &scope, View::Changes, 4).0, [public, last]);
    }

    #[test]
    fn snapshot_pages_seek_origins_and_their_boundary_revisions() {
        let conn = db();
        let first = insert(&conn, "changing", "Task", "agent");
        let hidden = insert(&conn, "hidden", "Task", "other");
        let public = insert(&conn, "public", "Task", policy::SYSTEM);
        let wrong_kind = insert(&conn, "wrong-kind", "Input", "agent");
        let second = insert(&conn, "second", "Task", "agent");
        conn.execute_batch(
            "WITH RECURSIVE n(rev) AS (VALUES(2) UNION ALL SELECT rev+1 FROM n WHERE rev<12000)
             INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
             SELECT 'changing',rev,1,'Task','agent','{}','{}','now','now','change-'||rev,'fixture','local' FROM n",
        ).unwrap();
        let current_revision = conn.last_insert_rowid();
        let created_after = insert(&conn, "created-after", "Task", "agent");
        let mut scope = cursor("agent", "Task", None);
        scope.snapshot = format!("{}test", super::super::ORDERED_SNAPSHOT_PREFIX);
        scope.high = current_revision - 1;
        let (items, first_steps) = page(&conn, &scope, View::Snapshot, 2);
        assert_eq!(items, [(first, current_revision - 1), (public, public)]);
        scope.position = public;
        let (items, next_steps) = page(&conn, &scope, View::Snapshot, 2);
        assert_eq!(items, [(second, second)]);
        assert!(
            first_steps < 400 && next_steps < 400,
            "{first_steps}, {next_steps}"
        );
        scope.position = 0;
        scope.kind.clear();
        let (items, steps) = page(&conn, &scope, View::Snapshot, 5);
        assert_eq!(
            items,
            [
                (first, current_revision - 1),
                (public, public),
                (wrong_kind, wrong_kind),
                (second, second)
            ]
        );
        assert!(steps < 400, "{steps}");
        scope.actor = policy::OWNER.into();
        let (items, steps) = page(&conn, &scope, View::Snapshot, 8);
        assert_eq!(
            items.iter().map(|(origin, _)| *origin).collect::<Vec<_>>(),
            [first, hidden, public, wrong_kind, second]
        );
        assert!(steps < 400, "{steps}");
        scope.actor = policy::SYSTEM.into();
        assert_eq!(page(&conn, &scope, View::Snapshot, 8).0, [(public, public)]);
        scope.actor = "agent".into();
        scope.position = scope.high;
        assert_eq!(
            read(&conn, &scope, View::Changes, 8).0,
            [current_revision, created_after]
        );
    }
}
