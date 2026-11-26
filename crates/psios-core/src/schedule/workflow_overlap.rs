//! Follow the Workflow created by the last accepted occurrence of an opted-in schedule.
use crate::{
    Result, artifact, broker,
    db::{self, Doc, row},
    err, require,
    workflow::{self, START_TOOL},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::apps::volume_rebind::DIRECT_CLONE_FORMAT + 1;
const MODE: &str = "SkipWhileWorkflowActive";
const TASK_ACTIVE: &str = "PreviousTaskActive";
const WORKFLOW_ACTIVE: &str = "PreviousWorkflowActive";
const UNVERIFIED: &str = "PreviousWorkflowUnverified";
const RECEIPT_LIMIT: usize = 4096;

pub(super) struct Block {
    pub(super) reason: &'static str,
    pub(super) evidence: Value,
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "workflow overlap lost its exact launch custody",
    )
}

fn enabled(schedule: &Doc) -> bool {
    schedule.spec["repeat"]["overlap"] == MODE || schedule.spec["calendar"]["overlap"] == MODE
}

fn references(schedule: &Doc) -> Result<&[Value]> {
    require(
        schedule.kind == "Schedule"
            && enabled(schedule)
            && (schedule.spec["repeat"].is_object() ^ schedule.spec["calendar"].is_object())
            && schedule.spec["request"]["task_type"] == START_TOOL
            && schedule.spec["event"].is_null(),
        "RecoveryRequired",
        "workflow overlap is outside a timed workflow launch schedule",
    )?;
    schedule.body["trigger_refs"]
        .as_array()
        .filter(|references| references.len() <= 32)
        .map(Vec::as_slice)
        .ok_or_else(invalid)
}

fn trigger(conn: &Connection, schedule: &Doc, reference: &Value, ordinal: usize) -> Result<Doc> {
    let trigger = db::resolve(conn, reference)?;
    let schedule_id = &schedule.id;
    let timing = &trigger.spec["timing"];
    let run_at = trigger.body["run_at"].as_str().ok_or_else(invalid)?;
    require(
        trigger.kind == "ScheduleTrigger"
            && trigger.rev == 1
            && trigger.owner == schedule.owner
            && trigger.spec["schedule_id"] == *schedule_id
            && trigger.body["schedule_id"] == *schedule_id
            && trigger.spec["definition_revision"] == "r1"
            && trigger.body["ordinal"] == ordinal + 1
            && trigger.body["ordinal"] == timing["ordinal"]
            && trigger.body["deadline_at"] == timing["deadline_at"]
            && trigger.body["run_at"] == timing["run_at"]
            && trigger.id == timing["trigger_id"]
            && trigger.id == super::trigger_id(schedule_id, run_at),
        "RecoveryRequired",
        "workflow overlap trigger differs from its schedule occurrence",
    )?;
    Ok(trigger)
}

fn previous_trigger(conn: &Connection, schedule: &Doc) -> Result<Option<Doc>> {
    for (ordinal, reference) in references(schedule)?.iter().enumerate().rev() {
        let candidate = trigger(conn, schedule, reference, ordinal)?;
        if candidate.body["lifecycle"] == "Triggered" {
            require(
                candidate.body["task_receipt"]["task_id"] == schedule.body["last_task_id"],
                "RecoveryRequired",
                "workflow overlap lost its most recent accepted Task",
            )?;
            return Ok(Some(candidate));
        }
    }
    require(
        schedule.body["last_task_id"].is_null(),
        "RecoveryRequired",
        "workflow overlap lost its accepted occurrence",
    )?;
    Ok(None)
}

fn state_at(conn: &Connection, id: &str, before: Option<i64>) -> Result<Doc> {
    if let Some(sequence) = before {
        return conn
            .query_row(
                "SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY rev DESC LIMIT 1",
                params![id, sequence],
                row,
            )
            .optional()?
            .ok_or_else(invalid);
    }
    db::find(conn, id)?.ok_or_else(invalid)
}

