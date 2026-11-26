//! Project-local closure dependencies between parent, child and blocker work items.
use super::{
    ITEM, MANAGE, MAX_BLOCKERS, MAX_ITEMS, MAX_PROJECTS, PROJECT, READ, access_item, invalid_state,
    item_page, role_allows,
};
use crate::{
    Result,
    db::{Db, Doc, find, row},
    decode, label, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const FORMAT: u32 = super::workflow_proof::FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE INDEX project_work_item_children
    ON versions((spec->>'project_id'),(spec->>'parent_work_item_id'),id,rev)
    WHERE kind='WorkItem' AND spec->>'parent_work_item_id' IS NOT NULL;";

const CHILDREN_PAGE: &str = "SELECT child.*
    FROM versions child INDEXED BY project_work_item_children
    WHERE child.kind='WorkItem'
      AND child.spec->>'parent_work_item_id' IS NOT NULL
      AND child.spec->>'parent_work_item_id'=?1 AND child.spec->>'project_id'=?2
      AND child.id>?3
      AND NOT EXISTS (SELECT 1 FROM versions newer WHERE newer.id=child.id AND newer.rev>child.rev)
    ORDER BY child.id LIMIT ?4";

fn parent_id(item: &Doc) -> Result<Option<&str>> {
    item.spec
        .get("parent_work_item_id")
        .map(|parent| parent.as_str().ok_or_else(invalid_state))
        .transpose()
}

fn in_project(item: &Doc, project: &Doc) -> bool {
    item.kind == ITEM && item.owner == project.owner && item.spec["project_id"] == project.id
}

fn project_items(conn: &Connection, project: &Doc) -> Result<Vec<Doc>> {
    let items = conn
        .prepare_cached(
            "SELECT * FROM current WHERE kind='WorkItem' AND spec->>'project_id'=?1 LIMIT ?2",
        )?
        .query_map(params![project.id, MAX_ITEMS + 1], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    require(
        items.len() as i64 <= MAX_ITEMS && items.iter().all(|item| in_project(item, project)),
        "RecoveryRequired",
        "invalid hierarchy project scope or work item count",
    )?;
    Ok(items)
}

/// Edges point from a work item to the work that must complete before it can close.
type Graph<'a> = BTreeMap<&'a str, Vec<&'a str>>;

fn graph(items: &[Doc]) -> Result<Graph<'_>> {
    let mut edges = Graph::new();
    for item in items {
        let blockers = item.spec["blocked_by"]
            .as_array()
            .ok_or_else(invalid_state)?;
        require(
            blockers.len() <= MAX_BLOCKERS,
            "RecoveryRequired",
            "invalid hierarchy blocker count",
        )?;
        let blockers = blockers
            .iter()
            .map(|blocker| blocker.as_str().ok_or_else(invalid_state))
            .collect::<Result<Vec<_>>>()?;
        edges.insert(item.id.as_str(), blockers);
    }
    for item in items {
        if let Some(parent) = parent_id(item)? {
            require(
                label(parent).is_ok(),
                "RecoveryRequired",
                "invalid parent work item identifier",
            )?;
            let children = edges.get_mut(parent).ok_or_else(invalid_state)?;
            children.push(&item.id);
        }
    }
    Ok(edges)
}

fn reaches(edges: &Graph<'_>, blockers: &[String], parent: &str) -> Result<bool> {
    let mut visited = BTreeSet::new();
    let mut pending = blockers.iter().map(String::as_str).collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        if id == parent {
            return Ok(true);
        }
        if visited.insert(id) {
            pending.extend(edges.get(id).ok_or_else(invalid_state)?.iter().copied());
        }
    }
    Ok(false)
}

pub(super) fn validate_new_child(
    db: &Db<'_>,
    project: &Doc,
    parent_id: &str,
    blockers: &[String],
) -> Result<()> {
    if blockers.is_empty() {
        return Ok(());
    }
    let items = project_items(db.c, project)?;
    let edges = graph(&items)?;
    // The new parent→child edge can cycle only through an outgoing blocker of the new child.
    require(
        !reaches(&edges, blockers, parent_id)?,
        "InvalidArgument",
        "work item parent and blockers create a dependency cycle",
    )
}

