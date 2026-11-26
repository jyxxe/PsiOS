//! Retain the accepted health bytes until dispatch and keep their provenance afterwards.
use super::*;
use crate::{
    artifact,
    db::{find, resolve},
};
use psios_api::{canonical, sha256};

const KIND: &str = "OCIStopExitEvidence";
const MAX_BYTES: usize = 16 * 1024;
pub(super) const SCHEMA: &str = "CREATE TABLE oci_stop_exit_pins (
    task TEXT PRIMARY KEY REFERENCES queue(task) ON DELETE CASCADE,
    producer TEXT NOT NULL
) STRICT;
CREATE INDEX oci_stop_exit_pins_producer ON oci_stop_exit_pins(producer);";

fn record_id(task: &str) -> String {
    format!("oci-stop-exit-evidence-{task}")
}

fn identified(conn: &Connection, value: &Value) -> Result<Doc> {
    let id = value.as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI exit proof has no producer identity",
        )
    })?;
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "OCI exit proof producer is missing"))
}

pub(super) fn capture(conn: &Connection, proof: &Doc, observation: &Value) -> Result<Value> {
    let task = identified(conn, &proof.spec["task_id"])?;
    let run = identified(conn, &proof.spec["run_id"])?;
    let operation = identified(conn, &proof.spec["operation_id"])?;
    let effect = resolve(conn, last_effect(&operation)?)?;
    Ok(json!({
        "artifact_ref":proof.reference(),"task_ref":task.reference(),"run_ref":run.reference(),
        "operation_ref":operation.reference(),"effect_ref":effect.reference(),"observation":observation
    }))
}

fn last_effect(operation: &Doc) -> Result<&Value> {
    operation.body["effect_refs"]
        .as_array()
        .and_then(|refs| refs.last())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "OCI health producer has no confirmed effect",
            )
        })
}

pub(super) fn verify(
    conn: &Connection,
    snapshot: &Value,
    original: &Doc,
    app_ref: &Value,
    pinned: bool,
) -> Result<Doc> {
    let proof = resolve(conn, &snapshot["artifact_ref"])?;
    let source = resolve(conn, &snapshot["task_ref"])?;
    let run = resolve(conn, &snapshot["run_ref"])?;
    let operation = resolve(conn, &snapshot["operation_ref"])?;
    let effect = resolve(conn, &snapshot["effect_ref"])?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let fresh: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions sample JOIN versions lost ON sample.seq>lost.seq
         WHERE sample.kind='Run' AND sample.id=?1 AND sample.rev=1 AND lost.id=?2 AND lost.rev=?3)",
        params![run.id, original.id, original.rev],
        |r| r.get(0),
    )?;
    let refs = json!([proof.reference()]);
    let bytes = canonical(&snapshot["observation"]);
    require(
        fresh
            && bytes.len() <= MAX_BYTES
            && snapshot["artifact_ref"]["digest"] == sha256(&bytes)
            && artifact::is_json(&proof)
            && proof.rev == 1
            && artifact::is_task_output(&proof, &source)
            && source.kind == "Task"
            && source.spec["task_type"] == "application.health"
            && source.body["lifecycle"] == "Succeeded"
            && source.body["outcome_knowledge"] == "Known"
            && source.body["result"]["availability"] == "Available"
            && source.body["result"]["artifact_refs"] == refs
            && source.body["operation_refs"] == json!([operation.reference()])
            && run.kind == "Run"
            && run.owner == source.owner
            && run.body["task_id"] == source.id
            && run.id == source.body["current_run_id"]
            && run.body["lifecycle"] == "Succeeded"
            && run.body["outcome_knowledge"] == "Known"
            && run.body["output_refs"] == refs
            && run.body["operation_refs"] == source.body["operation_refs"]
            && operation.kind == "Operation"
            && operation.id == proof.spec["operation_id"]
            && operation.owner == source.owner
            && operation.spec["task_id"] == source.id
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && *last_effect(&operation)? == effect.reference()
            && plan.kind == "OperationPlan"
            && plan.owner == source.owner
            && plan.body["allowed_effects"][0]["action"] == "application.health"
            && plan.body["allowed_effects"][0]["resource_ref"] == *app_ref
            && effect.kind == "Effect"
            && effect.owner == source.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["action"] == "application.health"
            && effect.body["resource_ref"] == *app_ref
            && effect.body["phase"] == "EffectConfirmed"
            && effect.body["outcome_knowledge"] == "Known"
            && effect.body["evidence_refs"] == refs
            && proof.spec["effect_id"] == effect.id,
        "OutcomeUnknown",
        "OCI health proof does not match the independent producer and exact digest",
    )?;
    if pinned {
        require(
            artifact::checkpoint_bytes(conn, &proof, MAX_BYTES)? == bytes,
            "RecoveryRequired",
            "retained OCI exit proof differs from its admitted bytes",
        )?;
    }
    Ok(source)
}

