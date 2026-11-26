//! Rebind an idle OCI application to an independently verified restore clone.

use super::{Engine, check_release_selection};
use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::{Action, Target, Work},
    policy, require, storage,
};
use psios_api::{ResourceRef, id};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "application.rebind_restored_volume";
pub(crate) const FORMAT: u32 = crate::workflow::admission_delay::FORMAT + 1;
pub(crate) mod verify;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub target: Target,
    source_volume_ref: ResourceRef,
    restored_volume_ref: ResourceRef,
}

pub(crate) struct Prepared {
    pub source: Doc,
    pub restored: Doc,
    deployment: Doc,
    backup: Doc,
    activation: Doc,
    previous_runtime: Option<(String, String)>,
}

fn exact_volume(db: &Db<'_>, actor: &str, reference: &ResourceRef) -> Result<Doc> {
    let volume = db.of(&reference.resource_id, "Volume")?;
    policy::owned(actor, &volume)?;
    require(
        volume.reference() == json!(reference),
        "Conflict",
        "rebind volume reference changed",
    )?;
    Ok(volume)
}

impl Input {
    pub(crate) fn volumes(&self, db: &Db<'_>, actor: &str) -> Result<(Doc, Doc)> {
        require(
            self.source_volume_ref.resource_id != self.restored_volume_ref.resource_id,
            "InvalidArgument",
            "restored and source volume must be distinct",
        )?;
        Ok((
            exact_volume(db, actor, &self.source_volume_ref)?,
            exact_volume(db, actor, &self.restored_volume_ref)?,
        ))
    }

    pub(crate) fn prepare(&self, db: &Db<'_>, actor: &str, app: &Doc) -> Result<Prepared> {
        let (source, restored) = self.volumes(db, actor)?;
        require(
            app.body["lifecycle"] == "Stopped"
                && app.body["desired_state"] == "Stopped"
                && app.body["runtime_state"] == "Absent"
                && app.body["instance_ref"].is_null()
                && app.body["container_id"].is_null(),
            "PreconditionFailed",
            "volume rebind requires a stopped application without a runtime instance",
        )?;
        require(
            app.spec["volume_id"] == source.id
                && app.spec["data_incarnation"] == source.body["data_incarnation"],
            "PreconditionFailed",
            "source volume is not the current application binding",
        )?;
        let unattached = |volume: &Doc| {
            volume.body["lifecycle"] == "Available"
                && volume.body["attachment_refs"]
                    .as_array()
                    .is_some_and(Vec::is_empty)
        };
        require(
            unattached(&source) && unattached(&restored),
            "Conflict",
            "volume rebind requires both volumes to be unattached",
        )?;
        let referenced: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM current WHERE kind='Application'
                   AND spec->>'volume_id'=?1 AND body->>'lifecycle'!='Uninstalled')",
            [&restored.id],
            |row| row.get(0),
        )?;
        require(
            !referenced,
            "Conflict",
            "restored volume is already bound to an application",
        )?;

        let deployment = db.resolve(&app.spec["deployment_ref"])?;
        require(
            deployment.kind == "Deployment"
                && deployment.owner == app.owner
                && deployment.spec["application_id"] == app.id
                && deployment.spec["volume_id"] == source.id
                && deployment.spec["data_incarnation"] == source.body["data_incarnation"]
                && deployment.spec["release_ref"] == app.spec["release_ref"]
                && deployment.body["desired_state"] == "Stopped"
                && matches!(
                    deployment.body["lifecycle"].as_str(),
                    Some("Prepared" | "Active")
                ),
            "RecoveryRequired",
            "stopped deployment disagrees with the application data binding",
        )?;
        check_release_selection(db, app, &app.spec["release_ref"])?;
        let activation = activation(db.c, &restored)?;
        let backup = lineage(db.c, &source, &restored)?;
        let previous_runtime = previous_runtime(db, app)?;
        Ok(Prepared {
            source,
            restored,
            deployment,
            backup,
            activation,
            previous_runtime,
        })
    }
}

