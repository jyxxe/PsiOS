//! Exact, separately authorized input data for one local Agent session.
use super::*;
use crate::db::resolve;
use psios_api::ResourceRef;
use rusqlite::Connection;
use std::collections::BTreeSet;

mod verify;
pub(crate) use verify::verify;

pub(crate) const ACTION: &str = "session.context.read";
pub(crate) const FORMAT: u32 = crate::apps::volume_rebind::FORMAT + 1;
// Older queue writers could disclose newly queued inputs without negotiation.
pub(crate) const QUEUED_FORMAT: u32 = crate::node::block_device::FORMAT + 1;
pub(crate) const BROKERED_FORMAT: u32 = crate::storage::transfer::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("session_inputs.sql");
pub(crate) const BROKERED_SCHEMA: &str = include_str!("brokered_session_input_holds.sql");
const MAX_INPUTS: usize = 4;
const MAX_INPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Kind {
    Json,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    name: String,
    reference: ResourceRef,
    kind: Kind,
}

pub(super) fn validate(inputs: &[Input]) -> Result<()> {
    require(
        inputs.len() <= MAX_INPUTS,
        "InvalidArgument",
        "session accepts at most four inputs",
    )?;
    let mut names = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    for input in inputs {
        require(
            input.name.len() <= 64
                && label(&input.name).is_ok()
                && names.insert(&input.name)
                && artifacts.insert(&input.reference.resource_id),
            "InvalidArgument",
            "session inputs require unique names and artifact references",
        )?;
    }
    Ok(())
}

fn configured(session: &Doc) -> Result<Vec<Input>> {
    let Some(inputs) = session.spec.get("input_artifacts") else {
        return Ok(Vec::new());
    };
    let inputs: Vec<Input> = decode(inputs)?;
    validate(&inputs)?;
    Ok(inputs)
}

pub(super) fn require_provider(session: &Doc, accepts_inputs: bool) -> Result<()> {
    require(
        accepts_inputs || configured(session)?.is_empty(),
        "UnsupportedCapability",
        "model provider has not opted into session artifact inputs",
    )
}

fn source(conn: &Connection, owner: &str, input: &Input) -> Result<Doc> {
    let artifact = resolve(conn, &json!(input.reference))?;
    require(
        artifact.kind == "Artifact"
            && artifact::is_raw(&artifact)
            && !artifact::files::managed(&artifact)
            && artifact.owner == owner
            && artifact.spec["source"]["kind"] == "LocalSubmission"
            && artifact.spec["source"]["principal_id"] == owner
            && artifact.spec["declared_media_type"] == "application/json",
        "PermissionDenied",
        "session input must be an owned local JSON publication",
    )?;
    Ok(artifact)
}

fn access(
    db: &Db<'_>,
    owner: &str,
    delegation_ref: &Value,
    model_ref: &Value,
    inputs: &[Input],
) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let delegation_id = delegation_ref["resource_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "session input lacks its original delegation reference",
        )
    })?;
    let delegation = policy::delegation(db, owner, delegation_id, ACTION)?;
    policy::delegation(db, owner, delegation_id, artifact::ingress::READ)?;
    let grant_id = delegation.body["grant_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "session input delegation lacks a grant"))?;
    let grant = db.of(grant_id, "Grant")?;
    require(
        delegation.reference() == *delegation_ref
            && (grant.spec["tool_targets"].is_array()
                || delegation.body["tool_targets"].is_array()),
        "PermissionDenied",
        "session input requires the original explicit disclosure delegation",
    )?;
    let model: ResourceRef = decode(model_ref)?;
    policy::tool_scope(db, &delegation.reference(), &model.resource_id)?;
    policy::tool_scope(db, &delegation.reference(), &db.meta("authority")?)?;
    for input in inputs {
        policy::tool_scope(db, &delegation.reference(), &input.reference.resource_id)?;
    }
    Ok(())
}

fn session_access(db: &Db<'_>, session: &Doc, inputs: &[Input]) -> Result<()> {
    if !inputs.is_empty() {
        require(
            session.spec["delegation_id"] == session.body["delegation_ref"]["resource_id"],
            "RecoveryRequired",
            "session input delegation differs from its immutable configuration",
        )?;
    }
    access(
        db,
        &session.owner,
        &session.body["delegation_ref"],
        &session.spec["model_ref"],
        inputs,
    )
}

pub(super) fn live_access(db: &Db<'_>, session: &Doc) -> Result<()> {
    let inputs = configured(session)?;
    session_access(db, session, &inputs)?;
    for input in inputs {
        require(
            hold_exists(db.c, &session.id, &input.reference.resource_id)?,
            "RecoveryRequired",
            "session input hold is missing",
        )?;
    }
    Ok(())
}

fn value(conn: &Connection, artifact: &Doc, limit: usize) -> Result<Value> {
    let bytes = artifact::checkpoint_bytes(conn, artifact, limit.min(MAX_INPUT_BYTES))?;
    psios_api::parse(&bytes).map_err(Into::into)
}

