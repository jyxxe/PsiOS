//! Fixed, project-private groups of work items with a single terminal decision.
pub(crate) mod verify;

use super::{
    ITEM, MANAGE, PAGE_BYTES, READ, access, denied, digest, invalid_state, stable_id, text_ok,
};
use crate::{
    ById, Result,
    db::{Db, Doc, row},
    decode, err, label, now, require,
};
use psios_api::CreateMilestone;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::agent_tool::FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE INDEX project_milestones ON versions((spec->>'project_id'),id,rev) WHERE kind='Milestone';";
const KIND: &str = "Milestone";
const MAX_MILESTONES: i64 = 128;
const MAX_WORK_ITEMS: usize = 32;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    project_id: String,
    after_id: Option<String>,
    limit: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Abandon {
    id: String,
    expected_resource_version: String,
    reason: String,
}

fn creation_fields(
    project_id: &str,
    title: &str,
    description: &str,
    work_item_ids: &[String],
    actor: &str,
    key_digest: &str,
) -> Value {
    json!({"project_id":project_id,"title":title,"description":description,
        "work_item_ids":work_item_ids,"created_by_principal_id":actor,"key_digest":key_digest})
}

fn reason_ok(reason: &str) -> bool {
    !reason.trim().is_empty() && reason.len() <= 512 && !reason.chars().any(char::is_control)
}

fn in_project(doc: &Doc, kind: &str, project: &Doc) -> bool {
    doc.kind == kind && doc.owner == project.owner && doc.spec["project_id"] == project.id
}

fn access_milestone(db: &Db<'_>, actor: &str, id: &str, action: &str) -> Result<(Doc, Doc)> {
    let milestone = db
        .find(id)?
        .filter(|doc| doc.kind == KIND)
        .ok_or_else(denied)?;
    let project = access(db, actor, super::project_id(&milestone)?, action)?;
    require(
        in_project(&milestone, KIND, &project),
        "RecoveryRequired",
        "milestone owner differs from its project authority",
    )?;
    Ok((project, milestone))
}

fn normalized_ids(mut ids: Vec<String>) -> Result<Vec<String>> {
    require(
        (1..=MAX_WORK_ITEMS).contains(&ids.len()),
        "InvalidArgument",
        "milestones require 1 to 32 work items",
    )?;
    for id in &ids {
        label(id)?;
    }
    ids.sort_unstable();
    require(
        ids.windows(2).all(|pair| pair[0] != pair[1]),
        "InvalidArgument",
        "milestone work items must be distinct",
    )?;
    Ok(ids)
}

fn load_group(db: &Db<'_>, project: &Doc, ids: &[String]) -> Result<Option<Vec<Doc>>> {
    let mut members = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(member) = db.find(id)?.filter(|doc| in_project(doc, ITEM, project)) else {
            return Ok(None);
        };
        members.push(member);
    }
    Ok(Some(members))
}

fn group(db: &Db<'_>, project: &Doc, milestone: &Doc) -> Result<Vec<Doc>> {
    let ids: Vec<String> = decode(&milestone.spec["work_item_ids"]).map_err(|_| invalid_state())?;
    if !(1..=MAX_WORK_ITEMS).contains(&ids.len()) || !ids.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(invalid_state());
    }
    load_group(db, project, &ids)?.ok_or_else(invalid_state)
}

fn view(milestone: &Doc, members: &[Doc]) -> Value {
    let unresolved = members
        .iter()
        .filter(|item| item.body["lifecycle"] != "Closed")
        .map(|item| json!({"id":item.id,"lifecycle":item.body["lifecycle"]}))
        .collect::<Vec<_>>();
    let acceptance = milestone.body.get("acceptance").map(|proof| {
        json!({"closed_by_principal_id":proof["closed_by_principal_id"],"closed_at":proof["closed_at"],
            "closed_work_item_refs":proof["closed_work_item_refs"]})
    });
    let abandonment = milestone.body.get("abandonment").map(|proof| {
        json!({"abandoned_by_principal_id":proof["abandoned_by_principal_id"],"abandoned_at":proof["abandoned_at"],
            "reason":proof["reason"]})
    });
    json!({"reference":milestone.reference(),"value":{
        "schema_version":psios_api::VERSION,"project_id":milestone.spec["project_id"],
        "title":milestone.spec["title"],"description":milestone.spec["description"],
        "work_item_ids":milestone.spec["work_item_ids"],"unresolved_work_items":unresolved,
        "lifecycle":milestone.body["lifecycle"],"acceptance":acceptance,"abandonment":abandonment,
        "created_at":milestone.created,"created_by_principal_id":milestone.spec["created_by_principal_id"],
        "updated_at":milestone.stamp,"updated_by_principal_id":milestone.body["updated_by_principal_id"],
    }})
}

