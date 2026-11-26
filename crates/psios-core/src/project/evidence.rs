//! Bind WorkItem acceptance to the original task receipt without granting result access.
use super::workflow_proof;
use crate::{
    Result,
    apps::log_tail,
    artifact,
    broker::result_access,
    db::{Db, Doc, find, resolve, row},
    err, now, policy, require,
};
use psios_api::MAX_FRAME;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub(crate) const SCHEMA: &str = include_str!("evidence.sql");
pub(crate) const KIND: &str = "WorkItemEvidence";
const MAX_SUBMISSIONS: i64 = 8;
const MAX_ARTIFACT_ASSOCIATIONS: i64 = 64;

struct Production {
    run: Doc,
    operation: Doc,
}

fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "work item evidence resource is missing"))
}

fn proof(
    conn: &Connection,
    item: &Doc,
    task: &Doc,
    artifact: &Doc,
    sealed_run: Option<&Value>,
) -> Result<Production> {
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
        matches!(
            criterion["kind"].as_str(),
            Some("TaskResult" | "WorkflowTaskResult")
        ) && task.spec["task_type"] == criterion["tool"],
        "PreconditionFailed",
        "task tool does not satisfy the work item criterion",
    )?;
    let run = match sealed_run {
        Some(reference) => resolve(conn, reference)?,
        None => needed(
            conn,
            task.body["current_run_id"].as_str().unwrap_or_default(),
        )?,
    };
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
            && run.id == task.body["current_run_id"]
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

fn live_output(conn: &Connection, task: &Doc, artifact: &Doc) -> Result<Doc> {
    artifact::retention::require_content(conn, artifact)?;
    let current = needed(conn, &task.id)?;
    require(
        current.kind == "Task"
            && current.owner == task.owner
            && current.spec == task.spec
            && current.body["lifecycle"] == "Succeeded"
            && current.body["outcome_knowledge"] == "Known"
            && current.body["operation_refs"] == task.body["operation_refs"]
            && current.body["current_run_id"] == task.body["current_run_id"]
            && current.body["result"]["availability"] == "Available"
            && current.body["result"]["artifact_refs"]
                .as_array()
                .is_some_and(|refs| refs.contains(&artifact.reference())),
        "PreconditionFailed",
        "the original successful task output is no longer available",
    )?;
    Ok(current)
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

fn artifact_associations(conn: &Connection, artifact: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM (
            SELECT 1 FROM versions INDEXED BY work_item_evidence_by_artifact
             WHERE kind='WorkItemEvidence' AND body->'artifact_ref'->>'resource_id'=?1 LIMIT ?2
         )",
        params![artifact, MAX_ARTIFACT_ASSOCIATIONS + 1],
        |row| row.get(0),
    )?)
}