fn lineage(conn: &Connection, source: &Doc, restored: &Doc) -> Result<Doc> {
    let backup = crate::db::resolve(conn, &restored.spec["restored_from"])?;
    let captured = crate::db::resolve(conn, &backup.spec["source_ref"])?;
    require(
        backup.kind == "Backup"
            && backup.body["lifecycle"] == "Stored"
            && backup.spec["source_volume"] == source.id
            && captured.kind == "Volume"
            && captured.id == source.id
            && captured.body["data_incarnation"] == source.body["data_incarnation"]
            && restored.body["data_incarnation"] != source.body["data_incarnation"],
        "PreconditionFailed",
        "restored volume is not a verified clone of the bound volume incarnation",
    )?;
    Ok(backup)
}

fn activation(conn: &Connection, volume: &Doc) -> Result<Doc> {
    require(
        volume.spec["storage_restore_phase"] == "Activated"
            && volume.spec["restore_verification_ref"].is_object()
            && volume.spec["restore_proof"]["tree_digest"].is_string(),
        "PreconditionFailed",
        "volume rebind requires an independently activated, verified restore clone",
    )?;
    let evidence = crate::db::resolve(conn, &volume.spec["restore_verification_ref"])?;
    let pending = crate::db::resolve(conn, &evidence.body["volume_ref"])?;
    let mut expected = pending.spec.clone();
    expected["storage_restore_phase"] = json!("Activated");
    expected["restore_verification_ref"] = evidence.reference();
    require(
        pending.rev + 1 == volume.rev && expected == volume.spec,
        "PreconditionFailed",
        "volume rebind requires the pristine activation revision",
    )?;
    let issued = crate::db::resolve(conn, &evidence.body["operation_ref"])?;
    let operation = crate::db::find(conn, &issued.id)?
        .filter(|record| record.kind == "Operation")
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "restored volume has no activation operation",
            )
        })?;
    let plan = crate::db::resolve(conn, &issued.body["plan_ref"])?;
    require(
        evidence.kind == "RecoveryEvidence"
            && evidence.body["verified_at"].is_string()
            && evidence.body["backup_ref"] == pending.spec["restored_from"]
            && evidence.body["tree_digest"] == pending.spec["restore_proof"]["tree_digest"]
            && pending.kind == "Volume"
            && pending.id == volume.id
            && pending.body["data_incarnation"] == volume.body["data_incarnation"]
            && pending.spec["storage_restore_phase"] == "AwaitingActivation"
            && issued.kind == "Operation"
            && issued.body["outcome_knowledge"] == "InFlight"
            && issued.body["plan_ref"] == operation.body["plan_ref"]
            && operation.body["outcome_knowledge"] == "Known"
            && operation.body["lifecycle"] == "Succeeded"
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == "volume.activate"
            && plan.body["allowed_effects"][0]["resource_ref"] == pending.reference(),
        "RecoveryRequired",
        "restored volume activation provenance is inconsistent",
    )?;
    Ok(evidence)
}

fn previous_runtime(db: &Db<'_>, app: &Doc) -> Result<Option<(String, String)>> {
    let Some(stop) = app.body["last_stop_operation_id"].as_str() else {
        return Ok(None);
    };
    let operation = db.of(stop, "Operation")?;
    let plan = db.resolve(&operation.body["plan_ref"])?;
    let previous = db.resolve(&plan.body["allowed_effects"][0]["resource_ref"])?;
    require(
        operation.body["outcome_knowledge"] == "Known"
            && plan.body["allowed_effects"][0]["action"] == "application.stop"
            && previous.kind == "Application"
            && previous.id == app.id
            && previous.body["lifecycle"] == "Running",
        "RecoveryRequired",
        "application stop does not establish the preceding runtime identity",
    )?;
    let text = |key| {
        previous.body[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "previous application runtime identity is unavailable",
                )
            })
    };
    Ok(Some((text("engine_id")?, text("container_id")?)))
}

