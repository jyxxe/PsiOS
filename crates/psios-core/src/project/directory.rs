use super::{PROJECT, READ, project_view, role_allows};
use crate::{
    Result,
    db::{Db, row},
    decode, policy, require,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    after_id: Option<String>,
    limit: Option<u32>,
}

pub(crate) fn list(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    let request: List = decode(input)?;
    let limit = request.limit.unwrap_or(16);
    require(
        (1..=32).contains(&limit),
        "InvalidArgument",
        "project page limit must be 1 to 32",
    )?;
    let after = request.after_id.as_deref().unwrap_or_default();
    if request.after_id.is_some() {
        require(
            after.strip_prefix("project-").is_some_and(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }),
            "InvalidArgument",
            "invalid project page position",
        )?;
    }
    policy::active(db, actor)?;
    policy::can(db, actor, READ)?;
    let mut statement = db.c.prepare(
        "SELECT * FROM current WHERE kind=?1 AND id>?2
         AND (owner=?3 OR body->'members'->>?3 IS NOT NULL) ORDER BY id",
    )?;
    let mut rows = statement.query(params![PROJECT, after, actor])?;
    let mut items = Vec::new();
    let mut last = None;
    let mut next_after_id = None;
    while let Some(record) = rows.next()? {
        let project = row(record)?;
        if !role_allows(&project, actor, READ)? {
            continue;
        }
        match policy::can_target(db, actor, READ, &project.id) {
            Ok(()) => (),
            Err(error) if error.0.code == "PermissionDenied" => continue,
            Err(error) => return Err(error),
        }
        if items.len() == limit as usize {
            next_after_id = last;
            break;
        }
        items.push(project_view(&project, actor)?);
        last = Some(project.id);
    }
    Ok(json!({"items":items,"next_after_id":next_after_id}))
}
