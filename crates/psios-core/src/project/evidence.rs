//! Bind WorkItem acceptance to the original task receipt without granting result access.
use crate::{
    Result,
    apps::log_tail,
    artifact,
    broker::result_access,
    db::{Db, Doc, find, resolve, row},
    err, now, policy, require,
};
use psios_api::MAX_FRAME;
use rusqlite::Connection;
use serde_json::{Value, json};

pub(crate) const SCHEMA: &str = include_str!("evidence.sql");
pub(crate) const KIND: &str = "WorkItemEvidence";

struct Production {
    run: Doc,
    operation: Doc,
}

fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "work item evidence resource is missing"))
}

fn proof(conn: &Connection, item: &Doc, task: &Doc, artifact: &Doc) -> Result<Production> {
    let criterion = &item.spec["criterion"];
    require(
        task.kind == "Task"
            && task.owner == item.spec["assignee_principal_id"]
            && task.body["lifecycle"] == "Succeeded"
            && task.body["outcome_knowledge"] == "Known"
            && task.body["result"]["availability"] == "Available"
            && artifact::is_task_output(artifact, task)
            && task.body["result"]["artifact_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&artifact.reference())),
        "PreconditionFailed",
        "evidence requires an exact successful output of the assigned principal",
    )?;
    require(
        criterion["kind"] == "TaskResult" && task.spec["task_type"] == criterion["tool"],
        "PreconditionFailed",
        "task tool does not satisfy the work item criterion",
    )?;
    let run = needed(
        conn,
        task.body["current_run_id"].as_str().unwrap_or_default(),
    )?;
    let operation = resolve(conn, &task.body["operation_refs"][0])?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let allowed = &plan.body["allowed_effects"][0];
    let target = resolve(conn, &allowed["resource_ref"])?;
    require(
        target.reference() == criterion["target_ref"],
        "PreconditionFailed",
        "task target does not satisfy the work item criterion",
    )?;
    let effect_ref = operation.body["effect_refs"]
        .as_array()
        .and_then(|refs| refs.last())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "work item evidence has no producing effect",
            )
        })?;
    let effect = resolve(conn, effect_ref)?;
    let alternate_effect = if let Some(reference) = artifact.spec.get("effect_ref") {
        let prior = resolve(conn, reference)?;
        prior.kind == "Effect"
            && prior.id == effect.id
            && prior.body["operation_id"] == operation.id
    } else {
        false
    };
    require(
        run.kind == "Run"
            && run.owner == task.owner
            && run.body["task_id"] == task.id
            && run.body["outcome_knowledge"] == "Known"
            && matches!(run.body["lifecycle"].as_str(), Some("Succeeded" | "Lost"))
            && run.body["operation_refs"][0] == operation.reference()
            && run.body["output_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&artifact.reference()))
            && operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && plan.kind == "OperationPlan"
            && plan.owner == task.owner
            && allowed["action"] == criterion["tool"]
            && effect.kind == "Effect"
            && effect.owner == task.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["resource_ref"] == criterion["target_ref"]
            && effect.body["action"] == criterion["tool"]
            && effect.body["outcome_knowledge"] == "Known"
            && matches!(
                effect.body["phase"].as_str(),
                Some("EffectConfirmed" | "EffectReconciled")
            )
            && effect.body["evidence_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&artifact.reference()))
            && artifact.spec["operation_id"] == operation.id
            && (artifact.spec["effect_id"] == effect.id || alternate_effect),
        "RecoveryRequired",
        "artifact does not trace to the task's settled run and effect",
    )?;
    Ok(Production { run, operation })
}

fn live_output(conn: &Connection, task: &Doc, artifact: &Doc) -> Result<()> {
    require(
        needed(conn, &task.id)?.reference() == task.reference(),
        "PreconditionFailed",
        "task result revision changed",
    )?;
    artifact::retention::require_content(conn, artifact)
}

fn verify_payload(db: &Db<'_>, artifact: &Doc) -> Result<()> {
    if artifact::is_raw(artifact) {
        return artifact::verify_raw_bytes(db.c, &db.cfg.state_dir, artifact, MAX_FRAME);
    }
    artifact::checkpoint_bytes(db.c, artifact, MAX_FRAME).map(|_| ())
}

fn result_access(
    db: &Db<'_>,
    actor: &str,
    task: &Doc,
    artifact_ref: &Value,
) -> Result<(Doc, bool)> {
    let (artifact, shared) = result_access::resolve_artifact(db, actor, artifact_ref)?;
    log_tail::authorize_task(db, actor, task)?;
    Ok((artifact, shared))
}

pub(super) fn attach(
    db: &Db<'_>,
    item: &Doc,
    actor: &str,
    task_ref: &Value,
    artifact_ref: &Value,
) -> Result<Doc> {
    require(
        item.kind == "WorkItem" && item.spec["assignee_principal_id"] == actor,
        "PermissionDenied",
        "only the WorkItem assignee may submit task evidence",
    )?;
    let (artifact, shared) = result_access::resolve_artifact(db, actor, artifact_ref)?;
    let task = db.resolve(task_ref)?;
    let provenance = proof(db.c, item, &task, &artifact)?;
    log_tail::authorize_task(db, actor, &task)?;
    live_output(db.c, &task, &artifact)?;
    verify_payload(db, &artifact)?;
    if shared {
        result_access::audit_read(db, actor, "WorkItem.SubmitEvidence", &artifact.id)?;
    }
    db.record(
        KIND,
        actor,
        json!({"work_item_id":item.id, "project_id":item.spec["project_id"],
            "work_item_ref":item.reference(), "criterion":item.spec["criterion"],
            "task_ref":task.reference(), "artifact_ref":artifact.reference(),
            "operation_ref":provenance.operation.reference(), "run_ref":provenance.run.reference(),
            "submitted_by":actor, "submitted_at":now()}),
    )
}

struct Attached {
    evidence: Doc,
    task: Doc,
    artifact: Doc,
}

fn attached(conn: &Connection, item: &Doc) -> Result<Attached> {
    let evidence = resolve(conn, &item.body["evidence_ref"])?;
    let original = resolve(conn, &evidence.body["work_item_ref"])?;
    let task = resolve(conn, &evidence.body["task_ref"])?;
    let artifact = resolve(conn, &evidence.body["artifact_ref"])?;
    let provenance = proof(conn, item, &task, &artifact)?;
    require(
        item.kind == "WorkItem"
            && evidence.kind == KIND
            && evidence.rev == 1
            && evidence.owner == task.owner
            && evidence.body["submitted_by"] == task.owner
            && evidence.body["work_item_id"] == item.id
            && evidence.body["project_id"] == item.spec["project_id"]
            && evidence.body["criterion"] == item.spec["criterion"]
            && evidence.body["run_ref"] == provenance.run.reference()
            && evidence.body["operation_ref"] == provenance.operation.reference()
            && original.kind == "WorkItem"
            && original.id == item.id
            && original.owner == item.owner
            && original.spec == item.spec
            && original.rev < item.rev,
        "RecoveryRequired",
        "work item does not bind its exact evidence and original task",
    )?;
    Ok(Attached {
        evidence,
        task,
        artifact,
    })
}

pub(super) fn review(db: &Db<'_>, item: &Doc, actor: &str) -> Result<Doc> {
    let attached = attached(db.c, item)?;
    require(
        actor != attached.task.owner
            && item.spec["assignee_principal_id"] != actor
            && attached.evidence.body["submitted_by"] != actor,
        "PermissionDenied",
        "work item review requires an independent principal",
    )?;
    policy::can_target(db, actor, result_access::ACTION, &attached.task.id)?;
    let (artifact, _) = result_access(
        db,
        actor,
        &attached.task,
        &attached.evidence.body["artifact_ref"],
    )?;
    live_output(db.c, &attached.task, &artifact)?;
    verify_payload(db, &artifact)?;
    result_access::audit_read(db, actor, "WorkItem.Review", &artifact.id)?;
    Ok(attached.evidence)
}

const RETAINED: [&str; 2] = ["InReview", "Verified"];

fn pinned(item: &Doc) -> bool {
    item.body["lifecycle"]
        .as_str()
        .is_some_and(|state| RETAINED.contains(&state))
}

pub(super) fn release(db: &Db<'_>, item: &Doc, evidence: &Doc) -> Result<()> {
    let attached = attached(db.c, item)?;
    require(
        pinned(item) && attached.evidence.reference() == evidence.reference(),
        "RecoveryRequired",
        "work item evidence changed before releasing its retention pin",
    )?;
    live_output(db.c, &attached.task, &attached.artifact)
}

pub(super) fn view(db: &Db<'_>, item: &Doc, actor: &str) -> Result<Option<Value>> {
    if item.body["evidence_ref"].is_null() {
        return Ok(None);
    }
    let attached = attached(db.c, item)?;
    Ok(
        match result_access(
            db,
            actor,
            &attached.task,
            &attached.evidence.body["artifact_ref"],
        ) {
            Ok((artifact, shared)) => {
                if shared {
                    result_access::audit_read(db, actor, "WorkItem.Evidence", &artifact.id)?;
                }
                Some(attached.evidence.body)
            }
            Err(error) if error.0.code == "PermissionDenied" => None,
            Err(error) => return Err(error),
        },
    )
}

pub(crate) fn retains_artifact(conn: &Connection, artifact: &Doc) -> Result<bool> {
    let mut items = conn.prepare(
        "SELECT w.* FROM versions e INDEXED BY work_item_evidence_by_artifact
         CROSS JOIN current w
         WHERE e.kind='WorkItemEvidence' AND e.body->'artifact_ref'->>'resource_id'=?1
           AND w.id=e.body->>'work_item_id' AND w.kind='WorkItem'
           AND w.body->'evidence_ref'->>'resource_id'=e.id
           AND w.body->>'lifecycle' IN ('InReview','Verified')",
    )?;
    let mut rows = items.query([&artifact.id])?;
    if let Some(item) = rows.next()? {
        let item = row(item)?;
        let attached = attached(conn, &item)?;
        require(
            attached.artifact.reference() == artifact.reference(),
            "RecoveryRequired",
            "work item retention pin disagrees with the exact output revision",
        )?;
        return Ok(true);
    }
    Ok(false)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_items(conn).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            error
        } else {
            err(
                "RecoveryRequired",
                format!("work item evidence verification: {}", error.0.message),
            )
        }
    })
}

fn verify_items(conn: &Connection) -> Result<()> {
    let mut items = conn.prepare(
        "SELECT * FROM current WHERE kind='WorkItem'
         AND (body->>'evidence_ref' IS NOT NULL OR body->>'lifecycle' IN ('InReview','Verified','Closed'))",
    )?;
    for item in items.query_map([], row)? {
        let item = item?;
        let attached = attached(conn, &item)?;
        if pinned(&item) {
            live_output(conn, &attached.task, &attached.artifact)?;
        }
    }
    Ok(())
}
