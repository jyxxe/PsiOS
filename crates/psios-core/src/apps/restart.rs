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
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "application.restart";
pub(crate) const INSPECT: &str = "application.inspect_instance";
pub(crate) const FORMAT: u32 = crate::storage::restore_publication::FORMAT + 1;

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

fn boot_id() -> Result<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot = boot.trim();
    require(
        uuid::Uuid::parse_str(boot).is_ok_and(|id| id.to_string() == boot),
        "DependencyUnavailable",
        "Linux boot identity unavailable",
    )?;
    Ok(boot.into())
}

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
    Ok(json!({
        "application_ref":work.target.reference(),"engine_id":profile.id,"container_id":cid,
        "start_operation_id":start_op,"boot_id":boot,"started_at":started_at,"process_id":pid,
        "process_running":true,"runtime_state":"Running","readiness":"Unknown","health":"Unknown",
        "effective_limits":limits,"observed_at":now()
    }))
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

fn held(db: &Db<'_>, app: &Doc, op: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![app.id, op.id],
        |row| row.get(0),
    )?;
    require(
        held && app.kind == "Application",
        "RecoveryRequired",
        "original application restart hold is missing",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.target, &work.op)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
            "application_ref":work.target.reference()}),
    )?;
    Ok(())
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    let plan = db.resolve(&op.body["plan_ref"])?;
    Ok(bound(&intent, INTENT, op)
        && intent.body["application_ref"] == plan.body["allowed_effects"][0]["resource_ref"]
        && db.find(&checkpoint_id(&op.id))?.is_none())
}

pub(crate) fn record(db: &Db<'_>, work: &Work, request: &Request, before: &Value) -> Result<()> {
    held(db, &work.target, &work.op)?;
    require(
        not_dispatched(db, &work.op)?
            && request.matches(before)
            && before["application_ref"] == work.target.reference()
            && before["engine_id"] == work.target.body["engine_id"]
            && before["container_id"] == work.target.body["container_id"],
        "Fenced",
        "restart invocation differs from its admitted intent",
    )?;
    db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,"before":before,
            "volume_ref":work.volume.as_ref().map(Doc::reference)}),
    )?;
    Ok(())
}

fn confirmed(db: &Db<'_>, op: &Doc, app: &Doc, after: &Value) -> Result<Doc> {
    held(db, app, op)?;
    let checkpoint = db
        .find(&checkpoint_id(&op.id))?
        .ok_or_else(|| err("RecoveryRequired", "no committed OCI restart checkpoint"))?;
    let plan = db.resolve(&op.body["plan_ref"])?;
    let volume = app.spec["volume_id"]
        .as_str()
        .map(|id| db.of(id, "Volume"))
        .transpose()?;
    require(
        bound(&checkpoint, CHECKPOINT, op)
            && checkpoint.body["before"]["application_ref"] == app.reference()
            && checkpoint.body["volume_ref"] == json!(volume.as_ref().map(Doc::reference))
            && volume.as_ref().is_none_or(|v| {
                plan.body["preconditions"]
                    .as_array()
                    .is_some_and(|conditions| {
                        conditions
                            .iter()
                            .any(|c| c["resource_ref"] == v.reference())
                    })
            })
            && changed(&checkpoint.body["before"], after),
        "OutcomeUnknown",
        "inspection does not confirm restart of the checkpointed invocation",
    )?;
    Ok(checkpoint)
}

pub(crate) fn settle(db: &Db<'_>, op: &Doc, app: &Doc, after: &Value) -> Result<Value> {
    let checkpoint = confirmed(db, op, app, after)?;
    apply_confirmed(db, op, app, &checkpoint, after)
}

fn apply_confirmed(
    db: &Db<'_>,
    op: &Doc,
    app: &Doc,
    checkpoint: &Doc,
    after: &Value,
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
    for field in ["runtime_state", "readiness", "health", "observed_at"] {
        state[field] = after[field].clone();
    }
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
        apply_confirmed(db, op, app, &checkpoint, after)?,
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
