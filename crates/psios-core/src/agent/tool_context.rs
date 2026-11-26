//! Bound verified tool observations to the session and the exact model lease.
use super::*;
use crate::db::{find, resolve, row};
use psios_api::ResourceRef;
use rusqlite::Connection;

pub(crate) const FORMAT: u32 = crate::apps::restore_image::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("tool_context.sql");
const MAX_WINDOW: u8 = 4;
const HISTORY: &str = "tool_context_refs";

pub(super) fn validate(window: Option<u8>) -> Result<()> {
    require(
        window.is_none_or(|limit| (1..=MAX_WINDOW).contains(&limit)),
        "InvalidArgument",
        "tool context window must be 1 to 4 observations",
    )
}

pub(super) fn enabled(session: &Doc) -> bool {
    !session.spec["tool_context_window"].is_null()
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    source_session_id: String,
    turn: u32,
    attempt: u32,
    task_ref: ResourceRef,
    artifact_ref: ResourceRef,
}

fn entries(session: &Doc) -> Result<Vec<Entry>> {
    let window: u8 = decode(&session.spec["tool_context_window"])?;
    let entries: Vec<Entry> = decode(&session.body[HISTORY])?;
    validate(Some(window))?;
    let turn: u32 = decode(&session.body["turn"])?;
    require(
        session.body["tool_context_window"] == window
            && entries.len() <= usize::from(window)
            && entries
                .iter()
                .all(|entry| (1..=turn).contains(&entry.turn) && (1..=3).contains(&entry.attempt))
            && entries.windows(2).all(|pair| pair[0].turn < pair[1].turn),
        "RecoveryRequired",
        "session context window is malformed",
    )?;
    Ok(entries)
}

pub(super) fn remember(db: &Db<'_>, session: &Doc, task: &Doc, state: &mut Value) -> Result<()> {
    if !enabled(session)
        || state["lifecycle"] == "Cancelled"
        || task.body["lifecycle"] != "Succeeded"
        || task.body["outcome_knowledge"] != "Known"
    {
        return Ok(());
    }
    let (turn, attempt) = db.c.query_row(
        "SELECT turn,attempt FROM session_tasks WHERE session=?1 AND task=?2",
        params![session.id, task.id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let artifact = db.resolve(&task.body["result"]["artifact_refs"][0])?;
    require(
        artifact::is_json(&artifact) && artifact::is_task_output(&artifact, task),
        "RecoveryRequired",
        "session tool completion does not have one verified local result",
    )?;
    let mut observations = entries(session)?;
    // Only the successful attempt for a turn is visible to the next model.
    observations.retain(|old| old.turn != turn);
    observations.push(Entry {
        source_session_id: session.id.clone(),
        turn,
        attempt,
        task_ref: decode(&task.reference())?,
        artifact_ref: decode(&artifact.reference())?,
    });
    let limit: u8 = decode(&session.spec["tool_context_window"])?;
    observations.drain(..observations.len().saturating_sub(usize::from(limit)));
    state[HISTORY] = json!(observations);
    Ok(())
}

pub(super) fn retained(session: &Doc, task: &Doc) -> Result<bool> {
    Ok(entries(session)?
        .iter()
        .any(|entry| entry.task_ref.resource_id == task.id))
}

fn origins(conn: &Connection, session: &Doc) -> Result<Vec<String>> {
    let mut origins = vec![session.id.clone()];
    let mut reference = session.spec["predecessor_ref"].clone();
    // Each fallback uses one of at most three pre-authorized alternatives.
    while !reference.is_null() {
        let prior = resolve(conn, &reference)?;
        require(
            origins.len() < 4
                && !origins.contains(&prior.id)
                && prior.kind == "Session"
                && prior.owner == session.owner
                && prior.spec["tool_context_window"] == session.spec["tool_context_window"]
                && prior.spec["context_policy"] == "ReferencesOnly"
                && session.spec["context_policy"] == "ReferencesOnly",
            "RecoveryRequired",
            "session observation crosses an unrelated model handoff",
        )?;
        origins.push(prior.id);
        reference = prior.spec["predecessor_ref"].clone();
    }
    Ok(origins)
}

fn source(
    conn: &Connection,
    session: &Doc,
    origins: &[String],
    entry: &Entry,
) -> Result<(Doc, Doc, Doc)> {
    let task = resolve(conn, &json!(entry.task_ref)).map_err(artifact::dependency_error)?;
    let artifact = resolve(conn, &json!(entry.artifact_ref)).map_err(artifact::dependency_error)?;
    let operation = resolve(conn, &task.body["operation_refs"][0])?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let linked: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_tasks WHERE task=?1 AND session=?2 AND turn=?3 AND attempt=?4)",
        params![task.id, entry.source_session_id, entry.turn, entry.attempt],
        |row| row.get(0),
    )?;
    require(
        linked
            && origins.contains(&entry.source_session_id)
            && task.kind == "Task"
            && task.owner == session.owner
            && task.body["lifecycle"] == "Succeeded"
            && task.body["outcome_knowledge"] == "Known"
            && task.body["result"]["availability"] == "Available"
            && task.body["result"]["artifact_refs"] == json!([entry.artifact_ref])
            && artifact::is_json(&artifact)
            && artifact::is_task_output(&artifact, &task)
            && operation.kind == "Operation"
            && operation.owner == session.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && artifact.spec["operation_id"] == operation.id
            && plan.kind == "OperationPlan"
            && plan.owner == session.owner
            && plan.body["allowed_effects"][0]["action"] == task.spec["task_type"]
            && plan.body["delegation_ref"] == task.spec["delegation_ref"],
        "RecoveryRequired",
        "pinned observation disagrees with its original session, task or result",
    )?;
    Ok((task, artifact, plan))
}