fn prepared(work: &Work) -> Result<&Prepared> {
    work.volume_rebind
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "missing prepared restoration binding"))
}

pub(crate) fn observe(work: &Work, check: &mut impl FnMut() -> Result<()>) -> Result<Value> {
    let prepared = prepared(work)?;
    check()?;
    let fingerprint = storage::verify_restored_for_rebind(&work.cfg, &prepared.restored, check)?;
    let engine = Engine(&work.cfg);
    let identity = engine.identity()?;
    let cid = prepared
        .previous_runtime
        .as_ref()
        .map(|(id, cid)| (id, cid.as_str()));
    require(
        cid.is_none_or(|(id, _)| id == &identity),
        "PreconditionFailed",
        "OCI engine no longer matches the last stopped runtime",
    )?;
    require(
        engine.application_absent(&work.target.id, cid.map(|(_, cid)| cid), &identity)?,
        "PreconditionFailed",
        "OCI engine still has a container for this application",
    )?;
    check()?;
    Ok(json!({"engine_id":identity,"volume_fingerprint":fingerprint}))
}

pub(crate) fn validate_observation(db: &Db<'_>, work: &Work, observed: &Value) -> Result<()> {
    let prepared = prepared(work)?;
    let Action::RebindRestoredVolume(input) = &work.action else {
        return Err(err(
            "RecoveryRequired",
            "wrong task for restoration binding",
        ));
    };
    input.prepare(db, &work.task.owner, &work.target)?;
    let held: i64 = db.c.query_row(
        "SELECT count(*) FROM holds WHERE holder=?1 AND volume IN (?2,?3,?4)",
        params![
            work.op.id,
            work.target.id,
            prepared.source.id,
            prepared.restored.id
        ],
        |row| row.get(0),
    )?;
    require(
        held == 3,
        "RecoveryRequired",
        "volume rebind lost exclusive custody",
    )?;
    require(
        observed["engine_id"]
            .as_str()
            .is_some_and(|id| (16..=128).contains(&id.len()))
            && observed["volume_fingerprint"]
                == storage::restored_fingerprint(db.cfg, &prepared.restored)?,
        "Conflict",
        "restored volume changed after verification",
    )
}

pub(crate) fn settle(db: &Db<'_>, work: &Work, observed: &Value) -> Result<Value> {
    let prepared = prepared(work)?;
    db.status(
        &prepared.deployment,
        json!({"lifecycle":"Superseded","desired_state":"Stopped"}),
    )?;
    let mut config = prepared.deployment.spec.clone();
    config["volume_id"] = json!(prepared.restored.id);
    config["data_incarnation"] = prepared.restored.body["data_incarnation"].clone();
    config["restore_rebind"] = json!({
        "operation_ref":work.op.reference(),"previous_application_ref":work.target.reference(),
        "previous_deployment_ref":prepared.deployment.reference(),
        "source_volume_ref":prepared.source.reference(),"restored_volume_ref":prepared.restored.reference(),
        "backup_ref":prepared.backup.reference(),"activation_ref":prepared.activation.reference(),
        "observation": observed,
    });
    let deployment = db.create(
        "Deployment",
        &id("deployment"),
        &work.target.owner,
        config,
        json!({"lifecycle":"Prepared","desired_state":"Stopped"}),
    )?;
    let mut spec = work.target.spec.clone();
    spec["deployment_ref"] = deployment.reference();
    spec["volume_id"] = deployment.spec["volume_id"].clone();
    spec["data_incarnation"] = deployment.spec["data_incarnation"].clone();
    let app = db.update(&work.target, spec, work.target.body.clone())?;
    Ok(
        json!({"application_ref":app.reference(),"deployment_ref":deployment.reference(),
              "previous_deployment_ref":prepared.deployment.reference(),"source_volume_ref":prepared.source.reference(),
              "restored_volume_ref":prepared.restored.reference()}),
    )
}
