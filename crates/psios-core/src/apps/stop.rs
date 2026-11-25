//! Authority boundary for stopping an OCI workload and removing its exact container.
use super::{Engine, log_tail};
use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::{Checkpoint, Target, Work},
    require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "application.continue_stop";
pub(crate) fn is_tool(tool: &str) -> bool {
    matches!(tool, "application.stop" | TOOL)
}
pub(crate) const FORMAT: u32 = crate::node::service_control::FORMAT + 1;
const SCHEMA: &str = "CREATE TABLE oci_stop_cleanups (
    original TEXT PRIMARY KEY, cleanup TEXT NOT NULL UNIQUE
) STRICT;";

mod evidence;
mod verification;
pub(crate) use evidence::retains_result;
pub(crate) use verification::{verify, verify_active};

const EPOCH: &str = "oci-stop-checkpoint-epoch";
const INTENT: &str = "OCIStopIntent";
const STOP: &str = "OCIStopDispatch";
const REMOVE: &str = "OCIStopRemoval";
const CLEANUP: &str = "OCIStopCleanupDispatch";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Target,
    pub operation_ref: Value,
    pub evidence_ref: Value,
}

pub(crate) struct Prepared {
    original: Doc,
    checkpoint: Doc,
    evidence: Value,
    binding: Value,
    started_at: String,
}

fn original_app_binding(
    conn: &Connection,
    app: &Doc,
    op: &Doc,
    volume: Option<&Doc>,
) -> Result<Value> {
    let instance = crate::db::resolve(conn, &app.body["instance_ref"])?;
    let plan = crate::db::resolve(conn, &op.body["plan_ref"])?;
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![app.id, op.id],
        |row| row.get(0),
    )?;
    require(
        held && app.kind == "Application"
            && app.body["lifecycle"] == "Running"
            && instance.kind == "RuntimeInstance"
            && instance.owner == app.owner
            && instance.spec["application_id"] == app.id
            && instance.spec["container_id"] == app.body["container_id"]
            && instance.spec["engine_id"] == app.body["engine_id"]
            && instance.spec["start_operation_id"] == app.body["start_operation_id"]
            && plan.body["allowed_effects"][0]["action"] == "application.stop"
            && plan.body["allowed_effects"][0]["resource_ref"] == app.reference()
            && app.body["engine_id"].is_string()
            && app.body["start_operation_id"].is_string()
            && app.body["container_id"].as_str().is_some_and(|id| {
                id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
            }),
        "RecoveryRequired",
        "original OCI stop identity or exclusive application hold is missing",
    )?;
    let attachment = if let Some(v) = volume {
        let attachment = crate::db::resolve(conn, &v.body["attachment_refs"][0])?;
        let held: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
            params![v.id, app.id],
            |row| row.get(0),
        )?;
        require(
            held && app.spec["volume_id"] == v.id
                && v.kind == "Volume"
                && v.body["attachment_refs"] == json!([attachment.reference()])
                && attachment.kind == "VolumeAttachment"
                && attachment.body["volume_id"] == v.id
                && attachment.body["workload_id"] == app.id
                && attachment.body["data_incarnation"] == v.body["data_incarnation"]
                && attachment.body["lifecycle"] == "Attached"
                && attachment.body["effective_access"] == "ReadWrite"
                && plan.body["preconditions"]
                    .as_array()
                    .is_some_and(|conditions| {
                        conditions
                            .iter()
                            .any(|condition| condition["resource_ref"] == v.reference())
                    }),
            "RecoveryRequired",
            "original OCI stop volume version or attachment hold is missing",
        )?;
        Some(attachment.reference())
    } else {
        require(
            app.spec["volume_id"].is_null(),
            "RecoveryRequired",
            "original OCI stop volume is missing",
        )?;
        None
    };
    Ok(json!({
        "application_ref":app.reference(),"instance_ref":instance.reference(),
        "engine_id":app.body["engine_id"],"container_id":app.body["container_id"],
        "start_operation_id":app.body["start_operation_id"],
        "volume_ref":volume.map(Doc::reference),"attachment_ref":attachment,
    }))
}

fn id(kind: &str, operation: &str) -> String {
    let prefix = match kind {
        INTENT => "oci-stop-intent",
        STOP => "oci-stop-dispatch",
        REMOVE => "oci-stop-removal",
        CLEANUP => "oci-stop-cleanup-dispatch",
        _ => unreachable!(),
    };
    format!("{prefix}-{operation}")
}

