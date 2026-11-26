//! Restart only the observed invocation of an authority-managed OCI container.
use super::{Engine, verify_isolation};
use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::Work,
    now, require,
};
use chrono::DateTime;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "application.restart";
pub(crate) const INSPECT: &str = "application.inspect_instance";
pub(crate) const FORMAT: u32 = crate::storage::restore_publication::FORMAT + 1;
pub(crate) const CUSTODY_FORMAT: u32 = super::stop::FORMAT + 1;

mod journal;
mod settlement;
pub(crate) use journal::verify_active;
pub(crate) use settlement::{record_settlement, verify};

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    journal::migrate(db)
}

const INTENT: &str = "OCIRestartIntent";
const CHECKPOINT: &str = "OCIRestartCheckpoint";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: String,
    pub expected_resource_version: String,
    expected_boot_id: String,
    expected_started_at: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            uuid::Uuid::parse_str(&self.expected_boot_id)
                .is_ok_and(|id| id.to_string() == self.expected_boot_id)
                && self.expected_started_at.len() <= 40
                && DateTime::parse_from_rfc3339(&self.expected_started_at).is_ok(),
            "InvalidArgument",
            "restart requires a canonical boot ID and an observed start timestamp",
        )
    }

    fn matches(&self, sample: &Value) -> bool {
        sample["boot_id"] == self.expected_boot_id
            && sample["started_at"] == self.expected_started_at
    }
}

pub(super) use crate::linux_clock::boot_id;

pub(crate) fn observe(work: &Work) -> Result<Value> {
    let e = Engine(&work.cfg);
    let boot = boot_id()?;
    let profile = e.check_engine()?;
    require(
        work.target.body["engine_id"] == profile.id,
        "PreconditionFailed",
        "recorded application belongs to a different engine",
    )?;
    let cid = work.target.body["container_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "running container identity is missing"))?;
    let start_op = work.target.body["start_operation_id"]
        .as_str()
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "initial application start operation is missing",
            )
        })?;
    let launch = work
        .launch
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "running application release is missing"))?;
    let inspected = e.managed(work, cid, start_op)?;
    let user = format!(
        "{}:{}",
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw()
    );
    verify_isolation(work, &inspected, launch, &user)
        .map_err(|error| err("PreconditionFailed", error.0.message))?;
    let state = &inspected["State"];
    let started_at = state["StartedAt"]
        .as_str()
        .filter(|value| value.len() <= 40 && DateTime::parse_from_rfc3339(value).is_ok())
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "running container start timestamp is unavailable",
            )
        })?;
    let pid = state["Pid"]
        .as_u64()
        .filter(|pid| (1..=i32::MAX as u64).contains(pid))
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "running container process identity is unavailable",
            )
        })?;
    require(
        state["Running"] == true && state["Status"] == "running" && state["Paused"] == false,
        "PreconditionFailed",
        "container is not running an active invocation",
    )?;
    let limits = profile.observe_limits(cid, state, launch)?;
    require(
        e.identity()? == profile.id && boot_id()? == boot,
        "PreconditionFailed",
        "engine or kernel changed during instance inspection",
    )?;
    let mut observation = json!({
        "application_ref":work.target.reference(),"engine_id":profile.id,"container_id":cid,
        "start_operation_id":start_op,"boot_id":boot,"started_at":started_at,"process_id":pid,
        "process_running":true,"runtime_state":"Running",
        "effective_limits":limits,"observed_at":now()
    });
    e.observe_mount(work, &profile, cid, start_op, state, &mut observation)?;
    super::health::status(&mut observation, launch.health_check.as_ref(), state);
    Ok(observation)
}

fn changed(before: &Value, after: &Value) -> bool {
    after["process_running"] == true
        && after["started_at"].is_string()
        && after["started_at"] != before["started_at"]
        && [
            "application_ref",
            "engine_id",
            "container_id",
            "start_operation_id",
            "boot_id",
        ]
        .iter()
        .all(|field| before[*field] == after[*field])
        && matches!(
            after["effective_limits"]["source"].as_str(),
            Some("LinuxCgroupV2" | "DockerCgroupV1")
        )
}

pub(crate) fn run(
    work: &Work,
    request: &Request,
    checkpoint: impl FnOnce(&Value) -> Result<()>,
) -> Result<Value> {
    let before = observe(work)?;
    require(
        request.matches(&before),
        "PreconditionFailed",
        "container invocation changed since restart was requested",
    )?;
    checkpoint(&before)?;
    let cid = before["container_id"].as_str().unwrap();
    Engine(&work.cfg).command(&["restart", "--timeout", "5", cid].map(String::from), true)?;
    let after = observe(work).map_err(|_| {
        err(
            "OutcomeUnknown",
            "cannot verify the post-restart OCI invocation",
        )
    })?;
    require(
        changed(&before, &after),
        "OutcomeUnknown",
        "restart did not confirm a different authorized invocation",
    )?;
    Ok(after)
}