pub(super) fn admit(db: &Db<'_>, consumer: &Doc, prepared: &Prepared) -> Result<()> {
    let producer = verify(
        db.c,
        &prepared.evidence,
        &prepared.original,
        &prepared.binding["application_ref"],
        true,
    )?;
    db.create(KIND, &record_id(&consumer.id), &consumer.owner, json!({"task_id":consumer.id}),
        json!({"task_ref":consumer.reference(),"original_operation_ref":prepared.original.reference(),
            "binding":prepared.binding,"snapshot":prepared.evidence}))?;
    db.c.execute(
        "INSERT INTO oci_stop_exit_pins VALUES(?1,?2)",
        params![consumer.id, producer.id],
    )?;
    Ok(())
}

pub(super) fn record(conn: &Connection, task: &Doc, pinned: bool) -> Result<Doc> {
    let record = find(conn, &record_id(&task.id))?
        .ok_or_else(|| err("RecoveryRequired", "accepted OCI stop evidence is missing"))?;
    let accepted = resolve(conn, &record.body["task_ref"])?;
    let original = resolve(conn, &record.body["original_operation_ref"])?;
    let binding = &record.body["binding"];
    let snapshot = &record.body["snapshot"];
    let input = resolve(conn, &task.spec["input_ref"])?;
    let request: Request = crate::decode(crate::broker::dependencies::tool_input(&input)?)?;
    require(
        record.kind == KIND
            && record.rev == 1
            && record.owner == task.owner
            && record.spec["task_id"] == task.id
            && accepted.rev == 1
            && accepted.kind == "Task"
            && accepted.id == task.id
            && accepted.owner == task.owner
            && task.spec == accepted.spec
            && task.spec["task_type"] == TOOL
            && original.kind == "Operation"
            && original.body["outcome_knowledge"] == "Unknown"
            && request.operation_ref == original.reference()
            && request.evidence_ref == snapshot["artifact_ref"]
            && request.target.id == binding["application_ref"]["resource_id"]
            && request.target.expected_resource_version == binding["application_ref"]["version"]
            && exact_exited(binding, &snapshot["observation"]),
        "RecoveryRequired",
        "OCI exit evidence differs from the admitted Task",
    )?;
    let producer = verify(
        conn,
        snapshot,
        &original,
        &binding["application_ref"],
        pinned,
    )
    .map_err(|_| {
        err(
            "RecoveryRequired",
            "admitted OCI exit proof producer or digest is invalid",
        )
    })?;
    let live: Option<String> = conn
        .query_row(
            "SELECT producer FROM oci_stop_exit_pins WHERE task=?1",
            [&task.id],
            |r| r.get(0),
        )
        .optional()?;
    require(
        if pinned {
            live.as_deref() == Some(producer.id.as_str())
        } else {
            live.is_none()
        },
        "RecoveryRequired",
        "OCI exit evidence retention pin disagrees with the dispatch phase",
    )?;
    Ok(record)
}

pub(super) fn verify_pins(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT t.* FROM queue q JOIN current t ON q.task=t.id
         WHERE q.phase IN ('WaitingApproval','WaitingResource','Queued','Claimed','Dispatched')
           AND t.spec->>'task_type'=?1
           AND NOT EXISTS(SELECT 1 FROM versions WHERE id='oci-stop-cleanup-dispatch-'||q.operation)",
    )?;
    let mut pinned = 0i64;
    for task in statement.query_map([TOOL], crate::db::row)? {
        record(conn, &task?, true)?;
        pinned += 1;
    }
    let actual: i64 =
        conn.query_row("SELECT count(*) FROM oci_stop_exit_pins", [], |r| r.get(0))?;
    require(
        pinned == actual,
        "RecoveryRequired",
        "OCI exit evidence has an orphaned retention pin",
    )
}

pub(super) fn verify_history(conn: &Connection) -> Result<()> {
    let mut statement = conn.prepare(
        "SELECT t.*, EXISTS(SELECT 1 FROM oci_stop_exit_pins p WHERE p.task=t.id) AS pinned
         FROM current t WHERE t.kind='Task' AND t.spec->>'task_type'=?1",
    )?;
    let mut expected = 0i64;
    for entry in statement.query_map([TOOL], |r| {
        Ok((crate::db::row(r)?, r.get::<_, bool>("pinned")?))
    })? {
        let (task, pinned) = entry?;
        record(conn, &task, pinned)?;
        expected += 1;
    }
    let actual: i64 =
        conn.query_row("SELECT count(*) FROM current WHERE kind=?1", [KIND], |r| {
            r.get(0)
        })?;
    require(
        expected == actual,
        "RecoveryRequired",
        "OCI exit evidence has an orphaned provenance record",
    )
}

pub(crate) fn retains_result(db: &Db<'_>, producer: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM oci_stop_exit_pins WHERE producer=?1)",
        [producer],
        |r| r.get(0),
    )?)
}

pub(super) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute("DELETE FROM oci_stop_exit_pins WHERE task=?1", [task])?;
    Ok(())
}