pub(crate) fn create(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: CreateMilestone = decode(params)?;
    let project = access(db, actor, &request.project_id, MANAGE)?;
    label(&request.key)?;
    let description = request.description.unwrap_or_default();
    require(
        text_ok(&request.title, &description),
        "InvalidArgument",
        "invalid milestone title or description",
    )?;
    let work_item_ids = normalized_ids(request.work_item_ids)?;
    let key_digest = digest(&json!(request.key));
    let id = stable_id(
        "milestone",
        &json!([db.meta("authority")?, project.id, actor, key_digest]),
    );
    let mut spec = creation_fields(
        &project.id,
        &request.title,
        &description,
        &work_item_ids,
        actor,
        &key_digest,
    );
    let creation = digest(&spec);
    if let Some(existing) = db.find(&id)? {
        require(
            in_project(&existing, KIND, &project) && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "milestone creation key already binds a different milestone",
        )?;
        return Ok(view(&existing, &group(db, &project, &existing)?));
    }
    project.cas(&request.expected_project_version)?;
    let count: i64 = db.c.query_row(
        "SELECT count(*) FROM current WHERE kind='Milestone' AND spec->>'project_id'=?1",
        [&project.id],
        |row| row.get(0),
    )?;
    require(
        count < MAX_MILESTONES,
        "QuotaExceeded",
        "project milestone limit reached",
    )?;
    let members = load_group(db, &project, &work_item_ids)?.ok_or_else(|| {
        err(
            "InvalidArgument",
            "milestone members must be work items in the same project",
        )
    })?;
    db.cfg.check_admission_space()?;
    spec["creation_digest"] = json!(creation);
    spec["creation_project_ref"] = project.reference();
    spec["creation_principal_ref"] = db.of(actor, "Principal")?.reference();
    let milestone = db.create(
        KIND,
        &id,
        &project.owner,
        spec,
        json!({"lifecycle":"Open","updated_by_principal_id":actor}),
    )?;
    Ok(view(&milestone, &members))
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ById = decode(params)?;
    let (project, milestone) = access_milestone(db, actor, &request.id, READ)?;
    Ok(view(&milestone, &group(db, &project, &milestone)?))
}

pub(crate) fn list(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: List = decode(params)?;
    let project = access(db, actor, &request.project_id, READ)?;
    let limit = request.limit.unwrap_or(32);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "milestone page size must be 1 to 64",
    )?;
    let after = request.after_id.as_deref().unwrap_or_default();
    if request.after_id.is_some() {
        label(after)?;
        require(
            db.find(after)?
                .is_some_and(|doc| in_project(&doc, KIND, &project)),
            "InvalidArgument",
            "milestone cursor is outside the project",
        )?;
    }
    let mut query = db.c.prepare_cached(
        "SELECT * FROM current WHERE kind='Milestone' AND spec->>'project_id'=?1 AND id>?2 ORDER BY id LIMIT ?3",
    )?;
    let candidates = query
        .query_map(params![project.id, after, i64::from(limit + 1)], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    let mut bytes = 0;
    for milestone in candidates.iter().take(limit as usize) {
        if !in_project(milestone, KIND, &project) {
            return Err(invalid_state());
        }
        let record = view(milestone, &group(db, &project, milestone)?);
        bytes += serde_json::to_vec(&record)?.len();
        if bytes > PAGE_BYTES {
            break;
        }
        items.push(record);
    }
    require(
        candidates.is_empty() || !items.is_empty(),
        "QuotaExceeded",
        "milestone exceeds page byte limit",
    )?;
    let next_after_id = (items.len() < candidates.len()).then(|| &candidates[items.len() - 1].id);
    Ok(json!({"items":items,"next_after_id":next_after_id}))
}

fn open(milestone: &Doc, expected: &str) -> Result<()> {
    milestone.cas(expected)?;
    require(
        milestone.body["lifecycle"] == "Open",
        "PreconditionFailed",
        "milestone is no longer open",
    )
}

pub(crate) fn close(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: super::Conditional = decode(params)?;
    let (project, milestone) = access_milestone(db, actor, &request.id, MANAGE)?;
    open(&milestone, &request.expected_resource_version)?;
    let members = group(db, &project, &milestone)?;
    require(
        members
            .iter()
            .all(|item| item.body["lifecycle"] == "Closed"),
        "PreconditionFailed",
        "every nominated work item must be closed",
    )?;
    db.cfg.check_admission_space()?;
    let updated = db.status(&milestone, json!({"lifecycle":"Closed","updated_by_principal_id":actor,
        "acceptance":{"previous_milestone_ref":milestone.reference(),"project_ref":project.reference(),
            "principal_ref":db.of(actor,"Principal")?.reference(),"closed_by_principal_id":actor,
            "closed_at":now(),"closed_work_item_refs":members.iter().map(Doc::reference).collect::<Vec<_>>()}}))?;
    Ok(view(&updated, &members))
}

pub(crate) fn abandon(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Abandon = decode(params)?;
    let (project, milestone) = access_milestone(db, actor, &request.id, MANAGE)?;
    open(&milestone, &request.expected_resource_version)?;
    require(
        reason_ok(&request.reason),
        "InvalidArgument",
        "abandonment requires a reason up to 512 bytes",
    )?;
    let members = group(db, &project, &milestone)?;
    db.cfg.check_admission_space()?;
    let updated = db.status(&milestone, json!({"lifecycle":"Abandoned","updated_by_principal_id":actor,
        "abandonment":{"previous_milestone_ref":milestone.reference(),"project_ref":project.reference(),
            "principal_ref":db.of(actor,"Principal")?.reference(),"abandoned_by_principal_id":actor,
            "abandoned_at":now(),"reason":request.reason}}))?;
    Ok(view(&updated, &members))
}
