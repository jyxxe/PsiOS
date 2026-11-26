//! Reconstruct single-result model input from the Task revision bound at issue.
use super::*;
use crate::db::{find, resolve, row};
use rusqlite::Connection;

pub(crate) const FORMAT: u32 = session_inputs::BROKERED_FORMAT + 1;
const CUTOVER: &str = "default-model-context-cutover";
pub(super) const TASK_REF: &str = "default_context_task_ref";

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.create(
        "ModelContextCutover",
        CUTOVER,
        policy::SYSTEM,
        json!({}),
        json!({"database_format":FORMAT}),
    )?;
    Ok(())
}

pub(super) fn cutover(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=1 AND kind='ModelContextCutover'
         AND owner=?2 AND body->>'database_format'=?3",
        params![CUTOVER, policy::SYSTEM, FORMAT],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| {
        err(
            "RecoveryRequired",
            "default model context cutover is missing",
        )
    })
}

pub(super) struct Issue {
    session: Doc,
    pub(super) seq: i64,
    incarnation: String,
}

pub(super) fn issue(conn: &Connection, session: &Doc, lease: &str) -> Result<Option<Issue>> {
    Ok(conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 AND kind='Session'
         AND body->>'lifecycle'='ModelInFlight' AND body->>'current_lease'=?2
         ORDER BY rev LIMIT 1",
            params![session.id, lease],
            |r| {
                Ok(Issue {
                    session: row(r)?,
                    seq: r.get("seq")?,
                    incarnation: r.get("incarnation")?,
                })
            },
        )
        .optional()?)
}

pub(super) fn require_current_incarnation(db: &Db<'_>, issued: &str) -> Result<()> {
    require(
        issued == db.incarnation,
        "Fenced",
        "model lease belongs to another recovery incarnation",
    )
}

pub(super) fn latest_task(conn: &Connection, session: &Doc) -> Result<Option<Doc>> {
    session.body["last_task"]
        .as_str()
        .map(|id| {
            find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "session tool task is missing"))
        })
        .transpose()
}

fn task_at_issue(conn: &Connection, session: &Doc, seq: i64) -> Result<Option<Doc>> {
    session.body["last_task"]
        .as_str()
        .map(|id| {
            conn.query_row(
                "SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY seq DESC LIMIT 1",
                params![id, seq],
                row,
            )
            .optional()?
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "session tool task predates its first revision",
                )
            })
        })
        .transpose()
}

pub(super) fn pinned_task(
    conn: &Connection,
    session: &Doc,
    usage: &Doc,
    seq: i64,
) -> Result<Option<Doc>> {
    let selected = task_at_issue(conn, session, seq)?;
    require(
        usage.body.get(TASK_REF) == Some(&json!(selected.as_ref().map(Doc::reference))),
        "RecoveryRequired",
        "default model context disagrees with the Task revision at issuance",
    )?;
    Ok(selected)
}

