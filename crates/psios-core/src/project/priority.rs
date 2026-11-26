//! Priority is mutable Ready-state metadata with an exact project authority receipt.
use super::{
    ITEM, MANAGE, access_item, invalid_state, item_view,
    maintenance::{awaiting_assignment, manager_project, preceding, sequence},
};
use crate::{
    Result,
    db::{Db, Doc},
    decode, now, require,
};
use psios_api::{ResourceRef, WorkItemPriority};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::assigned::FORMAT + 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reprioritize {
    id: String,
    expected_resource_version: String,
    priority: WorkItemPriority,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    previous_work_item_ref: ResourceRef,
    project_ref: ResourceRef,
    principal_ref: ResourceRef,
    previous_priority: WorkItemPriority,
    priority: WorkItemPriority,
    reprioritized_by_principal_id: String,
    reprioritized_at: String,
}

pub(super) fn effective(item: &Doc) -> Result<WorkItemPriority> {
    item.body
        .get("priority")
        .or_else(|| item.spec.get("initial_priority"))
        .map_or(Ok(WorkItemPriority::Normal), |priority| {
            decode(priority).map_err(|_| invalid_state())
        })
}

pub(super) fn summary(item: &Doc) -> Value {
    let change = &item.body["reprioritization"];
    if change.is_null() {
        return Value::Null;
    }
    json!({
        "previous_priority":change["previous_priority"],
        "reprioritized_by_principal_id":change["reprioritized_by_principal_id"],
        "reprioritized_at":change["reprioritized_at"],
    })
}

fn body(item: &Doc, change: &Change) -> Result<Value> {
    let mut body = item.body.clone();
    body["priority"] = serde_json::to_value(change.priority)?;
    body["reprioritization"] = serde_json::to_value(change)?;
    body["updated_by_principal_id"] = json!(change.reprioritized_by_principal_id);
    Ok(body)
}

pub(crate) fn reprioritize(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Reprioritize = decode(params)?;
    let (project, item) = access_item(db, actor, &request.id, MANAGE)?;
    item.cas(&request.expected_resource_version)?;
    require(
        awaiting_assignment(&item),
        "PreconditionFailed",
        "priority changes require a ready work item without retained evidence",
    )?;
    let previous = effective(&item)?;
    if previous == request.priority {
        return item_view(db, &item, actor, false);
    }
    db.cfg.check_admission_space()?;
    let actor_ref = db.of(actor, "Principal")?.reference();
    let change = Change {
        previous_work_item_ref: decode(&item.reference())?,
        project_ref: decode(&project.reference())?,
        principal_ref: decode(&actor_ref)?,
        previous_priority: previous,
        priority: request.priority,
        reprioritized_by_principal_id: actor.into(),
        reprioritized_at: now(),
    };
    let updated = db.status(&item, body(&item, &change)?)?;
    item_view(db, &updated, actor, false)
}

pub(super) fn changed(previous: &Doc, next: &Doc) -> bool {
    previous.body.get("priority") != next.body.get("priority")
        || previous.body.get("reprioritization") != next.body.get("reprioritization")
}

fn canonical_time(stamp: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .ok()
        .is_some_and(|time| {
            time.to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                == stamp
        })
}

pub(super) fn verify_change(conn: &Connection, previous: &Doc, next: &Doc) -> Result<()> {
    let change: Change = decode(&next.body["reprioritization"]).map_err(|_| invalid_state())?;
    let position = sequence(conn, next)?;
    let actor = &change.reprioritized_by_principal_id;
    manager_project(
        conn,
        next,
        &serde_json::to_value(&change.project_ref)?,
        actor,
        position,
    )?;
    require(
        awaiting_assignment(previous)
            && next.spec == previous.spec
            && next.generation == previous.generation
            && preceding(conn, &next.id, ITEM, position)?.reference() == previous.reference()
            && previous.reference() == serde_json::to_value(&change.previous_work_item_ref)?
            && preceding(conn, actor, "Principal", position)?.reference()
                == serde_json::to_value(&change.principal_ref)?
            && change.previous_priority == effective(previous)?
            && change.priority != change.previous_priority
            && canonical_time(&change.reprioritized_at)
            && next.body == body(previous, &change)?,
        "RecoveryRequired",
        "work item priority change disagrees with its exact previous revision",
    )
}