pub(crate) fn add_task_audience(
    db: &Db<'_>,
    actor: &str,
    delegation_ref: &Value,
    blueprint: &SessionBlueprint,
    audience: &mut Vec<Value>,
) -> Result<()> {
    access(
        db,
        actor,
        delegation_ref,
        &blueprint.model_ref,
        &blueprint.input_artifacts,
    )?;
    for input in &blueprint.input_artifacts {
        audience.push(source(db.c, actor, input)?.reference());
    }
    Ok(())
}

pub(crate) fn pin_task(db: &Db<'_>, task: &Doc, blueprint: &SessionBlueprint) -> Result<()> {
    for input in &blueprint.input_artifacts {
        let artifact = source(db.c, &task.owner, input)?;
        value(db.c, &artifact, MAX_INPUT_BYTES)?;
        db.c.execute(
            "INSERT INTO brokered_session_input_holds(task,artifact) VALUES(?1,?2)",
            params![task.id, artifact.id],
        )?;
    }
    Ok(())
}

pub(super) fn require_task_holds(conn: &Connection, task: &Doc, inputs: &[Input]) -> Result<()> {
    let mut stmt =
        conn.prepare("SELECT artifact FROM brokered_session_input_holds WHERE task=?1")?;
    let retained: BTreeSet<String> = stmt
        .query_map([&task.id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let expected: BTreeSet<String> = inputs
        .iter()
        .map(|input| input.reference.resource_id.clone())
        .collect();
    require(
        retained == expected,
        "RecoveryRequired",
        "session launch task lacks its exact artifact custody",
    )
}

pub(crate) fn release_task(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM brokered_session_input_holds WHERE task=?1",
        [task],
    )?;
    Ok(())
}

pub(super) fn admit(db: &Db<'_>, session: &Doc) -> Result<()> {
    let inputs = configured(session)?;
    session_access(db, session, &inputs)?;
    for input in inputs {
        let artifact = source(db.c, &session.owner, &input)?;
        value(db.c, &artifact, MAX_INPUT_BYTES)?;
        db.c.execute(
            "INSERT INTO session_input_holds(session,artifact) VALUES(?1,?2)",
            params![session.id, artifact.id],
        )?;
    }
    Ok(())
}

pub(super) fn project(
    conn: &Connection,
    db: Option<&Db<'_>>,
    session: &Doc,
    context: &mut Value,
    limit: usize,
) -> Result<()> {
    let inputs = configured(session)?;
    if inputs.is_empty() {
        return Ok(());
    }
    if let Some(db) = db {
        session_access(db, session, &inputs)?;
    }
    context["inputs"] = json!([]);
    for input in inputs {
        let artifact = source(conn, &session.owner, &input)?;
        require(
            hold_exists(conn, &session.id, &artifact.id)?,
            "RecoveryRequired",
            "session input hold is missing",
        )?;
        let current = canonical(context).len();
        let material = value(conn, &artifact, limit.saturating_sub(current))?;
        context["inputs"].as_array_mut().unwrap().push(json!({
            "name":input.name,"reference":input.reference,"kind":input.kind,"value":material,
        }));
        tool_context::bound(context, limit)?;
    }
    Ok(())
}

fn hold_exists(conn: &Connection, session: &str, artifact: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_input_holds WHERE session=?1 AND artifact=?2)",
        params![session, artifact],
        |row| row.get(0),
    )?)
}

pub(crate) fn retains(db: &Db<'_>, artifact: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_input_holds WHERE artifact=?1)
             OR EXISTS(SELECT 1 FROM brokered_session_input_holds WHERE artifact=?1)",
        [artifact],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_inputs_require_distinct_names_and_resources() {
        let input = |name: &str, id: &str| Input {
            name: name.into(),
            reference: ResourceRef {
                resource_id: id.into(),
                version: "r1".into(),
                digest: "sha256:test".into(),
            },
            kind: Kind::Json,
        };
        assert!(validate(&[input("one", "artifact-1"), input("two", "artifact-2")]).is_ok());
        for invalid in [
            vec![input("one", "artifact-1"), input("one", "artifact-2")],
            vec![input("one", "artifact-1"), input("two", "artifact-1")],
            vec![input(&"x".repeat(65), "artifact-1")],
            (0..5)
                .map(|index| input(&format!("input-{index}"), &format!("artifact-{index}")))
                .collect(),
        ] {
            assert_eq!(validate(&invalid).unwrap_err().0.code, "InvalidArgument");
        }
    }

    #[test]
    fn only_terminal_session_revisions_release_their_holds() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE versions(kind TEXT, id TEXT, body TEXT)")
            .unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO session_input_holds VALUES ('one','artifact'),('two','artifact');",
        )
        .unwrap();
        let status = |kind: &str, session: &str, lifecycle: &str| {
            conn.execute(
                "INSERT INTO versions VALUES (?1,?2,?3)",
                params![kind, session, json!({"lifecycle":lifecycle})],
            )
            .unwrap();
        };
        status("Session", "one", "Paused");
        status("Task", "one", "Succeeded");
        assert!(hold_exists(&conn, "one", "artifact").unwrap());
        status("Session", "one", "Cancelled");
        assert!(!hold_exists(&conn, "one", "artifact").unwrap());
        assert!(hold_exists(&conn, "two", "artifact").unwrap());
    }
}