fn bound(doc: &Doc, kind: &str, op: &Doc) -> bool {
    doc.kind == kind
        && (kind == INTENT || doc.rev == 1)
        && doc.owner == op.owner
        && doc.spec["operation_id"] == op.id
        && doc.body["plan_ref"] == op.body["plan_ref"]
        && doc.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.c.execute_batch(SCHEMA)?;
    db.c.execute_batch(evidence::SCHEMA)?;
    db.create(
        "OCIStopCheckpointEpoch",
        EPOCH,
        crate::policy::SYSTEM,
        json!({}),
        json!({"format":FORMAT}),
    )?;
    Ok(())
}

fn create(db: &Db<'_>, work: &Work, kind: &str, mut body: Value) -> Result<Doc> {
    body["plan_ref"] = work.op.body["plan_ref"].clone();
    body["effect_id"] = json!(work.effect.id);
    db.create(
        kind,
        &id(kind, &work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        body,
    )
}

fn start_intent(db: &Db<'_>, work: &Work, mut body: Value) -> Result<()> {
    body["operation_ref"] = work.op.reference();
    body["effect_ref"] = work.effect.reference();
    body["run_ref"] = work.run.reference();
    create(db, work, INTENT, body)?;
    Ok(())
}

fn advance(db: &Db<'_>, work: &Work, intent: &Doc, kind: &str, mut body: Value) -> Result<()> {
    if kind != REMOVE {
        require(
            intent.rev == 1
                && intent.body["effect_ref"] == work.effect.reference()
                && work.effect.body["phase"] == "PermitBound"
                && db.of(&work.effect.id, "Effect")?.reference() == work.effect.reference()
                && db.of(&work.op.id, "Operation")?.reference() == work.op.reference()
                && db.of(&work.run.id, "Run")?.reference() == work.run.reference()
                && db.of(&work.task.id, "Task")?.reference() == work.task.reference(),
            "Fenced",
            "OCI stop lost its bound execution permit",
        )?;
        let mut effect = work.effect.body.clone();
        effect["phase"] = json!("EffectStarted");
        let effect = db.status(&work.effect, effect)?;
        let mut operation = work.op.body.clone();
        operation["effect_refs"] = json!([effect.reference()]);
        operation["updated_at"] = json!(crate::now());
        let operation = db.status(&work.op, operation)?;
        for subject in [&work.run, &work.task] {
            let mut state = subject.body.clone();
            state["operation_refs"] = json!([operation.reference()]);
            db.status(subject, state)?;
        }
        body["started_effect_ref"] = effect.reference();
        body["started_operation_ref"] = operation.reference();
    }
    let checkpoint = create(db, work, kind, body)?;
    let mut state = intent.body.clone();
    state[if kind == REMOVE {
        "removal_ref"
    } else {
        "dispatch_ref"
    }] = checkpoint.reference();
    db.status(intent, state)?;
    Ok(())
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    let binding = original_app_binding(db.c, &work.target, &work.op, work.volume.as_ref())?;
    start_intent(db, work, json!({"binding":binding}))
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = verification::intent(db.c, op, verification::epoch(db.c)?)? else {
        return Ok(false);
    };
    Ok(intent.dispatch.is_none())
}

pub(crate) fn worker_started(db: &Db<'_>, work: &Work, task: &Doc) -> Result<bool> {
    let op = db.of(&work.op.id, "Operation")?;
    let run = db.of(&work.run.id, "Run")?;
    let effect = db.resolve(&op.body["effect_refs"][0])?;
    let started = !not_dispatched(db, &op)?;
    require(
        op.spec == work.op.spec
            && op.body["plan_ref"] == work.op.body["plan_ref"]
            && op.body["outcome_knowledge"] == "InFlight"
            && effect.id == work.effect.id
            && effect.spec == work.effect.spec
            && effect.rev == work.effect.rev + i64::from(started)
            && db.of(&effect.id, "Effect")?.reference() == effect.reference()
            && run.body["operation_refs"] == json!([op.reference()])
            && task.body["operation_refs"] == json!([op.reference()]),
        "Fenced",
        "OCI stop worker result differs from its current effect",
    )?;
    Ok(started)
}

fn read_checkpoint(db: &Db<'_>, kind: &str, op: &Doc) -> Result<Doc> {
    let checkpoint = db
        .find(&id(kind, &op.id))?
        .ok_or_else(|| err("RecoveryRequired", "OCI stop checkpoint is missing"))?;
    require(
        bound(&checkpoint, kind, op),
        "RecoveryRequired",
        "OCI stop checkpoint does not match operation",
    )?;
    Ok(checkpoint)
}

fn exact_instance(binding: &Value, observation: &Value) -> bool {
    ["engine_id", "container_id", "start_operation_id"]
        .iter()
        .all(|field| binding[*field].is_string() && binding[*field] == observation[*field])
}

fn started(observation: &Value) -> Option<&str> {
    observation["started_at"]
        .as_str()
        .filter(|value| value.len() <= 40 && chrono::DateTime::parse_from_rfc3339(value).is_ok())
}

fn exact_exited(binding: &Value, observation: &Value) -> bool {
    exact_instance(binding, observation)
        && observation["exit_confirmed"] == true
        && observation["docker_state"] == "exited"
        && observation["process_running"] == false
        && observation["runtime_state"] == "Exited"
        && started(observation).is_some()
}

pub(super) fn health(instance: &Value, inspect: &Value) -> Value {
    let state = &inspect["State"];
    let exited = state["Status"] == "exited"
        && state["Running"] == false
        && state["Paused"] == false
        && state["Restarting"] == false
        && state["Dead"] == false
        && state["Pid"] == 0;
    json!({
        "container_id":instance["container_id"],"engine_id":instance["engine_id"],
        "start_operation_id":instance["start_operation_id"],"started_at":state["StartedAt"],
        "docker_state":state["Status"],"exit_confirmed":exited,"process_running":state["Running"],
        "exit_code":state["ExitCode"],
        "runtime_state":if state["Running"]==true {"Running"} else {"Exited"},
        "health":"Unknown","readiness":if state["Running"]==true {"Unknown"} else {"NotReady"},
        "observed_at":crate::now(),
    })
}

fn live(w: &Work, engine: &Engine<'_>) -> Result<Value> {
    let app = &w.target;
    let cid = app.body["container_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI instance identity missing"))?;
    let start = app.body["start_operation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI launch identity missing"))?;
    let inspect = engine.managed(w, cid, start)?;
    require(
        engine.identity()? == app.body["engine_id"],
        "PreconditionFailed",
        "OCI engine changed during inspection",
    )?;
    Ok(health(&app.body, &inspect))
}