fn check_admission(conn: &Connection, item: &str, artifact: &str) -> Result<()> {
    let submissions: i64 = conn.query_row(
        "SELECT count(*) FROM (
            SELECT 1 FROM versions INDEXED BY work_item_evidence_by_item
             WHERE kind='WorkItemEvidence' AND body->>'work_item_id'=?1 LIMIT ?2
         )",
        params![item, MAX_SUBMISSIONS],
        |row| row.get(0),
    )?;
    require(
        submissions < MAX_SUBMISSIONS,
        "QuotaExceeded",
        "work item has reached its retained evidence submission limit",
    )?;
    require(
        artifact_associations(conn, artifact)? < MAX_ARTIFACT_ASSOCIATIONS,
        "QuotaExceeded",
        "task output has reached its retained work item evidence limit",
    )
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
    let provenance = proof(db.c, item, &task, &artifact, None)?;
    log_tail::authorize_task(db, actor, &task)?;
    let current = live_output(db.c, &task, &artifact)?;
    require(
        current.reference() == task.reference(),
        "PreconditionFailed",
        "evidence submission requires the current task revision",
    )?;
    let workflow_witness = workflow_proof::witness(db.c, item, &task, &artifact, None)?;
    check_admission(db.c, &item.id, &artifact.id)?;
    verify_payload(db, &artifact)?;
    if shared {
        result_access::audit_read(db, actor, "WorkItem.SubmitEvidence", &artifact.id)?;
    }
    let mut body = json!({"work_item_id":item.id, "project_id":item.spec["project_id"],
        "work_item_ref":item.reference(), "criterion":item.spec["criterion"],
        "task_ref":task.reference(), "artifact_ref":artifact.reference(),
        "operation_ref":provenance.operation.reference(), "run_ref":provenance.run.reference(),
        "submitted_by":actor, "submitted_at":now()});
    if let Some(witness) = workflow_witness {
        body["workflow_witness"] = witness;
    }
    db.record(KIND, actor, body)
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
    let provenance = proof(
        conn,
        item,
        &task,
        &artifact,
        Some(&evidence.body["run_ref"]),
    )?;
    require(
        item.kind == "WorkItem"
            && evidence.kind == KIND
            && evidence.rev == 1
            && evidence.owner == task.owner
            && evidence.body["submitted_by"] == task.owner
            && evidence.body["work_item_id"] == item.id
            && evidence.body["project_id"] == item.spec["project_id"]
            && evidence.body["criterion"] == item.spec["criterion"]
            && evidence.body["operation_ref"] == provenance.operation.reference()
            && original.kind == "WorkItem"
            && original.id == item.id
            && original.owner == item.owner
            && original.spec == item.spec
            && original.rev < item.rev,
        "RecoveryRequired",
        "work item does not bind its exact evidence and original task",
    )?;
    workflow_proof::witness(conn, item, &task, &artifact, Some(&evidence))?;
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
    live_output(db.c, &attached.task, &attached.artifact).map(|_| ())
}

pub(super) fn abandon(db: &Db<'_>, item: &Doc) -> Result<()> {
    require(
        pinned(item),
        "PreconditionFailed",
        "work item has no active evidence to abandon",
    )?;
    attached(db.c, item).map(|_| ())
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
                let mut details = attached.evidence.body;
                workflow_proof::redact(db, actor, &mut details)?;
                Some(details)
            }
            Err(error) if error.0.code == "PermissionDenied" => None,
            Err(error) => return Err(error),
        },
    )
}

