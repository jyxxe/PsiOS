//! Exact, separately authorized input data for one direct local Agent session.
use super::*;
use crate::db::{resolve, row};
use psios_api::ResourceRef;
use rusqlite::Connection;
use std::collections::BTreeSet;

pub(crate) const ACTION: &str = "session.context.read";
pub(crate) const FORMAT: u32 = crate::apps::volume_rebind::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("session_inputs.sql");
const MAX_INPUTS: usize = 4;
const MAX_INPUT_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Serialize)]
enum Kind {
    Json,
}

#[derive(Deserialize, Serialize)]
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

fn source(conn: &Connection, session: &Doc, input: &Input) -> Result<Doc> {
    let artifact = resolve(conn, &json!(input.reference))?;
    require(
        artifact.kind == "Artifact"
            && artifact::is_raw(&artifact)
            && !artifact::files::managed(&artifact)
            && artifact.owner == session.owner
            && artifact.spec["source"]["kind"] == "LocalSubmission"
            && artifact.spec["source"]["principal_id"] == session.owner
            && artifact.spec["declared_media_type"] == "application/json",
        "PermissionDenied",
        "session input must be an owned local JSON publication",
    )?;
    Ok(artifact)
}

fn access(db: &Db<'_>, session: &Doc, inputs: &[Input]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let delegation_id = session.spec["delegation_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "session input lacks its original delegation",
        )
    })?;
    let delegation = policy::delegation(db, &session.owner, delegation_id, ACTION)?;
    policy::delegation(db, &session.owner, delegation_id, artifact::ingress::READ)?;
    require(
        delegation.reference() == session.body["delegation_ref"]
            && delegation.body["tool_targets"].is_array(),
        "PermissionDenied",
        "session input requires the original explicit disclosure delegation",
    )?;
    let model: ResourceRef = decode(&session.spec["model_ref"])?;
    policy::tool_scope(db, &delegation.reference(), &model.resource_id)?;
    policy::tool_scope(db, &delegation.reference(), &db.meta("authority")?)?;
    for input in inputs {
        policy::tool_scope(db, &delegation.reference(), &input.reference.resource_id)?;
    }
    Ok(())
}

pub(super) fn live_access(db: &Db<'_>, session: &Doc) -> Result<()> {
    let inputs = configured(session)?;
    access(db, session, &inputs)?;
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

pub(super) fn admit(db: &Db<'_>, session: &Doc) -> Result<()> {
    let inputs = configured(session)?;
    access(db, session, &inputs)?;
    for input in inputs {
        let artifact = source(db.c, session, &input)?;
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
        access(db, session, &inputs)?;
    }
    context["inputs"] = json!([]);
    for input in inputs {
        let artifact = source(conn, session, &input)?;
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
        "SELECT EXISTS(SELECT 1 FROM session_input_holds WHERE artifact=?1)",
        [artifact],
        |row| row.get(0),
    )?)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut sessions = conn.prepare(
        "SELECT * FROM current WHERE kind='Session' AND spec->>'input_artifacts' IS NOT NULL",
    )?;
    let mut expected_holds = 0i64;
    for session in sessions.query_map([], row)? {
        let session = session?;
        let inputs = configured(&session)?;
        if inputs.is_empty() {
            continue;
        }
        let original = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=1",
            [&session.id],
            row,
        )?;
        require(
            original.kind == "Session"
                && original.owner == session.owner
                && original.spec == session.spec
                && original.body["delegation_ref"] == session.body["delegation_ref"]
                && session.spec["provider_queue"] != true
                && !session.spec["fallback_model_refs"]
                    .as_array()
                    .is_some_and(|refs| !refs.is_empty())
                && !session.spec["origin_task_ref"].is_object()
                && tool_context::enabled(&session),
            "RecoveryRequired",
            "session input differs from its original direct-session configuration",
        )?;
        for input in inputs {
            let artifact = source(conn, &session, &input)?;
            if !matches!(state(&session), "Succeeded" | "Failed" | "Cancelled") {
                require(
                    hold_exists(conn, &session.id, &artifact.id)?,
                    "RecoveryRequired",
                    "session input hold is missing",
                )?;
                value(conn, &artifact, MAX_INPUT_BYTES)?;
                expected_holds += 1;
            }
        }
    }
    let actual_holds = conn.query_row("SELECT count(*) FROM session_input_holds", [], |row| {
        row.get::<_, i64>(0)
    })?;
    require(
        actual_holds == expected_holds,
        "RecoveryRequired",
        "session input contains an unbound hold",
    )
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