fn child_rows(
    conn: &Connection,
    project: &Doc,
    parent: &Doc,
    after: &str,
    count: i64,
) -> Result<Vec<Doc>> {
    let children = conn
        .prepare_cached(CHILDREN_PAGE)?
        .query_map(params![parent.id, project.id, after, count], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    require(
        children.iter().all(|item| in_project(item, project)),
        "RecoveryRequired",
        "child work item owner differs from its project authority",
    )?;
    Ok(children)
}

pub(super) fn require_children_closed(db: &Db<'_>, project: &Doc, parent: &Doc) -> Result<()> {
    let children = child_rows(db.c, project, parent, "", MAX_ITEMS + 1)?;
    require(
        children.len() as i64 <= MAX_ITEMS,
        "RecoveryRequired",
        "invalid work item child count",
    )?;
    require(
        children
            .iter()
            .all(|child| child.body["lifecycle"] == "Closed"),
        "PreconditionFailed",
        "work item has children that are not closed",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Children {
    id: String,
    after_id: Option<String>,
    limit: Option<u32>,
}

pub(crate) fn children(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Children = decode(params)?;
    let (project, parent) = access_item(db, actor, &request.id, READ)?;
    let limit = request.limit.unwrap_or(32);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "work item page size must be 1 to 64",
    )?;
    if let Some(after) = &request.after_id {
        label(after)?;
        require(
            db.find(after)?.is_some_and(|item| {
                in_project(&item, &project) && item.spec["parent_work_item_id"] == parent.id
            }),
            "InvalidArgument",
            "cursor is outside the work item's children",
        )?;
    }
    let children = child_rows(
        db.c,
        &project,
        &parent,
        request.after_id.as_deref().unwrap_or_default(),
        i64::from(limit + 1),
    )?;
    item_page(db, actor, &project, &children, limit)
}

fn acyclic(edges: &Graph<'_>) -> Result<()> {
    let mut incoming = edges
        .keys()
        .map(|&id| (id, 0usize))
        .collect::<BTreeMap<_, _>>();
    for &target in edges.values().flatten() {
        *incoming.get_mut(target).ok_or_else(invalid_state)? += 1;
    }
    let mut ready = incoming
        .iter()
        .filter_map(|(&id, &count)| (count == 0).then_some(id))
        .collect::<Vec<_>>();
    let mut visited = 0;
    while let Some(id) = ready.pop() {
        visited += 1;
        for &target in &edges[id] {
            let count = incoming.get_mut(target).ok_or_else(invalid_state)?;
            *count -= 1;
            if *count == 0 {
                ready.push(target);
            }
        }
    }
    require(
        visited == edges.len(),
        "RecoveryRequired",
        "work item hierarchy has a dependency cycle",
    )
}

fn revision(conn: &Connection, item_id: &str, rev: i64) -> Result<(i64, Doc)> {
    conn.prepare_cached("SELECT * FROM versions WHERE id=?1 AND rev=?2")?
        .query_row(params![item_id, rev], |r| Ok((r.get("seq")?, row(r)?)))
        .optional()?
        .ok_or_else(invalid_state)
}

fn preceding(conn: &Connection, id: &str, seq: i64) -> Result<(i64, Doc)> {
    conn.prepare_cached("SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1")?
        .query_row(params![id, seq], |r| Ok((r.get("seq")?, row(r)?)))
        .optional()?
        .ok_or_else(invalid_state)
}

fn require_manager(conn: &Connection, project: &Doc, actor: &Value, seq: i64) -> Result<()> {
    let actor = actor.as_str().ok_or_else(invalid_state)?;
    let (_, then_project) = preceding(conn, &project.id, seq)?;
    let (_, principal) = preceding(conn, actor, seq)?;
    require(
        then_project.kind == PROJECT
            && then_project.owner == project.owner
            && role_allows(&then_project, actor, MANAGE)?
            && principal.kind == "Principal"
            && principal.body["lifecycle"] == "Active",
        "RecoveryRequired",
        "work item hierarchy transition lacks its active project manager",
    )
}

fn enrollment(conn: &Connection, project: &Doc, child: &Doc, parent: &Doc) -> Result<i64> {
    let (created, original) = revision(conn, &child.id, 1)?;
    let (_, prior_parent) = preceding(conn, &parent.id, created)?;
    require(
        in_project(&original, project)
            && parent_id(&original)? == Some(parent.id.as_str())
            && in_project(&prior_parent, project)
            && prior_parent.body["lifecycle"] == "Ready",
        "RecoveryRequired",
        "child was not enrolled under a ready work item in its project",
    )?;
    require_manager(
        conn,
        project,
        &original.spec["created_by_principal_id"],
        created,
    )?;
    Ok(created)
}

fn check_closure(
    conn: &Connection,
    project: &Doc,
    parent: &Doc,
    children: &[(i64, &Doc)],
) -> Result<()> {
    if parent.body["lifecycle"] != "Closed" {
        return Ok(());
    }
    let (closed, _) = revision(conn, &parent.id, parent.rev)?;
    require_manager(
        conn,
        project,
        &parent.body["updated_by_principal_id"],
        closed,
    )?;
    for &(created, child) in children {
        let (child_seq, prior_child) = preceding(conn, &child.id, closed)?;
        require(
            created < child_seq
                && in_project(&prior_child, project)
                && parent_id(&prior_child)? == Some(parent.id.as_str())
                && prior_child.body["lifecycle"] == "Closed"
                && prior_child.reference() == child.reference(),
            "RecoveryRequired",
            "parent closed before its nominated child work item",
        )?;
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut scoped = conn.prepare(
        "SELECT DISTINCT spec->'project_id' FROM current
         WHERE kind='WorkItem' AND spec->'parent_work_item_id' IS NOT NULL LIMIT ?1",
    )?;
    let scopes = scoped
        .query_map([MAX_PROJECTS + 1], |r| r.get::<_, Option<Value>>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    require(
        scopes.len() as i64 <= MAX_PROJECTS,
        "RecoveryRequired",
        "work item hierarchy crosses its project limit",
    )?;
    for id in scopes {
        let id = id
            .as_ref()
            .and_then(Value::as_str)
            .ok_or_else(invalid_state)?;
        let project = find(conn, id)?
            .filter(|doc| doc.kind == PROJECT)
            .ok_or_else(invalid_state)?;
        let items = project_items(conn, &project)?;
        // Edges are immutable. If the final graph is a DAG, every preceding induced graph was too.
        acyclic(&graph(&items)?)?;
        let by_id = items
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect::<BTreeMap<_, _>>();
        let mut children = BTreeMap::<&str, Vec<(i64, &Doc)>>::new();
        for child in &items {
            if let Some(id) = parent_id(child)? {
                let parent = by_id.get(id).ok_or_else(invalid_state)?;
                let created = enrollment(conn, &project, child, parent)?;
                children.entry(id).or_default().push((created, child));
            }
        }
        for (id, links) in children {
            check_closure(conn, &project, by_id[id], &links)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::OWNER;
    use serde_json::json;

    #[test]
    fn offline_verification_rejects_correct_current_states_in_the_wrong_order() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../state.sql")).unwrap();
        let write =
            |seq: i64, id: &str, rev: i64, kind: &str, spec: &Value, body: Value| {
                conn.execute(
                "INSERT INTO versions VALUES(?1,?2,?3,1,?4,?5,?6,?7,'now','now',?8,'test','test')",
                params![seq, id, rev, kind, OWNER, spec, body, format!("event-{seq}")],
            )
            .unwrap()
            };
        write(
            1,
            "manager",
            1,
            "Principal",
            &json!({}),
            json!({"lifecycle":"Active"}),
        );
        write(
            2,
            "project",
            1,
            PROJECT,
            &json!({}),
            json!({"members":{"manager":"Manager"}}),
        );
        let parent =
            json!({"project_id":"project","created_by_principal_id":"manager","blocked_by":[]});
        let mut child = parent.clone();
        child["parent_work_item_id"] = json!("parent");
        let ready = json!({"lifecycle":"Ready","updated_by_principal_id":"manager"});
        let closed = json!({"lifecycle":"Closed","updated_by_principal_id":"manager"});
        write(3, "parent", 1, ITEM, &parent, ready.clone());
        write(4, "parent", 2, ITEM, &parent, ready.clone());
        write(5, "child", 1, ITEM, &child, ready);
        write(6, "child", 2, ITEM, &child, closed.clone());
        write(7, "parent", 3, ITEM, &parent, closed);
        write(8, "project", 2, PROJECT, &json!({}), json!({"members":{}}));
        verify(&conn).unwrap();

        conn.execute("UPDATE versions SET seq=9 WHERE id='child' AND rev=2", [])
            .unwrap();
        let error = verify(&conn).unwrap_err();
        assert_eq!(error.0.code, "RecoveryRequired");
        assert!(error.0.message.contains("parent closed before"));

        conn.execute("UPDATE versions SET seq=6 WHERE id='child' AND rev=2", [])
            .unwrap();
        conn.execute(
            "UPDATE versions SET body=json_set(body,'$.lifecycle','Verified')
             WHERE id='parent' AND rev=2",
            [],
        )
        .unwrap();
        let error = verify(&conn).unwrap_err();
        assert_eq!(error.0.code, "RecoveryRequired");
        assert!(error.0.message.contains("not enrolled under a ready"));
    }
}