fn unknown() -> crate::Error {
    err(
        "OutcomeUnknown",
        "cannot confirm the exact OCI stop or removal postcondition",
    )
}

fn removal_observed(w: &Work, engine: &Engine<'_>, cid: &str) -> Result<()> {
    require(
        engine.removed(w, cid).map_err(|_| unknown())?,
        "OutcomeUnknown",
        "OCI container removal was not verified",
    )
}

pub(crate) fn run(
    w: &Work,
    mut checkpoint: impl FnMut(Checkpoint<'_>) -> Result<()>,
) -> Result<Value> {
    let engine = Engine(&w.cfg);
    let cid = w.target.body["container_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI instance identity missing"))?;
    let before = match live(w, &engine) {
        Ok(observed) => observed,
        Err(_) if engine.removed(w, cid)? => return Ok(json!({"already_absent":true})),
        Err(error) => return Err(error),
    };
    require(
        started(&before).is_some(),
        "PreconditionFailed",
        "OCI invocation start identity unavailable",
    )?;
    checkpoint(Checkpoint::OciStop(&before))?;
    engine.command(&["stop", "--time=5", cid].map(String::from), true)?;
    let tail = log_tail::capture(w, &engine).ok();
    let exited = live(w, &engine).map_err(|_| unknown())?;
    require(
        exact_exited(&w.target.body, &exited) && started(&exited) == started(&before),
        "OutcomeUnknown",
        "exact OCI invocation stop was not verified",
    )?;
    checkpoint(Checkpoint::OciStopRemoval(&exited)).map_err(|_| unknown())?;
    engine
        .command(&["container", "rm", cid].map(String::from), true)
        .map_err(|_| unknown())?;
    removal_observed(w, &engine, cid)?;
    Ok(json!({"retained_tail":tail}))
}

