//! Catalog changes share the authority transaction that publishes their Task receipt.
use crate::{
    Result,
    db::{Db, Doc},
    err, require,
};
use rusqlite::{OptionalExtension, params};
use serde_json::json;

// Older writers turn a pending direct catalog effect into an irreconcilable
// external Unknown, even though its mutation never left the authority.
pub(crate) const FORMAT: u32 = crate::storage::publication::FORMAT + 1;

pub(crate) fn is_tool(tool: &str) -> bool {
    matches!(
        tool,
        "application.install"
            | "application.stage_release"
            | "application.select_release"
            | "application.uninstall"
    )
}

pub(crate) fn uncommitted(db: &Db<'_>, task: &Doc, operation: &Doc) -> Result<bool> {
    let tool = task.spec["task_type"].as_str().unwrap_or_default();
    if !is_tool(tool) {
        return Ok(false);
    }
    let effect = db.resolve(&operation.body["effect_refs"][0])?;
    if effect.body["phase"] != "PermitBound" {
        // Historical dispatches did not record this publication boundary.
        return Ok(false);
    }
    let run_id = task.body["current_run_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI catalog Task lacks its original Run",
        )
    })?;
    let run = db.of(run_id, "Run")?;
    let plan = db.resolve(&operation.body["plan_ref"])?;
    let allowed = &plan.body["allowed_effects"][0];
    let target = db.resolve(&allowed["resource_ref"])?;
    let permit = db.resolve(&effect.spec["permit_ref"])?;
    let queue: Option<(String, Option<String>)> =
        db.c.query_row(
            "SELECT phase,operation FROM queue WHERE task=?1",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    require(
        task.kind == "Task"
            && matches!(
                task.body["lifecycle"].as_str(),
                Some("Running" | "Cancelling")
            )
            && task.body["outcome_knowledge"] == "InFlight"
            && task.body["operation_refs"] == json!([operation.reference()])
            && operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Running"
            && operation.body["outcome_knowledge"] == "InFlight"
            && operation.body["effect_refs"] == json!([effect.reference()])
            && effect.kind == "Effect"
            && effect.rev == 1
            && effect.owner == task.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["action"] == tool
            && effect.body["resource_ref"] == target.reference()
            && effect.body["outcome_knowledge"] == "InFlight"
            && effect.body["evidence_refs"] == json!([])
            && plan.kind == "OperationPlan"
            && plan.owner == task.owner
            && allowed["action"] == tool
            && run.owner == task.owner
            && run.body["task_id"] == task.id
            && run.body["outcome_knowledge"] == "InFlight"
            && run.body["operation_refs"] == task.body["operation_refs"]
            && permit.kind == "ExecutionPermit"
            && permit.owner == task.owner
            && permit.body["operation_id"] == operation.id
            && permit.body["effect_id"] == effect.id
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["recovery_incarnation"] == run.body["recovery_incarnation"]
            && queue.is_some_and(|(phase, id)| {
                phase == "Dispatched" && id.as_deref() == Some(operation.id.as_str())
            }),
        "RecoveryRequired",
        "OCI catalog Task, Run and original unspent effect disagree",
    )?;
    if matches!(target.kind.as_str(), "Application" | "Volume") {
        held(db, &target.id, &operation.id)?;
    }
    if tool == "application.uninstall"
        && let Some(volume) = target.spec["volume_id"].as_str()
    {
        held(db, volume, &operation.id)?;
    }
    Ok(true)
}

fn held(db: &Db<'_>, resource: &str, operation: &str) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![resource, operation],
        |row| row.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "OCI catalog operation lost its original resource hold",
    )
}