fn live_access(db: &Db<'_>, owner: &str, task: &Doc, plan: &Doc) -> Result<()> {
    let delegation = db.resolve(&task.spec["delegation_ref"])?;
    let tool = task.spec["task_type"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "session observation has no original tool action",
        )
    })?;
    policy::delegation(db, owner, &delegation.id, "task.read")?;
    policy::delegation(db, owner, &delegation.id, tool)?;
    let targets = plan.body["preconditions"]
        .as_array()
        .filter(|targets| !targets.is_empty())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "session observation has no plan targets",
            )
        })?;
    require(
        targets[0]["resource_ref"] == plan.body["allowed_effects"][0]["resource_ref"],
        "RecoveryRequired",
        "session observation lost its primary plan target",
    )?;
    for condition in targets {
        let target = condition["resource_ref"]["resource_id"]
            .as_str()
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "session observation has an invalid plan target",
                )
            })?;
        policy::tool_scope(db, &task.spec["delegation_ref"], target)?;
    }
    Ok(())
}

fn context_limit(model: &Doc) -> Result<usize> {
    decode(&model.spec["max_context_bytes"])
}

fn current_result(conn: &Connection, pinned: &Doc) -> Result<()> {
    let current = find(conn, &pinned.id)?.ok_or_else(|| {
        err(
            "ResultUnavailable",
            "current session tool result is unavailable",
        )
    })?;
    require(
        current.kind == "Task"
            && current.owner == pinned.owner
            && current.body["lifecycle"] == "Succeeded"
            && current.body["outcome_knowledge"] == "Known"
            && current.body["result"]["artifact_refs"] == pinned.body["result"]["artifact_refs"],
        "RecoveryRequired",
        "current session result differs from the pinned artifact",
    )?;
    artifact::require_task_result(&current.body["result"])?;
    require(
        current.body["result"]["availability"] == "Available",
        "RecoveryRequired",
        "current session result has invalid availability",
    )
}

pub(super) fn prepare_retry(
    db: &Db<'_>,
    session: &Doc,
    model: &Doc,
    failed: &Doc,
    turn: u32,
    state: &mut Value,
) -> Result<()> {
    if !enabled(session) {
        return Ok(());
    }
    let origins = origins(db.c, session)?;
    let mut kept = Vec::new();
    let mut remaining = context_limit(model)?;
    for entry in entries(session)? {
        require(
            entry.turn < turn && entry.task_ref.resource_id != failed.id,
            "RecoveryRequired",
            "session retry context contains its unsuccessful turn",
        )?;
        let (task, artifact, plan) = source(db.c, session, &origins, &entry)?;
        live_access(db, &session.owner, &task, &plan)?;
        if session.spec["context_policy"] == "LocalToolResults" {
            match artifact::retention::require_content(db.c, &artifact) {
                Err(e) if e.0.code == "ResultEvicted" => continue,
                other => other?,
            }
            current_result(db.c, &task)?;
            artifact::availability::verify_bytes(db, &artifact, &mut remaining)
                .map_err(artifact::dependency_error)?;
        }
        kept.push(entry);
    }
    state[HISTORY] = json!(kept);
    Ok(())
}

