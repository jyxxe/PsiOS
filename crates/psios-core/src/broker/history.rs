use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct History {
    id: String,
    after_version: Option<String>,
    through_version: Option<String>,
    limit: Option<u32>,
}
fn revision(db: &Db<'_>, task: &str, version: &str) -> Result<i64> {
    db.c.query_row(
        "SELECT rev FROM versions WHERE id=?1 AND 'r'||rev=?2",
        params![task, version],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| err("PreconditionFailed", "task history boundary unavailable"))
}
pub fn history(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "task.read")?;
    let p: History = decode(p)?;
    let t = db.of(&p.id, "Task")?;
    policy::owned(actor, &t)?;
    let after = p
        .after_version
        .as_deref()
        .map(|v| revision(db, &t.id, v))
        .transpose()?
        .unwrap_or(0);
    let through = p
        .through_version
        .as_deref()
        .map(|v| revision(db, &t.id, v))
        .transpose()?
        .unwrap_or(t.rev);
    let limit = p.limit.unwrap_or(32);
    require(
        (1..=128).contains(&limit) && after <= through,
        "InvalidArgument",
        "invalid history range or page size",
    )?;
    let mut items=db.c.prepare("SELECT * FROM versions WHERE id=?1 AND rev>?2 AND rev<=?3 ORDER BY rev LIMIT ?4")?
        .query_map(params![t.id,after,through,limit+1],|r|{
            let d=crate::db::row(r)?;
            Ok(json!({"reference":d.reference(),"at":d.stamp,"run_id":d.body["current_run_id"],"lifecycle":d.body["lifecycle"],
                "outcome_knowledge":d.body["outcome_knowledge"],"conditions":d.body["conditions"],"operation_refs":d.body["operation_refs"]}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    Ok(
        json!({"through_version":format!("r{through}"),"next_after_version":more.then(||items.last().unwrap()["reference"]["version"].clone()),"items":items}),
    )
}
pub fn archive(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "task.submit")?;
    let p: Conditional = decode(p)?;
    let t = db.of(&p.id, "Task")?;
    policy::owned(actor, &t)?;
    t.cas(&p.expected_resource_version)?;
    let aid = format!("archive-{}", t.id);
    if let Some(d) = db.find(&aid)? {
        return Ok(d.reference());
    }
    require(
        matches!(
            t.body["lifecycle"].as_str(),
            Some("Succeeded" | "Failed" | "Cancelled")
        ) && t.body["outcome_knowledge"] != "Unknown",
        "PreconditionFailed",
        "archive requires a settled terminal task",
    )?;
    require(
        db.c.execute("DELETE FROM queue WHERE task=?1 AND phase='Done'", [&t.id])? == 1,
        "PreconditionFailed",
        "task still has outstanding work",
    )?;
    let body = json!({"task_ref":t.reference(),"result":t.body["result"],"archived_at":now(),"operation_refs":t.body["operation_refs"]});
    Ok(db
        .create("TaskArchive", &aid, &t.owner, json!({"task_id":t.id}), body)?
        .reference())
}