pub(super) fn replay_legacy(
    db: &Db<'_>,
    session: &Doc,
    skill: &Doc,
    model: &Doc,
    lease: &str,
    turn: u32,
) -> Result<Value> {
    let unknown = || {
        err(
            "OutcomeUnknown",
            "original default model context cannot be verified",
        )
    };
    let issue = issue(db.c, session, lease)?.ok_or_else(unknown)?;
    require_current_incarnation(db, &issue.incarnation)?;
    let issued = &issue.session;
    require(
        issue.seq < cutover(db.c)?,
        "RecoveryRequired",
        "new default model lease has no context checkpoint",
    )?;
    let usage = reserved_usage(db, issued, lease).map_err(|error| {
        if matches!(
            error.0.code.as_str(),
            "InvalidArgument" | "PreconditionFailed" | "RecoveryRequired"
        ) {
            unknown()
        } else {
            error
        }
    })?;
    if issued.body.get("current_usage_ref").is_none() {
        // The old writer stored ModelUsage immediately after the issued
        // Session in the same request. An identity match alone cannot prove that order.
        let witnessed: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions s JOIN versions u ON u.seq=s.seq+1
             WHERE s.seq=?1 AND u.id=?2 AND u.rev=1
               AND s.correlation=u.correlation AND s.incarnation=u.incarnation)",
            params![issue.seq, usage.id],
            |r| r.get(0),
        )?;
        require(
            witnessed,
            "OutcomeUnknown",
            "original default model reservation does not witness its issuance",
        )?;
    }
    require(
        issued.owner == session.owner
            && issued.spec == session.spec
            && issued.body["turn"] == turn
            && issued.body["last_task"] == session.body["last_task"]
            && usage.kind == "ModelUsage"
            && usage.owner == session.owner
            && usage.rev == 1
            && usage.body["reserved_calls"] == 1
            && usage
                .body
                .get("budget_ref")
                .is_none_or(|r| *r == issued.body["budget_ref"])
            && usage.body["session_id"] == session.id
            && usage.body["call_id"] == lease
            && usage.body["model_ref"] == model.reference(),
        "OutcomeUnknown",
        "original default model context cannot be verified",
    )?;
    let selected = task_at_issue(db.c, issued, issue.seq).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            unknown()
        } else {
            error
        }
    })?;
    let current = latest_task(db.c, session)?;
    require(
        selected.as_ref().map(Doc::reference) == current.as_ref().map(Doc::reference),
        "OutcomeUnknown",
        "session tool Task changed since the original model context was issued",
    )?;
    let context = build(db.c, Some(db), issued, skill, model, selected.as_ref())?;
    require(
        usage.body["input_bytes"] == canonical(&context).len(),
        "OutcomeUnknown",
        "original default model context size cannot be verified",
    )?;
    Ok(context)
}

pub(super) fn build(
    conn: &Connection,
    db: Option<&Db<'_>>,
    session: &Doc,
    skill: &Doc,
    model: &Doc,
    task: Option<&Doc>,
) -> Result<Value> {
    let maximum: usize = decode(&model.spec["max_context_bytes"])?;
    let mut context = tool_context::base(session, skill);
    require(
        session.body["last_task"].as_str() == task.map(|task| task.id.as_str()),
        "RecoveryRequired",
        "default model context does not match the last Task",
    )?;
    if let Some(task) = task {
        require(
            task.kind == "Task"
                && task.owner == session.owner
                && task.body["lifecycle"] == "Succeeded"
                && task.body["outcome_knowledge"] == "Known",
            "RecoveryRequired",
            "default model context has no confirmed tool task",
        )?;
        let op = resolve(conn, &task.body["operation_refs"][0])?;
        let plan = resolve(conn, &op.body["plan_ref"])?;
        require(
            op.kind == "Operation"
                && op.owner == session.owner
                && op.spec["task_id"] == task.id
                && op.body["lifecycle"] == "Succeeded"
                && op.body["outcome_knowledge"] == "Known"
                && plan.kind == "OperationPlan"
                && plan.owner == session.owner
                && plan.body["allowed_effects"][0]["action"] == task.spec["task_type"]
                && plan.body["delegation_ref"] == task.spec["delegation_ref"],
            "RecoveryRequired",
            "default model tool task does not match its settled operation",
        )?;
        if let Some(db) = db {
            tool_context::live_access(db, &session.owner, task, &plan)?;
            crate::apps::log_tail::authorize_task(db, &session.owner, task)?;
        }
        let mut last = json!({"task_id":task.id,"tool":task.spec["task_type"],
            "lifecycle":task.body["lifecycle"],"result":task.body["result"]});
        if session.spec["context_policy"] == "LocalToolResults" {
            let reference = &task.body["result"]["artifact_refs"][0];
            if db.is_some() {
                tool_context::current_result(conn, task)?;
            }
            let artifact = resolve(conn, reference).map_err(artifact::dependency_error)?;
            require(
                artifact.owner == session.owner
                    && artifact::is_json(&artifact)
                    && artifact::is_task_output(&artifact, task),
                "RecoveryRequired",
                "default model tool output does not belong to its producing task",
            )?;
            let bytes = artifact::checkpoint_bytes(conn, &artifact, maximum)
                .map_err(artifact::dependency_error)?;
            last["local_tool_result"] = serde_json::from_slice(&bytes)
                .map_err(|_| err("ResultUnavailable", "pinned tool output JSON is unreadable"))?;
        }
        context["last_tool"] = last;
    }
    tool_context::bound(&context, maximum)?;
    Ok(context)
}
