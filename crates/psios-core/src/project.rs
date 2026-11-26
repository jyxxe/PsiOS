//! Private project membership, work-item dependencies, and local review decisions.
pub(crate) mod evidence;

use crate::{
    ById, Result,
    db::{Db, Doc, row},
    decode, err, label, now,
    policy::{
        self, PROJECT_CONTRIBUTE as CONTRIBUTE, PROJECT_MANAGE as MANAGE, PROJECT_READ as READ,
        PROJECT_REVIEW as REVIEW,
    },
    require,
};
use psios_api::{
    CreateProject, CreateWorkItem, ProjectRole as Role, SetProjectMember, SubmitWorkItemEvidence,
    WorkItemCriterion as Criterion, canonical, sha256,
};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const FORMAT: u32 = crate::agent::provider_authority::FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE INDEX project_work_items ON versions((spec->>'project_id'),id,rev) WHERE kind='WorkItem';";
const PROJECT: &str = "Project";
const ITEM: &str = "WorkItem";
const MAX_PROJECTS: i64 = 128;
const MAX_MEMBERS: usize = 32;
const MAX_ITEMS: i64 = 256;
const MAX_BLOCKERS: usize = 16;
const PAGE_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListItems {
    project_id: String,
    after_id: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    id: String,
    expected_resource_version: String,
    accept: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Conditional {
    id: String,
    expected_resource_version: String,
}

fn denied() -> crate::Error {
    err(
        "PermissionDenied",
        "project resource is outside your authorized membership",
    )
}

fn invalid_state() -> crate::Error {
    err(
        "RecoveryRequired",
        "private project authority state is inconsistent",
    )
}

fn text_ok(title: &str, description: &str) -> bool {
    !title.trim().is_empty()
        && title.len() <= 128
        && !title.chars().any(char::is_control)
        && description.len() <= 2048
        && description
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
}

fn digest(input: &Value) -> String {
    sha256(&canonical(input))
}

fn stable_id(prefix: &str, input: &Value) -> String {
    format!("{prefix}-{}", &digest(input)[7..])
}

fn members(project: &Doc) -> Result<BTreeMap<String, Role>> {
    decode(&project.body["members"]).map_err(|_| invalid_state())
}

fn role_allows(project: &Doc, actor: &str, action: &str) -> Result<bool> {
    Ok(actor == project.owner
        || members(project)?.get(actor).is_some_and(|role| {
            action == READ
                || matches!(
                    (*role, action),
                    (Role::Manager, MANAGE)
                        | (Role::Contributor, CONTRIBUTE)
                        | (Role::Reviewer, REVIEW)
                )
        }))
}

fn access(db: &Db<'_>, actor: &str, id: &str, action: &str) -> Result<Doc> {
    let project = db
        .find(id)?
        .filter(|project| project.kind == PROJECT)
        .ok_or_else(denied)?;
    if !role_allows(&project, actor, action)? {
        return Err(denied());
    }
    policy::active(db, actor)?;
    policy::can_target(db, actor, READ, &project.id)?;
    if action != READ {
        policy::can_target(db, actor, action, &project.id)?;
    }
    Ok(project)
}

fn project_view(project: &Doc, actor: &str) -> Result<Value> {
    let enrolled = members(project)?;
    let list = enrolled
        .iter()
        .map(|(id, role)| json!({"principal_id":id,"role":role}))
        .collect::<Vec<_>>();
    let role = if actor == project.owner {
        json!("Owner")
    } else {
        json!(enrolled.get(actor))
    };
    Ok(json!({"reference":project.reference(),"value":{
        "schema_version":psios_api::VERSION,"title":project.spec["title"],"description":project.spec["description"],
        "owner_principal_id":project.owner,"role":role,"members":list,
        "created_at":project.created,"updated_at":project.stamp,"updated_by_principal_id":project.body["updated_by_principal_id"],
    }}))
}

fn lookup_item(db: &Db<'_>, id: &str) -> Result<Doc> {
    db.find(id)?
        .filter(|item| item.kind == ITEM)
        .ok_or_else(denied)
}

fn project_id(item: &Doc) -> Result<&str> {
    item.spec["project_id"].as_str().ok_or_else(invalid_state)
}

fn assigned(item: &Doc) -> Result<&str> {
    item.spec["assignee_principal_id"]
        .as_str()
        .ok_or_else(invalid_state)
}

fn access_item(db: &Db<'_>, actor: &str, id: &str, action: &str) -> Result<(Doc, Doc)> {
    let item = lookup_item(db, id)?;
    let project = access(db, actor, project_id(&item)?, action)?;
    require(
        item.owner == project.owner,
        "RecoveryRequired",
        "work item owner differs from its project authority",
    )?;
    Ok((project, item))
}

fn blocker_ids(item: &Doc) -> Result<Vec<String>> {
    decode(&item.spec["blocked_by"]).map_err(|_| invalid_state())
}

fn unresolved(db: &Db<'_>, item: &Doc) -> Result<Vec<Value>> {
    let mut blocked = Vec::new();
    for id in blocker_ids(item)? {
        let blocker = db
            .find(&id)?
            .filter(|blocker| {
                blocker.kind == ITEM
                    && blocker.owner == item.owner
                    && blocker.spec["project_id"] == item.spec["project_id"]
            })
            .ok_or_else(invalid_state)?;
        if blocker.body["lifecycle"] != "Closed" {
            blocked.push(json!({"id":id,"lifecycle":blocker.body["lifecycle"]}));
        }
    }
    Ok(blocked)
}

fn criterion_view(db: &Db<'_>, item: &Doc, actor: &str) -> Result<Value> {
    let mut criterion = item.spec["criterion"].clone();
    for action in ["resource.read", "task.read"] {
        match policy::can(db, actor, action) {
            Ok(()) => (),
            Err(error) if error.0.code == "PermissionDenied" => {
                criterion
                    .as_object_mut()
                    .ok_or_else(invalid_state)?
                    .remove("target_ref");
                break;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(criterion)
}

fn item_view(db: &Db<'_>, item: &Doc, actor: &str, include_evidence: bool) -> Result<Value> {
    let mut value = json!({
        "schema_version":psios_api::VERSION,"project_id":item.spec["project_id"],"title":item.spec["title"],
        "description":item.spec["description"],"assignee_principal_id":item.spec["assignee_principal_id"],
        "blocked_by":item.spec["blocked_by"],"blockers":unresolved(db,item)?,"criterion":criterion_view(db, item, actor)?,
        "lifecycle":item.body["lifecycle"],"has_evidence":item.body.get("evidence_ref").is_some(),
        "review":item.body["review"],"created_at":item.created,"updated_at":item.stamp,
        "created_by_principal_id":item.spec["created_by_principal_id"],"updated_by_principal_id":item.body["updated_by_principal_id"],
    });
    if include_evidence && let Some(evidence) = evidence::view(db, item, actor)? {
        value["evidence"] = evidence;
    }
    Ok(json!({"reference":item.reference(),"value":value}))
}

pub(crate) fn create_project(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    policy::active(db, actor)?;
    let request: CreateProject = decode(params)?;
    let description = request.description.unwrap_or_default();
    label(&request.key)?;
    require(
        text_ok(&request.title, &description),
        "InvalidArgument",
        "invalid project title or description",
    )?;
    let id = stable_id(
        "project",
        &json!([db.meta("authority")?, actor, request.key]),
    );
    let fields =
        json!({"title":request.title,"description":description,"created_by_principal_id":actor});
    let creation = digest(&fields);
    if let Some(existing) = db.find(&id)? {
        require(
            existing.kind == PROJECT
                && existing.owner == actor
                && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "project creation key already binds a different project",
        )?;
        if existing.rev != 1 {
            policy::can_target(db, actor, READ, &id)?;
        }
        return project_view(&existing, actor);
    }
    let count: i64 = db.c.query_row(
        "SELECT count(*) FROM current WHERE kind='Project'",
        [],
        |row| row.get(0),
    )?;
    require(
        count < MAX_PROJECTS,
        "QuotaExceeded",
        "local project limit reached",
    )?;
    db.cfg.check_admission_space()?;
    let mut spec = fields;
    spec["creation_digest"] = json!(creation);
    let project = db.create(
        PROJECT,
        &id,
        actor,
        spec,
        json!({"members":{},"updated_by_principal_id":actor}),
    )?;
    project_view(&project, actor)
}

pub(crate) fn get_project(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ById = decode(params)?;
    project_view(&access(db, actor, &request.id, READ)?, actor)
}

pub(crate) fn set_member(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: SetProjectMember = decode(params)?;
    let project = access(db, actor, &request.id, MANAGE)?;
    project.cas(&request.expected_resource_version)?;
    require(
        request.principal_id != project.owner && request.principal_id != policy::SYSTEM,
        "InvalidArgument",
        "reserved project principals have fixed membership",
    )?;
    let mut enrolled = members(&project)?;
    if let Some(role) = request.role {
        policy::active(db, &request.principal_id)?;
        require(
            enrolled.contains_key(&request.principal_id) || enrolled.len() < MAX_MEMBERS,
            "QuotaExceeded",
            "project membership limit reached",
        )?;
        enrolled.insert(request.principal_id, role);
    } else {
        enrolled.remove(&request.principal_id);
    }
    let members = serde_json::to_value(enrolled)?;
    if project.body["members"] == members {
        return project_view(&project, actor);
    }
    let updated = db.status(
        &project,
        json!({"members":members,"updated_by_principal_id":actor}),
    )?;
    // Access to the resulting projection depends on the role after a self-change.
    if role_allows(&updated, actor, READ)? {
        return project_view(&updated, actor);
    }
    Ok(json!({"reference":updated.reference(),"value":null}))
}

fn criterion_target(conn: &Connection, criterion: &Criterion) -> Result<()> {
    let Criterion::TaskResult { tool, target_ref } = criterion;
    let (kind, id) = match tool.as_str() {
        "system.status" => ("Profile", policy::PROFILE.to_owned()),
        "node.inventory"
        | "node.pressure"
        | "node.interfaces"
        | "node.routes"
        | "node.route_lookup"
        | "node.routing_rules"
        | "node.devices"
        | "node.block_layout"
        | "node.mounts"
        | "node.processes"
        | "node.process_inspect" => (
            "Node",
            conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |row| {
                row.get(0)
            })?,
        ),
        _ => {
            return Err(err(
                "UnsupportedCapability",
                "work item criteria support local system status and node observations",
            ));
        }
    };
    require(
        target_ref.resource_id == id,
        "InvalidArgument",
        "criterion target does not belong to its Task tool",
    )?;
    let reference = serde_json::to_value(target_ref)?;
    let target = crate::db::resolve(conn, &reference)?;
    require(
        target.kind == kind && target.owner == policy::SYSTEM && target.reference() == reference,
        "InvalidArgument",
        "invalid fixed work item criterion target",
    )?;
    Ok(())
}

pub(crate) fn create_item(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: CreateWorkItem = decode(params)?;
    let description = request.description.unwrap_or_default();
    let project = access(db, actor, &request.project_id, MANAGE)?;
    label(&request.key)?;
    require(
        text_ok(&request.title, &description),
        "InvalidArgument",
        "invalid work item title or description",
    )?;
    let id = stable_id(
        "work-item",
        &json!([db.meta("authority")?, project.id, actor, request.key]),
    );
    let blockers = request.blocked_by.iter().collect::<BTreeSet<_>>();
    require(
        blockers.len() == request.blocked_by.len()
            && blockers.len() <= MAX_BLOCKERS
            && !blockers.contains(&id),
        "InvalidArgument",
        "work item blockers must be distinct, bounded and exclude the item itself",
    )?;
    let fields = json!({"project_id":project.id,"title":request.title,"description":description,
        "assignee_principal_id":request.assignee_principal_id,"blocked_by":blockers,"criterion":request.criterion,
        "created_by_principal_id":actor});
    let creation = digest(&fields);
    if let Some(existing) = db.find(&id)? {
        require(
            existing.kind == ITEM
                && existing.owner == project.owner
                && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "work item creation key already binds a different item",
        )?;
        return item_view(db, &existing, actor, true);
    }
    project.cas(&request.expected_project_version)?;
    policy::active(db, &request.assignee_principal_id)?;
    require(
        role_allows(&project, &request.assignee_principal_id, CONTRIBUTE)?,
        "PermissionDenied",
        "assignee is not an enrolled contributor",
    )?;
    criterion_target(db.c, &request.criterion)?;
    let count: i64 = db.c.query_row(
        "SELECT count(*) FROM current WHERE kind='WorkItem' AND spec->>'project_id'=?1",
        [&project.id],
        |row| row.get(0),
    )?;
    require(
        count < MAX_ITEMS,
        "QuotaExceeded",
        "project work item limit reached",
    )?;
    // Every immutable blocker already exists in the same project. No create can add a cycle.
    for blocker in blockers {
        label(blocker)?;
        require(
            db.find(blocker)?.is_some_and(|item| {
                item.kind == ITEM
                    && item.spec["project_id"] == project.id
                    && item.owner == project.owner
            }),
            "InvalidArgument",
            "every work item blocker must belong to the same project",
        )?;
    }
    db.cfg.check_admission_space()?;
    let mut spec = fields;
    spec["creation_digest"] = json!(creation);
    let item = db.create(
        ITEM,
        &id,
        &project.owner,
        spec,
        json!({"lifecycle":"Ready","updated_by_principal_id":actor}),
    )?;
    item_view(db, &item, actor, true)
}

pub(crate) fn get_item(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ById = decode(params)?;
    let (_, item) = access_item(db, actor, &request.id, READ)?;
    item_view(db, &item, actor, true)
}

pub(crate) fn list_items(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ListItems = decode(params)?;
    let project = access(db, actor, &request.project_id, READ)?;
    let limit = request.limit.unwrap_or(32);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "work item page size must be 1 to 64",
    )?;
    let after = request.after_id.as_deref().unwrap_or_default();
    if request.after_id.is_some() {
        label(after)?;
        require(
            db.find(after)?.is_some_and(|item| {
                item.kind == ITEM
                    && item.owner == project.owner
                    && item.spec["project_id"] == project.id
            }),
            "InvalidArgument",
            "cursor is outside the project",
        )?;
    }
    let mut statement = db.c.prepare("SELECT * FROM current WHERE kind='WorkItem' AND spec->>'project_id'=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
    let items = statement
        .query_map(params![project.id, after, i64::from(limit + 1)], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut size = 0;
    let mut page = Vec::new();
    for item in items.iter().take(limit as usize) {
        require(
            item.owner == project.owner,
            "RecoveryRequired",
            "work item owner differs from its project authority",
        )?;
        let view = item_view(db, item, actor, false)?;
        size += serde_json::to_vec(&view)?.len();
        if size > PAGE_BYTES {
            break;
        }
        page.push(view);
    }
    require(
        items.is_empty() || !page.is_empty(),
        "QuotaExceeded",
        "work item exceeds page byte limit",
    )?;
    let next_after_id = (page.len() < items.len()).then(|| &items[page.len() - 1].id);
    Ok(json!({"items":page,"next_after_id":next_after_id}))
}

pub(crate) fn submit_evidence(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: SubmitWorkItemEvidence = decode(params)?;
    let (_, item) = access_item(db, actor, &request.id, CONTRIBUTE)?;
    item.cas(&request.expected_resource_version)?;
    require(
        assigned(&item)? == actor,
        "PermissionDenied",
        "only the enrolled assignee can submit work item evidence",
    )?;
    require(
        item.body["lifecycle"] == "Ready" && item.body.get("evidence_ref").is_none(),
        "PreconditionFailed",
        "work item does not accept an evidence submission in this state",
    )?;
    require(
        unresolved(db, &item)?.is_empty(),
        "PreconditionFailed",
        "work item has unresolved blockers",
    )?;
    db.cfg.check_admission_space()?;
    let evidence = evidence::attach(
        db,
        &item,
        actor,
        &serde_json::to_value(request.task_ref)?,
        &serde_json::to_value(request.artifact_ref)?,
    )?;
    let mut state = item.body.clone();
    state["lifecycle"] = json!("InReview");
    state["evidence_ref"] = evidence.reference();
    state["updated_by_principal_id"] = json!(actor);
    let fields = state.as_object_mut().ok_or_else(invalid_state)?;
    for key in ["review", "reviewed_item_ref", "reviewed_evidence_ref"] {
        fields.remove(key);
    }
    item_view(db, &db.status(&item, state)?, actor, true)
}

pub(crate) fn review(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Review = decode(params)?;
    let (_, item) = access_item(db, actor, &request.id, REVIEW)?;
    item.cas(&request.expected_resource_version)?;
    require(
        item.body["lifecycle"] == "InReview",
        "PreconditionFailed",
        "work item has no pending review",
    )?;
    require(
        assigned(&item)? != actor,
        "PermissionDenied",
        "work item review requires an independent principal",
    )?;
    let evidence = evidence::review(db, &item, actor)?;
    let mut state = item.body.clone();
    state["review"] =
        json!({"reviewer_principal_id":actor,"accepted":request.accept,"reviewed_at":now()});
    state["reviewed_item_ref"] = item.reference();
    state["reviewed_evidence_ref"] = evidence.reference();
    state["updated_by_principal_id"] = json!(actor);
    if request.accept {
        state["lifecycle"] = json!("Verified");
    } else {
        evidence::release(db, &item, &evidence)?;
        state["lifecycle"] = json!("Ready");
        state
            .as_object_mut()
            .expect("work item state")
            .remove("evidence_ref");
    }
    item_view(db, &db.status(&item, state)?, actor, true)
}

pub(crate) fn close(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Conditional = decode(params)?;
    let (_, item) = access_item(db, actor, &request.id, MANAGE)?;
    item.cas(&request.expected_resource_version)?;
    require(
        item.body["lifecycle"] == "Verified" && item.body["review"]["accepted"] == true,
        "PreconditionFailed",
        "closing a work item requires independent verified evidence",
    )?;
    let evidence = db.resolve(&item.body["evidence_ref"])?;
    evidence::release(db, &item, &evidence)?;
    let mut state = item.body.clone();
    state["lifecycle"] = json!("Closed");
    state["updated_by_principal_id"] = json!(actor);
    item_view(db, &db.status(&item, state)?, actor, true)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let projects = conn
        .prepare("SELECT * FROM current WHERE kind='Project' ORDER BY id LIMIT 129")?
        .query_map([], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    require(
        projects.len() as i64 <= MAX_PROJECTS,
        "RecoveryRequired",
        "local project limit is inconsistent",
    )?;
    let owners = projects
        .iter()
        .map(|project| (project.id.as_str(), project.owner.as_str()))
        .collect::<BTreeMap<_, _>>();
    for project in &projects {
        let roster = members(project)?;
        require(
            project.owner == policy::OWNER
                && roster.len() <= MAX_MEMBERS
                && !roster.contains_key(&project.owner)
                && !roster.contains_key(policy::SYSTEM),
            "RecoveryRequired",
            "invalid project membership roster",
        )?;
        verify_document(conn, project)?;
        for principal in roster.keys() {
            verify_principal(conn, principal)?;
        }
    }
    let mut statement = conn.prepare("SELECT * FROM current WHERE kind='WorkItem' ORDER BY seq")?;
    let mut rows = statement.query([])?;
    let mut counts = BTreeMap::new();
    let mut seen = BTreeMap::new();
    while let Some(record) = rows.next()? {
        let item = row(record)?;
        let project = project_id(&item)?.to_owned();
        let count = counts.entry(project.clone()).or_insert(0);
        *count += 1;
        verify_document(conn, &item)?;
        let created = conn.query_row(
            "SELECT seq FROM versions WHERE id=?1 AND rev=1 AND kind='WorkItem'",
            [&item.id],
            |row| row.get::<_, i64>(0),
        )?;
        let blockers = blocker_ids(&item)?;
        require(
            owners.get(project.as_str()) == Some(&item.owner.as_str())
                && *count <= MAX_ITEMS
                && blockers.len() <= MAX_BLOCKERS
                && blockers.windows(2).all(|pair| pair[0] < pair[1]),
            "RecoveryRequired",
            "invalid work item authority or blocker scope",
        )?;
        verify_principal(conn, assigned(&item)?)?;
        let criterion: Criterion = decode(&item.spec["criterion"]).map_err(|_| invalid_state())?;
        criterion_target(conn, &criterion).map_err(|_| invalid_state())?;
        let lifecycle = item.body["lifecycle"].as_str();
        let evidence = item.body.get("evidence_ref");
        require(
            match lifecycle {
                Some("Ready") => evidence.is_none() && item.body["review"]["accepted"] != true,
                Some("InReview") => {
                    evidence.is_some_and(Value::is_object) && item.body.get("review").is_none()
                }
                Some("Verified" | "Closed") => {
                    evidence.is_some_and(Value::is_object)
                        && item.body["review"]["accepted"] == true
                }
                _ => false,
            },
            "RecoveryRequired",
            "invalid work item lifecycle and review state",
        )?;
        if let Some(review) = item.body.get("review") {
            verify_review(conn, &item, review)?;
        } else {
            require(
                item.body.get("reviewed_item_ref").is_none()
                    && item.body.get("reviewed_evidence_ref").is_none(),
                "RecoveryRequired",
                "work item has a review binding without a decision",
            )?;
        }
        seen.insert(item.id, (project, created, blockers));
    }
    for (project, created, blockers) in seen.values() {
        require(
            blockers.iter().all(|id| {
                seen.get(id)
                    .is_some_and(|(other, seq, _)| other == project && seq < created)
            }),
            "RecoveryRequired",
            "work item graph has a foreign, missing or cyclic blocker",
        )?;
    }
    Ok(())
}

fn verify_document(conn: &Connection, doc: &Doc) -> Result<()> {
    let original = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&doc.id],
        row,
    )?;
    let mut fields = doc.spec.clone();
    let creation = fields
        .as_object_mut()
        .ok_or_else(invalid_state)?
        .remove("creation_digest")
        .ok_or_else(invalid_state)?;
    require(
        doc.kind == original.kind
            && doc.owner == original.owner
            && doc.spec == original.spec
            && creation == digest(&fields)
            && doc.generation == 1
            && text_ok(
                fields["title"].as_str().unwrap_or_default(),
                fields["description"].as_str().ok_or_else(invalid_state)?,
            ),
        "RecoveryRequired",
        "project document creation fields changed",
    )?;
    for key in [
        &fields["created_by_principal_id"],
        &doc.body["updated_by_principal_id"],
    ] {
        verify_principal(conn, key.as_str().ok_or_else(invalid_state)?)?;
    }
    Ok(())
}

fn verify_review(conn: &Connection, item: &Doc, review: &Value) -> Result<()> {
    let reviewer = review["reviewer_principal_id"]
        .as_str()
        .ok_or_else(invalid_state)?;
    let accepted = review["accepted"].as_bool().ok_or_else(invalid_state)?;
    let before = crate::db::resolve(conn, &item.body["reviewed_item_ref"])?;
    let evidence = crate::db::resolve(conn, &item.body["reviewed_evidence_ref"])?;
    let closed = item.body["lifecycle"] == "Closed";
    require(
        reviewer != assigned(item)?
            && reviewer != evidence.body["submitted_by"]
            && review["reviewed_at"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && before.kind == ITEM
            && before.id == item.id
            && before.owner == item.owner
            && before.spec == item.spec
            && before.rev + if closed { 2 } else { 1 } == item.rev
            && before.body["lifecycle"] == "InReview"
            && before.body["evidence_ref"] == evidence.reference()
            && evidence.kind == evidence::KIND
            && evidence.body["work_item_id"] == item.id
            && evidence.body["project_id"] == item.spec["project_id"]
            && accepted == matches!(item.body["lifecycle"].as_str(), Some("Verified" | "Closed"))
            && (!accepted || item.body["evidence_ref"] == evidence.reference()),
        "RecoveryRequired",
        "work item decision is not bound to its independent review and evidence",
    )?;
    verify_principal(conn, reviewer)
}

fn verify_principal(conn: &Connection, id: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current WHERE id=?1 AND kind='Principal')",
        [id],
        |row| row.get(0),
    )?;
    require(
        exists,
        "RecoveryRequired",
        "project principal binding is missing",
    )
}