fn intent_id(op: &str) -> String {
    format!("oci-restart-intent-{op}")
}

fn checkpoint_id(op: &str) -> String {
    format!("oci-restart-{op}")
}

fn bound(doc: &Doc, kind: &str, op: &Doc) -> bool {
    doc.kind == kind
        && doc.owner == op.owner
        && doc.spec["operation_id"] == op.id
        && doc.body["plan_ref"] == op.body["plan_ref"]
        && doc.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    let (binding, approval_use) = journal::binding(db, work)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
            "application_ref":work.target.reference(),"binding":binding,"approval_use_ref":approval_use,
            "operation_ref":work.op.reference(),"effect_ref":work.effect.reference(),"run_ref":work.run.reference()}),
    )?;
    Ok(())
}

pub(crate) fn pending_mount_requirement(db: &Db<'_>, app: &Doc) -> Result<bool> {
    let operation: Option<String> =
        db.c.query_row(
            "SELECT q.operation FROM holds h JOIN queue q ON q.operation=h.holder
         JOIN current t ON t.id=q.task WHERE h.volume=?1
           AND q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type'=?2",
            params![app.id, TOOL],
            |row| row.get(0),
        )
        .optional()?;
    let Some(operation) = operation else {
        return Ok(false);
    };
    Ok(db
        .find(&checkpoint_id(&operation))?
        .is_some_and(|record| super::mount_observation::required(&record)))
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let record = journal::load(db.c, op, journal::epoch(db.c)?)?;
    record.custody.holds(db.c, op, true)?;
    Ok(record.checkpoint.is_none())
}

pub(crate) fn record(db: &Db<'_>, work: &Work, request: &Request, before: &Value) -> Result<()> {
    let record = journal::load(db.c, &work.op, journal::epoch(db.c)?)?;
    record.custody.holds(db.c, &work.op, true)?;
    require(
        record.modern
            && record.checkpoint.is_none()
            && record.custody.sample(before)
            && (!work.mount_observation_required
                || record
                    .custody
                    .volume
                    .as_ref()
                    .is_some_and(|volume| super::mount_observation::matches(volume, before)))
            && request.matches(before)
            && before["boot_id"] == boot_id()?
            && record.custody.app.reference() == work.target.reference()
            && record.effect.reference() == work.effect.reference()
            && db.of(&work.effect.id, "Effect")?.reference() == work.effect.reference()
            && db.of(&work.op.id, "Operation")?.reference() == work.op.reference()
            && record.task.reference() == work.task.reference()
            && record.run.reference() == work.run.reference(),
        "Fenced",
        "restart invocation differs from its admitted intent",
    )?;
    let mut effect = work.effect.body.clone();
    effect["phase"] = json!("EffectStarted");
    let effect = db.status(&work.effect, effect)?;
    let mut op = work.op.body.clone();
    op["effect_refs"] = json!([effect.reference()]);
    op["updated_at"] = json!(now());
    let op = db.status(&work.op, op)?;
    for subject in [&work.run, &work.task] {
        let mut body = subject.body.clone();
        body["operation_refs"] = json!([op.reference()]);
        db.status(subject, body)?;
    }
    let checkpoint = db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,"before":before,
            "volume_ref":record.custody.volume.as_ref().map(Doc::reference),"intent_ref":record.intent.reference(),
            "mount_observation_required":work.mount_observation_required,
            "started_effect_ref":effect.reference(),"started_operation_ref":op.reference()}),
    )?;
    let mut intent = record.intent.body.clone();
    intent["checkpoint_ref"] = checkpoint.reference();
    db.status(&record.intent, intent)?;
    Ok(())
}

pub(crate) fn worker_started(db: &Db<'_>, work: &Work, task: &Doc) -> Result<bool> {
    let op = db.of(&work.op.id, "Operation")?;
    let record = journal::load(db.c, &op, journal::epoch(db.c)?)?;
    record.custody.holds(db.c, &op, true)?;
    let started = record.checkpoint.is_some();
    require(
        record.modern
            && op.spec == work.op.spec
            && op.body["plan_ref"] == work.op.body["plan_ref"]
            && op.body["outcome_knowledge"] == "InFlight"
            && record.task.reference() == task.reference()
            && record.run.id == work.run.id
            && record.effect.id == work.effect.id
            && record.effect.spec == work.effect.spec
            && record.effect.rev == work.effect.rev + i64::from(started)
            && db.of(&record.effect.id, "Effect")?.reference() == record.effect.reference(),
        "Fenced",
        "OCI restart worker settlement differs from its current effect",
    )?;
    Ok(started)
}