pub(super) fn bound(context: &Value, maximum: usize) -> Result<()> {
    require(
        canonical(context).len() <= maximum,
        "QuotaExceeded",
        "model context exceeds pinned provider limit",
    )
}

pub(super) fn base(session: &Doc, skill: &Doc) -> Value {
    json!({"session_id":session.id,"goal":session.spec["goal"],"skill_ref":session.spec["skill_ref"],
        "model_ref":session.spec["model_ref"],"instructions":skill.spec["instructions"],
        "permitted_tools":skill.spec["tools"],"last_tool":null,"context_policy":session.spec["context_policy"]})
}

fn projected(
    conn: &Connection,
    db: Option<&Db<'_>>,
    session: &Doc,
    skill: &Doc,
    model: &Doc,
) -> Result<Value> {
    let entries = entries(session)?;
    let origins = origins(conn, session)?;
    let maximum = context_limit(model)?;
    let mut context = base(session, skill);
    context["previous_tools"] = json!([]);
    session_inputs::project(conn, db, session, &mut context, maximum)?;
    require(
        session.body["last_task"].as_str()
            == entries
                .last()
                .map(|entry| entry.task_ref.resource_id.as_str()),
        "RecoveryRequired",
        "session context does not match the most recently completed tool",
    )?;
    bound(&context, maximum)?;
    for (index, entry) in entries.iter().enumerate() {
        let (task, artifact, plan) = source(conn, session, &origins, entry)?;
        if let Some(db) = db {
            live_access(db, &session.owner, &task, &plan)?;
        }
        let previous = index + 1 != entries.len();
        let observation = json!({"task_id":task.id,"task_ref":entry.task_ref,"turn":entry.turn,
            "attempt":entry.attempt,"tool":task.spec["task_type"],"lifecycle":task.body["lifecycle"],
            "result":task.body["result"]});
        if previous {
            context["previous_tools"]
                .as_array_mut()
                .unwrap()
                .push(observation);
        } else {
            context["last_tool"] = observation;
        }
        bound(&context, maximum)?;
        if session.spec["context_policy"] == "LocalToolResults" {
            if db.is_some() {
                current_result(conn, &task)?;
            }
            let remaining = maximum - canonical(&context).len();
            let bytes = artifact::checkpoint_bytes(conn, &artifact, remaining)
                .map_err(artifact::dependency_error)?;
            let data: Value = serde_json::from_slice(&bytes)
                .map_err(|_| err("ResultUnavailable", "pinned tool output JSON is unreadable"))?;
            let output = if previous {
                &mut context["previous_tools"][index]
            } else {
                &mut context["last_tool"]
            };
            output["local_tool_result"] = data;
            bound(&context, maximum)?;
        }
    }
    Ok(context)
}

pub(super) fn build(db: &Db<'_>, session: &Doc, skill: &Doc, model: &Doc) -> Result<Value> {
    projected(db.c, Some(db), session, skill, model)
}

struct Snapshot {
    session_ref: Value,
    usage_ref: Value,
    digest: String,
}

fn snapshot(conn: &Connection, lease: &str) -> Result<Option<Snapshot>> {
    Ok(conn.query_row(
        "SELECT session_ref,usage_ref,context_digest FROM model_context_snapshots WHERE lease=?1",
        [lease], |row| Ok(Snapshot { session_ref: row.get(0)?, usage_ref: row.get(1)?, digest: row.get(2)? }),
    ).optional()?)
}

pub(super) fn bind(
    db: &Db<'_>,
    session: &Doc,
    lease: &str,
    usage: &Doc,
    digest: &str,
) -> Result<()> {
    db.c.execute(
        "INSERT INTO model_context_snapshots(lease,session_ref,usage_ref,context_digest) VALUES(?1,?2,?3,?4)",
        params![lease, session.reference(), usage.reference(), digest],
    )?;
    Ok(())
}