pub(crate) fn record(db: &Db<'_>, work: &Work, sample: &Value, removing: bool) -> Result<()> {
    let binding = original_app_binding(db.c, &work.target, &work.op, work.volume.as_ref())?;
    let intent = read_checkpoint(db, INTENT, &work.op)?;
    require(
        intent.body["binding"] == binding && exact_instance(&binding, sample),
        "Fenced",
        "OCI stop checkpoint does not match original instance",
    )?;
    if removing {
        let original = db.of(&work.op.id, "Operation")?;
        let stop = verification::intent(db.c, &original, verification::epoch(db.c)?)?
            .and_then(|intent| intent.dispatch)
            .ok_or_else(|| err("RecoveryRequired", "OCI stop command checkpoint is missing"))?;
        require(
            stop.body["binding"] == binding
                && exact_exited(&binding, sample)
                && started(sample) == started(&stop.body["observation"]),
            "Fenced",
            "OCI removal checkpoint does not match stopped invocation",
        )?;
    } else {
        require(
            started(sample).is_some(),
            "Fenced",
            "OCI stop checkpoint is missing invocation identity",
        )?;
    }
    advance(
        db,
        work,
        &intent,
        if removing { REMOVE } else { STOP },
        json!({"binding":binding,"observation":sample}),
    )
}

pub(crate) fn prepare(db: &Db<'_>, app: &Doc, request: &Request) -> Result<Prepared> {
    let original = db.resolve(&request.operation_ref)?;
    require(
        original.kind == "Operation"
            && db.get(&original.id)?.reference() == request.operation_ref
            && original.body["outcome_knowledge"] == "Unknown",
        "PreconditionFailed",
        "continuation requires the exact unresolved application stop",
    )?;
    let task_id = original.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI stop task missing"))?;
    let task = db.of(task_id, "Task")?;
    require(
        original.owner == task.owner
            && task.spec["task_type"] == "application.stop"
            && task.body["operation_refs"] == json!([original.reference()]),
        "PreconditionFailed",
        "continuation requires the original application stop task",
    )?;
    let volume = app.spec["volume_id"]
        .as_str()
        .map(|id| db.of(id, "Volume"))
        .transpose()?;
    let binding = original_app_binding(db.c, app, &original, volume.as_ref())?;
    let intent =
        verification::intent(db.c, &original, verification::epoch(db.c)?)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "legacy OCI stop has no continuation checkpoint",
            )
        })?;
    let checkpoint = intent.dispatch.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "original OCI stop dispatch checkpoint is missing",
        )
    })?;
    require(
        intent.removal.is_none(),
        "OutcomeUnknown",
        "original OCI removal may have been sent; reconcile its absence before proceeding",
    )?;
    let (_, exit, evidence) = inspect_health(db, Some(&request.evidence_ref), &original, app)?;
    require(
        checkpoint.body["binding"] == binding
            && exact_exited(&binding, &exit)
            && started(&exit) == started(&checkpoint.body["observation"]),
        "OutcomeUnknown",
        "fresh OCI health does not confirm the original invocation has exited",
    )?;
    Ok(Prepared {
        original,
        checkpoint,
        evidence,
        binding,
        started_at: started(&exit).unwrap().into(),
    })
}

pub(crate) fn fresh_health(
    db: &Db<'_>,
    reference: Option<&Value>,
    op: &Doc,
    app: &Doc,
) -> Result<(Doc, Value)> {
    let (proof, observation, _) = inspect_health(db, reference, op, app)?;
    Ok((proof, observation))
}

fn inspect_health(
    db: &Db<'_>,
    reference: Option<&Value>,
    op: &Doc,
    app: &Doc,
) -> Result<(Doc, Value, Value)> {
    let (proof, observation) =
        crate::broker::inspect_recovery(db, reference, op, app, "application.health")?;
    let snapshot = evidence::capture(db.c, &proof, &observation)?;
    evidence::verify(db.c, &snapshot, op, &app.reference(), false)?;
    Ok((proof, observation, snapshot))
}