fn confirmed(db: &Db<'_>, op: &Doc, app: &Doc, after: &Value) -> Result<Doc> {
    let op = db.of(&op.id, "Operation")?;
    let record = journal::load(db.c, &op, journal::epoch(db.c)?)?;
    record.custody.holds(db.c, &op, true)?;
    let checkpoint = record
        .checkpoint
        .ok_or_else(|| err("RecoveryRequired", "no committed OCI restart checkpoint"))?;
    require(
        record.custody.app.reference() == app.reference()
            && record.custody.running(after)
            && changed(&checkpoint.body["before"], after)
            && (!(super::mount_observation::required(&checkpoint)
                || db.cfg.oci_effective_volume_observation && record.custody.volume.is_some())
                || record
                    .custody
                    .volume
                    .as_ref()
                    .is_some_and(|volume| super::mount_observation::matches(volume, after)))
            && (!record.modern || after["boot_id"] == boot_id()?),
        "OutcomeUnknown",
        "inspection does not confirm restart of the checkpointed invocation",
    )?;
    Ok(checkpoint)
}

pub(crate) fn settle(db: &Db<'_>, op: &Doc, app: &Doc, after: &Value) -> Result<Value> {
    let checkpoint = confirmed(db, op, app, after)?;
    apply_confirmed(db, op, app, &checkpoint, after, false)
}

fn apply_confirmed(
    db: &Db<'_>,
    op: &Doc,
    app: &Doc,
    checkpoint: &Doc,
    after: &Value,
    recovered: bool,
) -> Result<Value> {
    let instance = db.resolve(&app.body["instance_ref"])?;
    require(
        instance.kind == "RuntimeInstance"
            && instance.spec["application_id"] == app.id
            && instance.spec["container_id"] == after["container_id"]
            && instance.spec["engine_id"] == after["engine_id"]
            && instance.spec["start_operation_id"] == after["start_operation_id"],
        "RecoveryRequired",
        "runtime instance record differs from the observed container",
    )?;
    let mut state = instance.body.clone();
    state["lifecycle"] = json!("Running");
    state["observation"] = after.clone();
    state["observed_at"] = after["observed_at"].clone();
    state["last_restart_operation_id"] = json!(op.id);
    let instance = db.status(&instance, state)?;
    let mut state = app.body.clone();
    state["instance_ref"] = instance.reference();
    state["last_observation"] = after.clone();
    for field in ["runtime_state", "observed_at"] {
        state[field] = after[field].clone();
    }
    let (health, readiness) = super::health::projection(after, recovered);
    state["health"] = json!(health);
    state["readiness"] = json!(readiness);
    state["last_restart_checkpoint_ref"] = checkpoint.reference();
    Ok(db.status(app, state)?.reference())
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    app: &Doc,
    observation: &Doc,
    after: &Value,
) -> Result<(Value, Value)> {
    let checkpoint = confirmed(db, op, app, after)?;
    Ok((
        apply_confirmed(db, op, app, &checkpoint, after, true)?,
        json!({
            "observation_ref":observation.reference(),"checkpoint_ref":checkpoint.reference(),
            "conclusion":"ObservedContainerRestart"
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_observation_is_bound_to_the_original_boot_and_container() {
        let first = json!({
            "application_ref":{"resource_id":"app","version":"r3","digest":"original"},
            "engine_id":"engine","container_id":"container","start_operation_id":"start",
            "boot_id":"boot","started_at":"2026-11-25T03:00:00Z","process_running":true,
            "effective_limits":{"source":"LinuxCgroupV2"}
        });
        let mut next = first.clone();
        assert!(!changed(&first, &next));
        next["started_at"] = json!("2026-11-25T03:01:00Z");
        assert!(changed(&first, &next));
        for field in [
            "boot_id",
            "container_id",
            "engine_id",
            "start_operation_id",
            "application_ref",
        ] {
            let mut unrelated = next.clone();
            unrelated[field] = json!("replacement");
            assert!(!changed(&first, &unrelated), "{field}");
        }
        next["effective_limits"] = Value::Null;
        assert!(!changed(&first, &next));
    }
}