fn checkpoint(
    conn: &Connection,
    session: &Doc,
    lease: &str,
    turn: u32,
    recorded: &Snapshot,
) -> Result<Doc> {
    let pinned = resolve(conn, &recorded.session_ref)?;
    let usage = resolve(conn, &recorded.usage_ref)?;
    let dispatched = conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=?2",
            params![pinned.id, pinned.rev.checked_add(1)],
            row,
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "model context has no adjacent dispatched session revision",
            )
        })?;
    let digest_valid = recorded
        .digest
        .strip_prefix("sha256:")
        .is_some_and(|digest| {
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    require(
        pinned.kind == "Session"
            && pinned.id == session.id
            && pinned.owner == session.owner
            && pinned.spec == session.spec
            && enabled(&pinned)
            && pinned.body["turn"]
                .as_u64()
                .and_then(|old| old.checked_add(1))
                == Some(u64::from(turn))
            && dispatched.kind == "Session"
            && dispatched.owner == pinned.owner
            && dispatched.spec == pinned.spec
            && state(&dispatched) == "ModelInFlight"
            && dispatched.body["turn"] == turn
            && dispatched.body["current_lease"] == lease
            && dispatched.body["current_usage_ref"] == recorded.usage_ref
            && digest_valid
            && usage.kind == "ModelUsage"
            && usage.owner == session.owner
            && usage.rev == 1
            && usage.body["session_id"] == session.id
            && usage.body["call_id"] == lease
            && usage.body["model_ref"] == session.spec["model_ref"]
            && usage.body["budget_ref"] == pinned.body["budget_ref"]
            && usage.body["context_digest"] == recorded.digest,
        "RecoveryRequired",
        "model lease context disagrees with its original session and usage",
    )?;
    Ok(pinned)
}

pub(super) fn replay(
    db: &Db<'_>,
    session: &Doc,
    skill: &Doc,
    model: &Doc,
    lease: &str,
    turn: u32,
) -> Result<(Value, String)> {
    let recorded = snapshot(db.c, lease)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "model lease has no durable context checkpoint",
        )
    })?;
    let pinned = checkpoint(db.c, session, lease, turn, &recorded)?;
    let context = build(db, &pinned, skill, model)?;
    require(
        sha256(&canonical(&context)) == recorded.digest,
        "RecoveryRequired",
        "replayed model context differs from its original input",
    )?;
    Ok((context, recorded.digest))
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut leases = conn.prepare(
        "SELECT t.lease,t.session,t.turn FROM session_turns t JOIN current s ON s.id=t.session
         WHERE s.spec->>'tool_context_window' IS NOT NULL",
    )?;
    let mut count = 0i64;
    for row in leases.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u32>(2)?,
        ))
    })? {
        let (lease, id, turn) = row?;
        let session =
            find(conn, &id)?.ok_or_else(|| err("RecoveryRequired", "model session is missing"))?;
        let recorded = snapshot(conn, &lease)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "opt-in model turn is missing its context checkpoint",
            )
        })?;
        let pinned = checkpoint(conn, &session, &lease, turn, &recorded)?;
        let origins = origins(conn, &pinned)?;
        for observation in entries(&pinned)? {
            source(conn, &pinned, &origins, &observation)?;
        }
        if state(&session) == "ModelInFlight" && session.body["current_lease"] == lease {
            let skill = resolve(conn, &session.spec["skill_ref"])?;
            let model = resolve(conn, &session.spec["model_ref"])?;
            let context = projected(conn, None, &pinned, &skill, &model)?;
            require(
                sha256(&canonical(&context)) == recorded.digest,
                "RecoveryRequired",
                "model lease context digest disagrees with the retained observations",
            )?;
        }
        count += 1;
    }
    let mut sessions = conn.prepare(
        "SELECT * FROM current WHERE kind='Session' AND spec->>'tool_context_window' IS NOT NULL",
    )?;
    for session in sessions.query_map([], row)? {
        let session = session?;
        let origins = origins(conn, &session)?;
        for observation in entries(&session)? {
            let (_, result, _) = source(conn, &session, &origins, &observation)?;
            if session.spec["context_policy"] == "LocalToolResults"
                && !matches!(state(&session), "Failed" | "Succeeded" | "Cancelled")
            {
                artifact::retention::require_content(conn, &result)?;
            }
        }
    }
    require(
        count
            == conn.query_row("SELECT count(*) FROM model_context_snapshots", [], |row| {
                row.get::<_, i64>(0)
            })?,
        "RecoveryRequired",
        "model context checkpoints include unbound leases",
    )
}