fn prior_task(
    conn: &Connection,
    schedule: &Doc,
    trigger: &Doc,
    before: Option<i64>,
) -> Result<Doc> {
    let receipt = &trigger.body["task_receipt"];
    let task = state_at(
        conn,
        receipt["task_id"].as_str().ok_or_else(invalid)?,
        before,
    )?;
    let deadline = trigger.body["deadline_at"].as_str().ok_or_else(invalid)?;
    let request = broker::retime_scheduled(&schedule.spec["request"], &trigger.id, deadline)?;
    require(
        task.kind == "Task"
            && task.owner == schedule.owner
            && task.spec == request
            && task.spec["origin_ref"] == task.body["origin_ref"]
            && task.spec["origin_ref"]["origin_type"] == "Schedule"
            && task.spec["origin_ref"]["origin_id"] == schedule.id
            && task.spec["task_type"] == START_TOOL
            && receipt["origin_ref"] == task.spec["origin_ref"]
            && receipt["idempotency_key"] == trigger.id
            && receipt["task_revision"] == "r1"
            && receipt["request_digest"] == task.spec["request_digest"],
        "RecoveryRequired",
        "workflow overlap Task differs from the original schedule submission",
    )?;
    Ok(task)
}

fn deadline(doc: &Value) -> Result<chrono::DateTime<chrono::Utc>> {
    super::instant(doc.as_str().ok_or_else(invalid)?).map_err(|_| invalid())
}

fn workflow_id(conn: &Connection, task: &Doc) -> Result<String> {
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |row| {
            row.get(0)
        })?;
    Ok(workflow::brokered_run_id(&authority, &task.owner, &task.id))
}

fn completed_launch(
    conn: &Connection,
    trigger: &Doc,
    task: &Doc,
    before: Option<i64>,
) -> Result<(Doc, Doc)> {
    artifact::require_task_result(&task.body["result"])?;
    let artifacts = task.body["result"]["artifact_refs"]
        .as_array()
        .ok_or_else(invalid)?;
    let [reference] = artifacts.as_slice() else {
        return Err(invalid());
    };
    let receipt = db::resolve(conn, reference)?;
    let operation = db::resolve(conn, &task.body["operation_refs"][0])?;
    let effect = db::resolve(conn, &operation.body["effect_refs"][0])?;
    let plan = db::resolve(conn, &operation.body["plan_ref"])?;
    let input = db::resolve(conn, &task.spec["input_ref"])?;
    let definition = &broker::dependencies::tool_input(&input)?["definition_ref"];
    require(
        artifact::is_json(&receipt)
            && artifact::is_task_output(&receipt, task)
            && receipt.spec["operation_id"] == operation.id
            && receipt.spec["effect_id"] == effect.id
            && operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && effect.kind == "Effect"
            && effect.owner == task.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["action"] == START_TOOL
            && effect.body["resource_ref"] == *definition
            && effect.body["phase"] == "EffectConfirmed"
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["evidence_refs"] == json!([reference])
            && plan.kind == "OperationPlan"
            && plan.owner == task.owner
            && plan.body["delegation_ref"] == task.spec["delegation_ref"]
            && plan.body["purpose_ref"] == task.spec["input_ref"]
            && plan.body["allowed_effects"][0]["resource_ref"] == *definition
            && plan.body["allowed_effects"][0]["action"] == START_TOOL
            && input.kind == "Input"
            && input.owner == task.owner
            && deadline(&plan.body["deadline_at"])? <= deadline(&trigger.body["deadline_at"])?,
        "RecoveryRequired",
        "workflow overlap receipt is outside its original known launch",
    )?;
    let body = psios_api::parse(&artifact::checkpoint_bytes(conn, &receipt, RECEIPT_LIMIT)?)?;
    let opened = db::resolve(conn, &body["workflow_run_ref"])?;
    let original = db::resolve(conn, &body["origin_task_ref"])?;
    let issued_operation = db::resolve(conn, &original.body["operation_refs"][0])?;
    let child = state_at(conn, &opened.id, before)?;
    let elapsed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=?1)
          AND EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=?2)
          AND NOT EXISTS(SELECT 1 FROM authorization_deadlines w
                JOIN authorization_deadlines t ON t.resource=?2 AND t.boot_id=w.boot_id
                WHERE w.resource=?1 AND (t.time_ns<>w.time_ns OR w.deadline_boot_ms>t.deadline_boot_ms))",
        params![child.id,task.id],
        |row| row.get(0),
    )?;
    require(
        opened.kind == "WorkflowRun"
            && opened.rev == 1
            && opened.id == workflow_id(conn, task)?
            && opened.owner == task.owner
            && opened.spec["definition_ref"] == *definition
            && opened.spec["delegation_id"] == task.spec["delegation_ref"]["resource_id"]
            && opened.body["origin_task_ref"] == original.reference()
            && original.kind == "Task"
            && original.id == task.id
            && original.owner == task.owner
            && original.spec == task.spec
            && original.body["current_run_id"] == task.body["current_run_id"]
            && original.body["outcome_knowledge"] == "InFlight"
            && issued_operation.id == operation.id
            && issued_operation.body["plan_ref"] == plan.reference()
            && child.kind == opened.kind
            && child.owner == opened.owner
            && child.spec == opened.spec
            && child.body["origin_task_ref"] == original.reference()
            && body
                == json!({"workflow_run_ref":opened.reference(),"origin_task_ref":original.reference(),
                          "definition_ref":definition,"deadline_at":opened.spec["deadline_at"]})
            && deadline(&opened.spec["deadline_at"])? <= deadline(&plan.body["deadline_at"])?
            && elapsed,
        "RecoveryRequired",
        "workflow overlap child differs from its exact launch or deadline",
    )?;
    Ok((receipt, child))
}

