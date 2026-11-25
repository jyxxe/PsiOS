//! Keep each authorized event stream ordered and bound its work to the query scope.
use super::Cursor;
use crate::policy;
use rusqlite::types::Value;

pub(crate) const FORMAT: u32 = crate::apps::restart::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("queries.sql");

#[derive(Clone, Copy)]
pub(super) enum View {
    Snapshot,
    Changes,
}

pub(super) fn selection(cursor: &Cursor, view: View, limit: u32) -> (String, Vec<Value>) {
    let mut arguments = vec![cursor.position.into(), i64::from(limit).into()];
    let mut conditions = String::from("v.seq > ?1");
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
    if matches!(view, View::Snapshot) {
        arguments.push(cursor.high.into());
        let high = arguments.len();
        if let Some(resource) = resource {
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
    }
    let own = cursor.actor != policy::OWNER;
    let index = match (resource.is_some(), own, kind) {
        (true, _, _) => " INDEXED BY versions_event_id",
        (false, true, true) => " INDEXED BY versions_event_owner_kind",
        (false, true, false) => " INDEXED BY versions_event_owner",
        (false, false, true) => " INDEXED BY versions_event_kind",
        (false, false, false) => "",
    };
    let stream = |owner: &str| {
        format!(
            "SELECT v.* FROM versions v{index} WHERE {conditions}{owner} ORDER BY v.seq LIMIT ?2"
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
    (sql, arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, StatementStatus, params, params_from_iter};

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../state.sql")).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
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

    fn read(conn: &Connection, cursor: &Cursor, view: View, limit: u32) -> (Vec<i64>, i32) {
        let (sql, values) = selection(cursor, view, limit);
        let mut stmt = conn.prepare(&sql).unwrap();
        let sequences = stmt
            .query_map(params_from_iter(values), |row| row.get::<_, i64>("seq"))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        (sequences, stmt.get_status(StatementStatus::VmStep))
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
}