pub(crate) fn retains_artifact(conn: &Connection, artifact: &Doc) -> Result<bool> {
    let associations = artifact_associations(conn, &artifact.id)?;
    if associations == 0 {
        return Ok(false);
    }
    require(
        associations <= MAX_ARTIFACT_ASSOCIATIONS,
        "RecoveryRequired",
        "task output exceeds its verified work item evidence limit",
    )?;
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
    verify_history(conn)
        .and_then(|()| verify_items(conn))
        .map_err(|error| {
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

fn verify_history(conn: &Connection) -> Result<()> {
    let invalid: bool = conn.query_row(
        "SELECT EXISTS (
            SELECT 1 FROM versions INDEXED BY work_item_evidence_by_item
             WHERE kind='WorkItemEvidence' GROUP BY body->>'work_item_id'
             HAVING count(*)>?1 OR typeof(body->>'work_item_id')<>'text'
         ) OR EXISTS (
            SELECT 1 FROM versions INDEXED BY work_item_evidence_by_artifact
             WHERE kind='WorkItemEvidence' GROUP BY body->'artifact_ref'->>'resource_id'
             HAVING count(*)>?2 OR typeof(body->'artifact_ref'->>'resource_id')<>'text'
         )",
        params![MAX_SUBMISSIONS, MAX_ARTIFACT_ASSOCIATIONS],
        |row| row.get(0),
    )?;
    require(
        !invalid,
        "RecoveryRequired",
        "work item evidence history exceeds its bounds or has an invalid association",
    )?;
    let mut submitted = conn.prepare(
        "SELECT * FROM versions WHERE kind='WorkItem' AND body->>'lifecycle'='InReview'",
    )?;
    let mut indexed = 0i64;
    for item in submitted.query_map([], row)? {
        let item = item?;
        let evidence = resolve(conn, &item.body["evidence_ref"])?;
        let original = resolve(conn, &evidence.body["work_item_ref"])?;
        require(
            evidence.kind == KIND
                && evidence.rev == 1
                && original.id == item.id
                && original.kind == "WorkItem"
                && original.rev.checked_add(1) == Some(item.rev)
                && original.spec == item.spec
                && original.body["lifecycle"] == "Ready"
                && original.body["evidence_ref"].is_null()
                && evidence.owner == item.spec["assignee_principal_id"]
                && evidence.body["submitted_by"] == evidence.owner
                && evidence.body["criterion"] == item.spec["criterion"]
                && evidence.body["work_item_id"] == item.id
                && evidence.body["project_id"] == item.spec["project_id"],
            "RecoveryRequired",
            "work item evidence is outside its original submission revision",
        )?;
        if item.spec["criterion"]["kind"] == "WorkflowTaskResult"
            || evidence.body.get("workflow_witness").is_some()
        {
            // Rejected submissions retain their exact evidence history after the byte pin is released.
            attached(conn, &item)?;
        }
        indexed += 1;
    }
    let total: i64 = conn.query_row(
        "SELECT count(*) FROM versions INDEXED BY work_item_evidence_by_item WHERE kind='WorkItemEvidence'",
        [],
        |row| row.get(0),
    )?;
    require(
        indexed == total,
        "RecoveryRequired",
        "work item history has unbound evidence",
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store, broker};
    use psios_api::{Request, VERSION, id};

    fn call(store: &mut Store, token: &str, method: &str, params: Value) -> Value {
        store
            .request(
                rustix::process::geteuid().as_raw(),
                Request {
                    schema_version: VERSION.into(),
                    request_id: id("project-evidence-test"),
                    token: token.into(),
                    method: method.into(),
                    params,
                },
            )
            .unwrap()
    }

    fn initialized() -> (tempfile::TempDir, Store, String) {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::local(&directory.path().join("core"));
        Store::init(&config).unwrap();
        let token = std::fs::read_to_string(config.witness_dir.join("owner.token")).unwrap();
        let store = Store::open(config).unwrap();
        (directory, store, token)
    }

    fn successful_task() -> (tempfile::TempDir, Store, String, String) {
        let (directory, mut store, token) = initialized();
        let prepared = call(
            &mut store,
            &token,
            "Task.Prepare",
            json!({"tool":"system.status","input":{},"key":id("project-evidence-source"),"delegation_id":"owner-delegation"}),
        );
        let submitted = call(&mut store, &token, "Task.Submit", prepared);
        let task_id = submitted["task_id"].as_str().unwrap();
        settle_task(&mut store, task_id);
        (directory, store, token, task_id.into())
    }

    fn settle_task(store: &mut Store, task_id: &str) {
        assert_eq!(
            store
                .transaction("claim-evidence-source", broker::claim)
                .unwrap()
                .as_deref(),
            Some(task_id)
        );
        let work = store
            .transaction("dispatch-evidence-source", |db| {
                broker::dispatch(db, task_id)
            })
            .unwrap()
            .unwrap();
        let result = work.perform(|checkpoint| {
            store.transaction("witness-source", |db| work.checkpoint(db, checkpoint))
        });
        store
            .transaction("finish-evidence-source", |db| {
                broker::finish(db, &work, result)
            })
            .unwrap();
    }

    #[test]
    fn workflow_evidence_rejects_a_later_witness_even_after_the_pin_is_released() {
        let (_directory, mut store, token) = initialized();
        let grant = call(
            &mut store,
            &token,
            "Grant.Issue",
            json!({
                "principal_id":policy::OWNER,"actions":["workflow.manage","task.submit","task.read","system.status"],
                "tool_targets":[policy::PROFILE],"units":1,"ttl_seconds":120,
            }),
        );
        let definition = call(
            &mut store,
            &token,
            "Workflow.Define",
            json!({
                "key":"evidence-workflow-definition",
                "steps":[{"id":"observe","tool":"system.status","input":{},"after":[]}],
            }),
        );
        let run = call(
            &mut store,
            &token,
            "Workflow.Start",
            json!({
                "key":"evidence-workflow-run","definition_ref":definition["reference"],
                "delegation_id":grant["delegation_ref"]["resource_id"],"deadline_at":crate::after(60),
            }),
        );
        let run_id = run["reference"]["resource_id"].as_str().unwrap();
        crate::workflow::tick(&mut store).unwrap();
        let active = call(&mut store, &token, "Workflow.Get", json!({"id":run_id}));
        let task_id = active["state"]["steps"][0]["task_receipt"]["task_id"]
            .as_str()
            .unwrap();
        settle_task(&mut store, task_id);
        crate::workflow::tick(&mut store).unwrap();

        store.transaction("check-historical-workflow-evidence", |db| {
            let settled = db.of(task_id, "Task")?;
            let workflow = db.of(run_id, "WorkflowRun")?;
            let output = db.resolve(&settled.body["result"]["artifact_refs"][0])?;
            let mut amended = settled.body.clone();
            amended["conditions"] = json!([broker::condition("MetadataChecked", "result unchanged")]);
            let task = db.status(&settled, amended)?;
            let ready = db.create(
                "WorkItem", "workflow-evidence-fixture", policy::OWNER,
                json!({"project_id":"project-fixture","assignee_principal_id":policy::OWNER,"criterion":{
                    "kind":"WorkflowTaskResult","definition_ref":definition["reference"],
                    "step_id":"observe","tool":"system.status","target_ref":db.get(policy::PROFILE)?.reference(),
                }}),
                json!({"lifecycle":"Ready"}),
            )?;
            let evidence = attach(db, &ready, policy::OWNER, &task.reference(), &output.reference())?;
            assert_eq!(evidence.body["task_ref"], task.reference());
            assert_eq!(evidence.body["workflow_witness"]["task_ref"], settled.reference());
            assert_eq!(evidence.body["workflow_witness"]["workflow_run_ref"], workflow.reference());
            let mut historical = db.status(&ready, json!({"lifecycle":"InReview","evidence_ref":evidence.reference()}))?;
            // Only historical evidence remains after rejection.
            db.status(&historical, json!({"lifecycle":"Ready"}))?;
            verify(db.c)?;

            let later = db.status(&workflow, workflow.body.clone())?;
            let mut forged = evidence;
            forged.body["workflow_witness"]["workflow_run_ref"] = later.reference();
            // Keep the outer evidence reference valid so verification has to check the witness itself.
            historical.body["evidence_ref"] = forged.reference();
            for doc in [&forged, &historical] {
                db.c.execute("UPDATE versions SET body=?1 WHERE id=?2 AND rev=?3",
                    params![doc.body, doc.id, doc.rev])?;
            }
            let failure = verify(db.c).unwrap_err();
            assert_eq!(failure.0.code, "RecoveryRequired");
            assert!(failure.0.message.contains("lost or changed its exact workflow step witness"));
            Ok(())
        }).unwrap();
    }

    #[test]
    fn active_evidence_keeps_its_original_output_across_task_metadata_revisions() {
        let (_directory, mut store, _token, task_id) = successful_task();
        store
            .transaction("verify-work-item-output", |db| {
                let issued = db.of(&task_id, "Task")?;
                let output = db.resolve(&issued.body["result"]["artifact_refs"][0])?;
                let mut body = issued.body.clone();
                body["conditions"] = json!([broker::condition(
                    "ArchiveVerified",
                    "task metadata was checked"
                )]);
                let advanced = db.status(&issued, body)?;
                assert_ne!(issued.reference(), advanced.reference());
                assert_eq!(
                    live_output(db.c, &issued, &output)?.reference(),
                    advanced.reference()
                );

                let mut body = advanced.body.clone();
                body["current_run_id"] = json!("different-run");
                db.status(&advanced, body)?;
                assert_eq!(
                    live_output(db.c, &issued, &output).unwrap_err().0.code,
                    "PreconditionFailed"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn abandonment_releases_missing_payload_and_reassignment_preserves_rejected_proof() {
        let (_directory, mut store, token, task_id) = successful_task();
        let reader = call(
            &mut store,
            &token,
            "Agent.Register",
            json!({"name":"evidence-maintenance-reader"}),
        );
        let reader = reader["principal_id"].as_str().unwrap();
        call(
            &mut store,
            &token,
            "Grant.Issue",
            json!({
                "principal_id":reader, "actions":[result_access::ACTION],
                "tool_targets":[task_id],"ttl_seconds":120,"units":1,
            }),
        );
        store.transaction("work-item-abandonment-evidence", |db| {
            let task = db.of(&task_id, "Task")?;
            let output = db.resolve(&task.body["result"]["artifact_refs"][0])?;
            let operation = db.resolve(&task.body["operation_refs"][0])?;
            let plan = db.resolve(&operation.body["plan_ref"])?;
            let spec = json!({"project_id":"evidence-maintenance-project","assignee_principal_id":policy::OWNER,
                "criterion":{"kind":"TaskResult","tool":"system.status",
                             "target_ref":plan.body["allowed_effects"][0]["resource_ref"]}});
            let submitted = |name: &str| -> Result<(Doc,Doc)> {
                let ready = db.create("WorkItem", name, policy::OWNER, spec.clone(), json!({"lifecycle":"Ready"}))?;
                let evidence = attach(db, &ready, policy::OWNER, &task.reference(), &output.reference())?;
                let item = db.status(&ready, json!({"lifecycle":"InReview","evidence_ref":evidence.reference()}))?;
                Ok((item, evidence))
            };
            let (abandoning, original) = submitted("abandoned-evidence-test")?;
            let (rejected, rejected_evidence) = submitted("reassigned-evidence-test")?;
            release(db, &rejected, &rejected_evidence)?;
            let rejected = db.status(&rejected, json!({"lifecycle":"Ready"}))?;
            let mut reassigned = spec.clone();
            reassigned["assignee_principal_id"] = json!(reader);
            let new = db.update(&rejected, reassigned, rejected.body.clone())?;
            assert!(view(db, &new, reader)?.is_none());
            verify(db.c)?;

            // Model lost result bytes together with a later observation of unavailability.
            // The original sealed task, run and artifact references still exist.
            assert!(db.c.execute("DELETE FROM artifact_chunks WHERE artifact_id=?1", [&output.id])? > 0);
            let mut unavailable = task.body.clone();
            unavailable["result"]["availability"] = json!("Unavailable");
            db.status(&task, unavailable)?;
            let run = db.resolve(&original.body["run_ref"])?;
            let mut changed = run.body.clone();
            changed["conditions"] = json!([broker::condition("MetadataVerified","run metadata was checked")]);
            db.status(&run, changed)?;
            assert!(verify_payload(db, &output).is_err());
            assert!(retains_artifact(db.c, &output)?);
            assert_eq!(release(db, &abandoning, &original).unwrap_err().0.code, "PreconditionFailed");
            abandon(db, &abandoning)?;
            let abandoned = db.status(&abandoning, json!({"lifecycle":"Abandoned","evidence_ref":original.reference()}))?;
            assert!(!retains_artifact(db.c, &output)?);
            let visible = view(db, &abandoned, reader)?.unwrap();
            assert_eq!(visible["task_ref"], task.reference());
            assert_eq!(visible["artifact_ref"], output.reference());
            assert!(view(db, &abandoned, policy::SYSTEM)?.is_none());
            verify(db.c)
        }).unwrap();
    }
}
