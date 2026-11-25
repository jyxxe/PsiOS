//! Output dependencies are pinned in the immutable Task input before admission.
use super::{approval, close, condition, execution, field};
use crate::{
    Result, artifact,
    db::{Db, Doc},
    decode, err, label, policy, require,
};
use psios_api::TaskOutputRequirement;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) const FORMAT: u32 = 41;
pub(crate) const SCHEMA: &str = include_str!("dependencies.sql");
pub(crate) const INPUT_KIND: &str = "TaskInputManifest";
const VERIFY_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn authorize(
    db: &Db<'_>,
    actor: &str,
    requirements: &[TaskOutputRequirement],
) -> Result<()> {
    if requirements.is_empty() {
        return Ok(());
    }
    require(
        requirements.len() <= 4,
        "InvalidArgument",
        "too many required task outputs",
    )?;
    policy::can(db, actor, "task.read")?;
    let mut sources = BTreeSet::new();
    for item in requirements {
        label(&item.task_id)?;
        let pinned = item.digest.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
        require(
            sources.insert(&item.task_id) && pinned,
            "InvalidArgument",
            "a required output needs a unique producing Task and SHA-256 digest",
        )?;
        policy::owned(actor, &db.of(&item.task_id, "Task")?)?;
    }
    Ok(())
}

pub(crate) fn tool_input(input: &Doc) -> Result<&Value> {
    if input.kind == INPUT_KIND {
        input
            .body
            .get("tool_input")
            .ok_or_else(|| err("RecoveryRequired", "task input manifest has no tool input"))
    } else {
        require(
            input.kind == "Input",
            "RecoveryRequired",
            "unexpected task input kind",
        )?;
        Ok(&input.body)
    }
}

pub(super) fn requirements(input: &Doc) -> Result<Vec<TaskOutputRequirement>> {
    if input.kind == INPUT_KIND {
        decode(&input.body["requires"])
    } else {
        Ok(Vec::new())
    }
}

pub(super) fn ready(
    db: &Db<'_>,
    actor: &str,
    requirements: &[TaskOutputRequirement],
) -> Result<Option<Vec<Value>>> {
    if requirements.is_empty() {
        return Ok(Some(Vec::new()));
    }
    policy::can(db, actor, "task.read")?;
    let mut outputs = Vec::with_capacity(requirements.len());
    for item in requirements {
        let producer = db.of(&item.task_id, "Task")?;
        policy::owned(actor, &producer)?;
        if producer.body["outcome_knowledge"] == "Unknown" {
            continue;
        }
        match producer.body["lifecycle"].as_str() {
            Some("Failed" | "Cancelled") => {
                return Err(err(
                    "DependencyUnavailable",
                    "a producing Task did not succeed",
                ));
            }
            Some("Succeeded") => {}
            _ => continue,
        }
        let result = &producer.body["result"];
        if result["availability"] == "Unavailable" {
            continue;
        }
        artifact::require_task_result(result)?;
        let reference = result["artifact_refs"]
            .as_array()
            .and_then(|refs| {
                refs.iter()
                    .find(|reference| reference["digest"] == item.digest)
            })
            .ok_or_else(|| {
                err(
                    "PreconditionFailed",
                    "producing Task has no output with the required digest",
                )
            })?;
        let output = db.resolve(reference).map_err(artifact::dependency_error)?;
        require(
            artifact::is_task_output(&output, &producer),
            "RecoveryRequired",
            "required artifact belongs to a different Task or Run",
        )?;
        outputs.push(output);
    }
    if outputs.len() != requirements.len() {
        return Ok(None);
    }
    let mut remaining = VERIFY_BYTES;
    for output in &outputs {
        artifact::retention::require_content(db.c, output)?;
        artifact::availability::verify_bytes(db, output, &mut remaining)
            .map_err(artifact::dependency_error)?;
    }
    Ok(Some(outputs.iter().map(Doc::reference).collect()))
}

pub(super) fn pin(db: &Db<'_>, task: &str, requirements: &[TaskOutputRequirement]) -> Result<()> {
    for item in requirements {
        db.c.execute(
            "INSERT INTO task_dependencies VALUES(?1,?2)",
            params![task, item.task_id],
        )?;
    }
    Ok(())
}

pub(super) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute("DELETE FROM task_dependencies WHERE task=?1", [task])?;
    Ok(())
}

pub(crate) fn retains_result(db: &Db<'_>, producer: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM task_dependencies WHERE producer=?1)",
        [producer],
        |row| row.get(0),
    )?)
}

pub(super) fn waiting() -> Value {
    condition(
        "DependencyUnavailable",
        "waiting for the required task outputs",
    )
}

pub(super) fn advance(db: &Db<'_>) -> Result<()> {
    let next = db.c.query_row(
        "SELECT q.task
         FROM queue q JOIN task_dependencies d ON d.task=q.task
         LEFT JOIN current p ON p.id=d.producer
         WHERE q.phase='WaitingResource'
         GROUP BY q.task,q.stamp
         HAVING MAX(CASE WHEN p.id IS NULL OR
               (p.body->>'lifecycle' IN ('Failed','Cancelled') AND p.body->>'outcome_knowledge'!='Unknown') OR
               p.body->'result'->>'availability'='Evicted' THEN 1 ELSE 0 END)=1
            OR SUM(CASE WHEN p.id IS NULL OR
               (p.body->>'lifecycle'='Succeeded' AND p.body->>'outcome_knowledge'='Known'
                   AND p.body->'result'->>'availability'!='Unavailable') THEN 0 ELSE 1 END)=0
         ORDER BY q.stamp,q.task LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    ).optional()?;
    let Some(next) = next else { return Ok(()) };
    let task = db.of(&next, "Task")?;
    match execution::preflight(db, &task) {
        Ok((op, plan, Some(_))) => {
            let mut body = task.body.clone();
            let target_phase = if policy::approval(field(&task.spec, "task_type")) {
                let request = approval::request(db, &task.owner, &task.id, &plan)?;
                let mut state = op.body.clone();
                state["lifecycle"] = json!("WaitingApproval");
                state["updated_at"] = json!(crate::now());
                body["operation_refs"] = json!([db.status(&op, state)?.reference()]);
                body["conditions"] = json!([request]);
                "WaitingApproval"
            } else {
                body["conditions"] = json!([]);
                "Queued"
            };
            body["lifecycle"] = json!(target_phase);
            db.status(&task, body)?;
            db.c.execute(
                "UPDATE queue SET phase=?1 WHERE task=?2",
                params![target_phase, task.id],
            )?;
        }
        Ok((_, _, None)) => {}
        Err(error) => {
            close(db, &task, "Failed", "NotStarted", Some(&error), None)?;
        }
    }
    Ok(())
}
