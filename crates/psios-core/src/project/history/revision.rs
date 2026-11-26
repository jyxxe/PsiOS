use super::{
    super::{ITEM, READ, access_item, criterion_view, evidence},
    projection,
};
use crate::{Result, db::Db, decode, require};
use psios_api::ResourceRef;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    reference: ResourceRef,
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Revision = decode(params)?;
    let (project, current) = access_item(db, actor, &request.reference.resource_id, READ)?;
    let reference = serde_json::to_value(request.reference)?;
    let item = db.resolve(&reference)?;
    require(
        item.kind == ITEM && item.owner == current.owner && item.spec["project_id"] == project.id,
        "RecoveryRequired",
        "work item revision is outside its project authority",
    )?;

    let mut value = projection(&item);
    value["schema_version"] = json!(psios_api::VERSION);
    value["project_id"] = item.spec["project_id"].clone();
    value["title"] = item.spec["title"].clone();
    value["description"] = item.spec["description"].clone();
    value["blocked_by"] = item.spec["blocked_by"].clone();
    value["criterion"] = criterion_view(db, &item, actor)?;
    value["created_at"] = json!(item.created);
    value["updated_at"] = json!(item.stamp);
    value["created_by_principal_id"] = item.spec["created_by_principal_id"].clone();
    value["updated_by_principal_id"] = item.body["updated_by_principal_id"].clone();
    if let Some(details) = evidence::view(db, &item, actor)? {
        value["evidence"] = details;
    }
    Ok(json!({"reference":reference,"value":value}))
}
