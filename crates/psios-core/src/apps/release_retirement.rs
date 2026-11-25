//! Retire an immutable release while preserving its audit and installation history.
use crate::{
    Result,
    db::{Db, Doc, find, resolve, row},
    host::Target,
    now, require,
};
use psios_api::ResourceRef;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = super::artifact_install::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("release_retirement.sql");
pub(crate) const TOOL: &str = "application.retire_release";
const KIND: &str = "ApplicationReleaseRetirement";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub target: Target,
    pub release_ref: ResourceRef,
    reason: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            !self.reason.trim().is_empty() && self.reason.len() <= 256,
            "InvalidArgument",
            "a bounded reason for release retirement is required",
        )
    }
}

fn receipt_id(release: &str) -> String {
    format!("retired-{release}")
}

pub(crate) fn associated(db: &Db<'_>, app: &Doc, reference: &Value) -> Result<Doc> {
    let release = db.resolve(reference)?;
    require(
        release.kind == "ApplicationRelease"
            && release.owner == app.owner
            && release.body["application_id"] == app.id,
        "PreconditionFailed",
        "release is outside the application's immutable releases",
    )?;
    Ok(release)
}

fn check_receipt(retired: &Doc, release: &Doc) -> Result<()> {
    require(
        retired.kind == KIND
            && retired.rev == 1
            && retired.id == receipt_id(&release.id)
            && release.kind == "ApplicationRelease"
            && retired.owner == release.owner
            && retired.spec["release_id"] == release.id
            && retired.spec["application_id"] == release.body["application_id"]
            && retired.body["release_ref"] == release.reference(),
        "RecoveryRequired",
        "release retirement receipt disagrees with the original release",
    )
}

pub(crate) fn available(db: &Db<'_>, release: &Doc) -> Result<()> {
    if let Some(retired) = db.find(&receipt_id(&release.id))? {
        check_receipt(&retired, release)?;
        return Err(crate::err(
            "PreconditionFailed",
            "application release has been retired",
        ));
    }
    Ok(())
}

fn in_use(conn: &Connection, app: &str, release: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current WHERE id=?1 AND kind='Application'
               AND spec->'release_ref'->>'resource_id'=?2
               AND (body->>'lifecycle' IS NOT 'Uninstalled' OR body->>'runtime_state' IS NOT 'Absent'))
           OR EXISTS(SELECT 1 FROM current WHERE kind='Deployment'
               AND spec->>'application_id'=?1 AND spec->'release_ref'->>'resource_id'=?2
               AND coalesce(body->>'lifecycle','') NOT IN ('Superseded','Removed'))",
        [app, release], |r| r.get(0),
    )?)
}

pub(crate) fn candidate(db: &Db<'_>, app: &Doc, input: &Request) -> Result<Doc> {
    let release = associated(db, app, &json!(input.release_ref))?;
    available(db, &release)?;
    require(
        !in_use(db.c, &app.id, &release.id)?,
        "Conflict",
        "an application or deployment still selects this release",
    )?;
    Ok(release)
}

pub(super) fn retire(db: &Db<'_>, work: &crate::host::Work, input: &Request) -> Result<Value> {
    let release = candidate(db, &work.target, input)?;
    let source = super::artifact_install::drop_release_pin(db, &release)?;
    let retirement = db.create(
        KIND,
        &receipt_id(&release.id),
        &release.owner,
        json!({"application_id":work.target.id,"release_id":release.id}),
        json!({"application_ref":work.target.reference(),"release_ref":release.reference(),
               "operation_id":work.op.id,"retired_at":now(),"reason":input.reason,
               "released_artifact_ref":source}),
    )?;
    db.audit(
        &work.task.owner,
        "Application.RetireRelease",
        Some(&release.id),
        "Retired",
    )?;
    Ok(retirement.reference())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut all = conn.prepare("SELECT * FROM versions WHERE kind=?1")?;
    for receipt in all.query_map([KIND], row)? {
        let receipt = receipt?;
        let id = receipt.spec["release_id"].as_str().ok_or_else(|| {
            crate::err("RecoveryRequired", "retirement receipt lost its release ID")
        })?;
        let release = find(conn, id)?.ok_or_else(|| {
            crate::err(
                "RecoveryRequired",
                "retirement receipt lost its original release",
            )
        })?;
        check_receipt(&receipt, &release)?;
        let app = resolve(conn, &receipt.body["application_ref"])
            .map_err(|_| crate::err("RecoveryRequired", "retirement lost its exact application"))?;
        let operation = receipt.body["operation_id"]
            .as_str()
            .ok_or_else(|| crate::err("RecoveryRequired", "retirement lost its operation ID"))?;
        let op = find(conn, operation)?
            .ok_or_else(|| crate::err("RecoveryRequired", "retirement lost its operation"))?;
        let plan = resolve(conn, &op.body["plan_ref"])
            .map_err(|_| crate::err("RecoveryRequired", "retirement lost its operation plan"))?;
        let source = super::artifact_install::release_source(conn, &release)?;
        require(
            app.kind == "Application"
                && app.owner == release.owner
                && receipt.spec["application_id"] == app.id
                && op.kind == "Operation"
                && op.owner == release.owner
                && op.body["lifecycle"] == "Succeeded"
                && op.body["outcome_knowledge"] == "Known"
                && plan.kind == "OperationPlan"
                && plan.spec["action"] == TOOL
                && plan.body["allowed_effects"][0]["resource_ref"] == app.reference()
                && plan.body["preconditions"][1]["resource_ref"] == release.reference()
                && receipt.body["released_artifact_ref"] == json!(source),
            "RecoveryRequired",
            "retirement receipt disagrees with its plan or source",
        )?;
        require(
            !in_use(conn, &app.id, id)?,
            "RecoveryRequired",
            "a retired release remains selected or deployed",
        )?;
    }
    Ok(())
}