fn terminal(doc: &Doc) -> bool {
    matches!(
        doc.body["lifecycle"].as_str(),
        Some("Succeeded" | "Failed" | "Cancelled")
    )
}

fn settled(doc: &Doc) -> bool {
    terminal(doc)
        && (doc.body["outcome_knowledge"] == "Known"
            || doc.body["lifecycle"] != "Succeeded"
                && doc.body["outcome_knowledge"] == "NotStarted")
}

fn child_incomplete(conn: &Connection, child: &Doc, before: Option<i64>) -> Result<bool> {
    let done = settled(child);
    if before.is_none() && child.body["outcome_knowledge"] != "Unknown" {
        let active: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM active_workflows WHERE run=?1)
                 OR EXISTS(SELECT 1 FROM active_workflow_tasks WHERE run=?1)",
            [&child.id],
            |row| row.get(0),
        )?;
        require(
            active != done,
            "RecoveryRequired",
            "workflow overlap child disagrees with the active Workflow index",
        )?;
    }
    Ok(!done)
}

fn base_evidence(trigger: &Doc, task: &Doc) -> Value {
    json!({"launch_trigger_ref":trigger.reference(),"launch_task_ref":task.reference()})
}

fn block_at(
    conn: &Connection,
    schedule: &Doc,
    trigger: &Doc,
    before: Option<i64>,
) -> Result<Option<Block>> {
    let task = prior_task(conn, schedule, trigger, before)?;
    let mut evidence = base_evidence(trigger, &task);
    if !settled(&task) {
        return Ok(Some(Block {
            reason: TASK_ACTIVE,
            evidence,
        }));
    }
    if task.body["lifecycle"] != "Succeeded" {
        if db::find(conn, &workflow_id(conn, &task)?)?.is_none() {
            return Ok(None);
        }
        return Ok(Some(Block {
            reason: UNVERIFIED,
            evidence,
        }));
    }
    let child = completed_launch(conn, trigger, &task, before)
        .and_then(|(receipt, child)| Ok((child_incomplete(conn, &child, before)?, receipt, child)));
    match child {
        Ok((false, _, _)) => Ok(None),
        Ok((true, receipt, child)) => {
            evidence["launch_artifact_ref"] = receipt.reference();
            evidence["workflow_run_ref"] = child.reference();
            Ok(Some(Block {
                reason: WORKFLOW_ACTIVE,
                evidence,
            }))
        }
        Err(error)
            if before.is_none()
                && matches!(
                    error.0.code.as_str(),
                    "RecoveryRequired"
                        | "ResultEvicted"
                        | "ResultUnavailable"
                        | "PreconditionFailed"
                        | "NotFound"
                        | "InvalidArgument"
                        | "QuotaExceeded"
                ) =>
        {
            Ok(Some(Block {
                reason: UNVERIFIED,
                evidence,
            }))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn blocked(conn: &Connection, schedule: &Doc) -> Result<Option<Block>> {
    let Some(previous) = previous_trigger(conn, schedule)? else {
        return Ok(None);
    };
    block_at(conn, schedule, &previous, None)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current t LEFT JOIN current s ON s.id=t.spec->>'schedule_id'
          WHERE t.kind='ScheduleTrigger' AND t.body->'overlap_evidence' IS NOT NULL
          AND (s.id IS NULL OR s.kind<>'Schedule' OR
              (s.spec->'repeat'->>'overlap' IS NOT ?1 AND s.spec->'calendar'->>'overlap' IS NOT ?1)
              OR NOT EXISTS(SELECT 1 FROM json_each(s.body->'trigger_refs')
                   WHERE value->>'resource_id'=t.id)))",
        [MODE],
        |row| row.get(0),
    )?;
    require(
        !orphan,
        "RecoveryRequired",
        "workflow overlap evidence lost its original schedule",
    )?;
    let mut schedules = conn.prepare(
        "SELECT * FROM current WHERE kind='Schedule'
         AND (spec->'repeat'->>'overlap'=?1 OR spec->'calendar'->>'overlap'=?1)",
    )?;
    for schedule in schedules.query_map([MODE], row)? {
        verify_schedule(conn, &schedule?)?;
    }
    Ok(())
}

fn verify_schedule(conn: &Connection, schedule: &Doc) -> Result<()> {
    let mut previous = None;
    for (ordinal, reference) in references(schedule)?.iter().enumerate() {
        let trigger = trigger(conn, schedule, reference, ordinal)?;
        let sequence: i64 = conn.query_row(
            "SELECT seq FROM versions WHERE id=?1 AND rev=1",
            [&trigger.id],
            |row| row.get(0),
        )?;
        let phase = trigger.body["lifecycle"].as_str().ok_or_else(invalid)?;
        let reason = trigger.body["reason"].as_str();
        let evidence = trigger.body.get("overlap_evidence");
        if phase == "Triggered" {
            prior_task(conn, schedule, &trigger, None)?;
            if let Some(prior) = &previous {
                require(
                    block_at(conn, schedule, prior, Some(sequence))?.is_none(),
                    "RecoveryRequired",
                    "workflow schedule triggered while the original workflow was active",
                )?;
            }
        } else {
            require(
                trigger.body["task_receipt"].is_null(),
                "RecoveryRequired",
                "workflow overlap skipped trigger has a Task",
            )?;
        }
        if matches!(reason, Some(TASK_ACTIVE | WORKFLOW_ACTIVE | UNVERIFIED)) {
            let prior = previous.as_ref().ok_or_else(invalid)?;
            require(
                phase == "Skipped",
                "RecoveryRequired",
                "workflow overlap has an invalid disposition",
            )?;
            if reason == Some(UNVERIFIED) {
                // Retained bytes may have been repaired since the protective skip.
                let task = prior_task(conn, schedule, prior, Some(sequence))?;
                require(
                    settled(&task) && evidence == Some(&base_evidence(prior, &task)),
                    "RecoveryRequired",
                    "unverified workflow skip lost its previous launch Task",
                )?;
            } else {
                let expected =
                    block_at(conn, schedule, prior, Some(sequence))?.ok_or_else(invalid)?;
                require(
                    reason == Some(expected.reason) && evidence == Some(&expected.evidence),
                    "RecoveryRequired",
                    "workflow overlap proof disagrees with the state at its original skip",
                )?;
            }
        } else {
            require(
                evidence.is_none(),
                "RecoveryRequired",
                "unexpected workflow overlap evidence",
            )?;
        }
        if phase == "Triggered" {
            previous = Some(trigger);
        }
    }
    let latest = previous
        .as_ref()
        .map(|trigger| &trigger.body["task_receipt"]["task_id"]);
    require(
        &schedule.body["last_task_id"] == latest.unwrap_or(&Value::Null),
        "RecoveryRequired",
        "workflow overlap latest launch pointer disagrees with trigger history",
    )?;
    if let Some(previous) = previous {
        require(
            block_at(conn, schedule, &previous, None)?
                .is_none_or(|block| block.reason != UNVERIFIED),
            "RecoveryRequired",
            "latest workflow launch cannot be verified",
        )?;
    }
    Ok(())
}