fn active(db: &Db<'_>, original: &str) -> Result<Option<String>> {
    Ok(db
        .c
        .query_row(
            "SELECT cleanup FROM oci_stop_cleanups WHERE original=?1",
            [original],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn admissible(db: &Db<'_>, app: &Doc, request: &Request) -> Result<Prepared> {
    let prepared = prepare(db, app, request)?;
    require(
        active(db, &prepared.original.id)?.is_none(),
        "Conflict",
        "the unresolved stop already has an active removal continuation",
    )?;
    Ok(prepared)
}

pub(crate) fn admit(db: &Db<'_>, task: &Doc, prepared: &Prepared) -> Result<()> {
    evidence::admit(db, task, prepared)
}

fn custody(prepared: &Prepared) -> Value {
    json!({"original_operation_ref":prepared.original.reference(),
        "original_checkpoint_ref":prepared.checkpoint.reference(),
        "exit_evidence_ref":prepared.evidence["artifact_ref"],"binding":prepared.binding,
        "started_at":prepared.started_at})
}

pub(crate) fn begin_cleanup(db: &Db<'_>, work: &Work, prepared: &Prepared) -> Result<()> {
    let accepted = evidence::record(db.c, &work.task, true)?;
    require(
        accepted.body["snapshot"] == prepared.evidence
            && accepted.body["original_operation_ref"] == prepared.original.reference()
            && accepted.body["binding"] == prepared.binding,
        "Fenced",
        "OCI cleanup proof changed since admission",
    )?;
    require(
        db.c.execute(
            "INSERT OR IGNORE INTO oci_stop_cleanups VALUES(?1,?2)",
            params![prepared.original.id, work.op.id],
        )? == 1,
        "Conflict",
        "the unresolved stop already has an active removal continuation",
    )?;
    start_intent(
        db,
        work,
        json!({"custody":custody(prepared),"exit_provenance_ref":accepted.reference()}),
    )
}

pub(crate) fn record_cleanup(
    db: &Db<'_>,
    work: &Work,
    request: &Request,
    sample: &Value,
) -> Result<()> {
    let prepared = prepare(db, &work.target, request)?;
    let intent = read_checkpoint(db, INTENT, &work.op)?;
    let accepted = evidence::record(db.c, &work.task, true)?;
    require(
        active(db, &prepared.original.id)?.as_deref() == Some(work.op.id.as_str())
            && intent.body["custody"] == custody(&prepared)
            && intent.body["exit_provenance_ref"] == accepted.reference()
            && accepted.body["snapshot"] == prepared.evidence
            && exact_exited(&prepared.binding, sample)
            && started(sample) == Some(prepared.started_at.as_str()),
        "Fenced",
        "OCI cleanup lost its exact removal authority",
    )?;
    advance(
        db,
        work,
        &intent,
        CLEANUP,
        json!({"custody":custody(&prepared),"observation":sample}),
    )?;
    evidence::release(db, &work.task.id)
}

pub(crate) fn cleanup(
    w: &Work,
    checkpoint: impl FnOnce(Checkpoint<'_>) -> Result<()>,
) -> Result<Value> {
    let prepared = w
        .stop_cleanup
        .as_ref()
        .ok_or_else(|| err("Fenced", "OCI stop cleanup input unavailable"))?;
    let engine = Engine(&w.cfg);
    let sample = live(w, &engine)?;
    require(
        exact_exited(&prepared.binding, &sample)
            && started(&sample) == Some(prepared.started_at.as_str()),
        "PreconditionFailed",
        "original stopped invocation changed before removal",
    )?;
    checkpoint(Checkpoint::OciStopCleanup(&sample))?;
    let cid = prepared.binding["container_id"].as_str().unwrap();
    engine.command(&["container", "rm", cid].map(String::from), true)?;
    removal_observed(w, &engine, cid)?;
    Ok(receipt(&prepared.original.reference(), &prepared.binding))
}

fn receipt(original_ref: &Value, binding: &Value) -> Value {
    json!({"original_operation_ref":original_ref,"application_ref":binding["application_ref"],
        "engine_id":binding["engine_id"],"container_id":binding["container_id"],
        "start_operation_id":binding["start_operation_id"],"removal_confirmed":true})
}

pub(crate) fn release(db: &Db<'_>, operation: &str, task: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM oci_stop_cleanups WHERE cleanup=?1",
        [operation],
    )?;
    evidence::release(db, task)
}

pub(crate) fn cleanup_held(db: &Db<'_>, request: &Request, op: &Doc) -> Result<bool> {
    let original = db.resolve(&request.operation_ref)?;
    Ok(active(db, &original.id)?.as_deref() == Some(op.id.as_str()))
}

pub(crate) fn original_ready(db: &Db<'_>, app: &Doc, op: &Doc, volume: Option<&Doc>) -> Result<()> {
    require(
        active(db, &op.id)?.is_none(),
        "Conflict",
        "settle the exact OCI removal continuation first",
    )?;
    // Retain the existing absence-only reconciliation path for in-flight
    // stop operations from before OCIStopIntent was introduced.
    if let Some(intent) = verification::intent(db.c, op, verification::epoch(db.c)?)? {
        let binding = original_app_binding(db.c, app, op, volume)?;
        require(
            intent
                .dispatch
                .is_some_and(|checkpoint| checkpoint.body["binding"] == binding),
            "RecoveryRequired",
            "original OCI stop checkpoint identity changed",
        )?;
    }
    Ok(())
}

pub(crate) fn settle_cleanup(db: &Db<'_>, work: &Work, observed: &Value) -> Result<Value> {
    let prepared = work
        .stop_cleanup
        .as_ref()
        .ok_or_else(|| err("Fenced", "OCI stop cleanup input unavailable"))?;
    let recorded = read_checkpoint(db, CLEANUP, &db.of(&work.op.id, "Operation")?)?;
    let binding =
        original_app_binding(db.c, &work.target, &prepared.original, work.volume.as_ref())?;
    require(
        active(db, &prepared.original.id)?.as_deref() == Some(work.op.id.as_str())
            && prepared.binding == binding
            && recorded.body["custody"] == custody(prepared)
            && *observed == receipt(&prepared.original.reference(), &binding),
        "OutcomeUnknown",
        "OCI removal result does not match the checkpointed continuation",
    )?;
    Ok(observed.clone())
}

pub(crate) fn reconcile_cleanup(
    db: &Db<'_>,
    app: &Doc,
    op: &Doc,
    request: &Request,
    proof: &Doc,
    observation: &Value,
) -> Result<(Value, Value)> {
    let recorded = read_checkpoint(db, CLEANUP, op)?;
    let original = db.resolve(&request.operation_ref)?;
    let volume = app.spec["volume_id"]
        .as_str()
        .map(|id| db.of(id, "Volume"))
        .transpose()?;
    let binding = original_app_binding(db.c, app, &original, volume.as_ref())?;
    let prepared = &recorded.body["custody"];
    require(
        active(db, &original.id)?.as_deref() == Some(op.id.as_str())
            && original.body["outcome_knowledge"] == "Unknown"
            && db.get(&original.id)?.reference() == original.reference()
            && prepared["original_operation_ref"] == original.reference()
            && prepared["exit_evidence_ref"] == request.evidence_ref
            && prepared["binding"] == binding
            && exact_instance(&binding, observation)
            && observation["removal_confirmed"] == true,
        "OutcomeUnknown",
        "fresh OCI health does not verify removal of the exact stopped invocation",
    )?;
    Ok((
        receipt(&original.reference(), &binding),
        json!({
            "observation_ref":proof.reference(),"checkpoint_ref":recorded.reference(),"conclusion":"RemovalConfirmed",
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_exited_runtime_is_removal_evidence() {
        let binding =
            json!({"engine_id":"engine","container_id":"container","start_operation_id":"launch"});
        let inspect = json!({"State":{"Status":"exited","Running":false,"Paused":false,
            "Restarting":false,"Dead":false,"Pid":0,"StartedAt":"2026-11-25T12:00:00Z"}});
        let exited = health(&binding, &inspect);
        assert!(exact_exited(&binding, &exited));
        for (field, changed) in [
            ("Status", json!("created")),
            ("Running", json!(true)),
            ("Paused", json!(true)),
            ("Restarting", json!(true)),
            ("Dead", json!(true)),
            ("Pid", json!(17)),
            ("StartedAt", Value::Null),
        ] {
            let mut invalid = inspect.clone();
            invalid["State"][field] = changed;
            assert!(
                !exact_exited(&binding, &health(&binding, &invalid)),
                "{field}"
            );
        }
        for field in ["engine_id", "container_id", "start_operation_id"] {
            let mut unrelated = exited.clone();
            unrelated[field] = json!("replacement");
            assert!(!exact_exited(&binding, &unrelated), "{field}");
        }
    }
}
