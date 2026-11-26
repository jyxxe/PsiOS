//! Recover the exact never-started container of a mutating Docker CREATE with no receipt.
mod disposal;
mod journal;

use super::{
    launch_from_history, restart::boot_id, restore_image::engine, verify_static_isolation,
};
use crate::{
    Config, Result,
    db::{self, Db, Doc},
    err,
    host::{Checkpoint, Install, Target, Work},
    require, storage,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

pub(crate) use disposal::{
    admissible, admit, begin_cleanup, cleanup_held, close, reconcile_cleanup, reconcile_original,
    record_cleanup, release_original, retains_result, settle_cleanup,
};
pub(crate) use disposal::{not_dispatched as cleanup_not_dispatched, prepare};
pub(crate) use journal::{acknowledge, allow_start, begin, issue};

pub(crate) const FORMAT: u32 = crate::node::service_resources::FORMAT + 1;
pub(crate) const CREATE: &str = "application.start";
pub(crate) const INSPECT: &str = "application.inspect_start_create";
pub(crate) const CLEANUP: &str = "application.discard_start_create";
pub(crate) const INTENT: &str = "OCIStartCreateIntent";
pub(crate) const DISPOSAL: &str = "OCIStartCreateDisposal";
const EPOCH: &str = "oci_start_create_epoch";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Target,
    pub operation_ref: Value,
    pub evidence_ref: Value,
}

pub(super) fn new_attempt() -> Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| err("InternalError", "OCI create entropy unavailable"))?;
    Ok(psios_api::sha256(&bytes)[7..].to_owned())
}

pub(crate) fn cid_ok(cid: &str) -> bool {
    cid.len() == 64
        && cid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    let boundary: i64 =
        db.c.query_row("SELECT coalesce(max(seq),0) FROM versions", [], |row| {
            row.get(0)
        })?;
    require(
        boundary > 0,
        "RecoveryRequired",
        "OCI create intent migration requires a bootstrapped authority",
    )?;
    db.c.execute(
        "INSERT INTO meta VALUES(?1,?2)",
        params![EPOCH, boundary.to_string()],
    )?;
    db.c.execute_batch(disposal::SCHEMA)?;
    Ok(())
}

fn validate_binding(binding: &Value, app: &Doc, volume: Option<&Doc>) -> bool {
    app.kind == "Application"
        && app.body["lifecycle"] == "Stopped"
        && binding["application_ref"] == app.reference()
        && binding["deployment_ref"] == app.spec["deployment_ref"]
        && binding["release_ref"] == app.spec["release_ref"]
        && binding["volume_ref"] == json!(volume.map(Doc::reference))
        && binding["user"].as_str().is_some_and(|user| {
            user.split_once(':').is_some_and(|(uid, gid)| {
                uid.parse::<u32>()
                    .is_ok_and(|value| value != 0 && value.to_string() == uid)
                    && gid
                        .parse::<u32>()
                        .is_ok_and(|value| value.to_string() == gid)
            })
        })
        && volume.map_or_else(
            || {
                app.spec["volume_id"].is_null()
                    && ["volume_data_incarnation", "volume_device", "volume_inode"]
                        .iter()
                        .all(|name| binding[*name].is_null())
            },
            |volume| {
                volume.kind == "Volume"
                    && volume.body["lifecycle"] == "Available"
                    && app.spec["volume_id"] == volume.id
                    && app.spec["data_incarnation"] == volume.body["data_incarnation"]
                    && binding["volume_data_incarnation"] == volume.body["data_incarnation"]
                    && binding["volume_device"] == volume.spec["device"]
                    && binding["volume_inode"] == volume.spec["inode"]
            },
        )
}

fn held(conn: &Connection, op: &Doc, app: &Doc, volume: Option<&Doc>) -> Result<()> {
    for resource in std::iter::once(app).chain(volume) {
        let actual = db::find(conn, &resource.id)?;
        let holder: Option<String> = conn
            .query_row(
                "SELECT holder FROM holds WHERE volume=?1",
                [&resource.id],
                |row| row.get(0),
            )
            .optional()?;
        require(
            actual.is_some_and(|doc| doc.reference() == resource.reference())
                && holder.as_deref() == Some(&op.id),
            "RecoveryRequired",
            "original OCI create target or exclusive hold is missing",
        )?;
    }
    Ok(())
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(journal::load(db.c, op)?.is_some_and(|journal| journal.issue.is_none()))
}

pub(crate) fn eligible_original(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(journal::load(db.c, op)?.is_some_and(|journal| {
        journal.intent.body["phase"] == "CreateIssued" && journal.start.is_none()
    }))
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    journal::read_all(conn, true)?;
    disposal::verify_active(conn)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    journal::read_all(conn, false)?;
    disposal::verify(conn)
}

#[derive(Clone)]
pub(crate) struct Inspection {
    original: Doc,
    app: Doc,
    volume: Option<Doc>,
    issue: Doc,
    launch: Install,
    path: Option<PathBuf>,
    removal: Option<Doc>,
}

impl Inspection {
    fn engine(&self) -> &str {
        self.issue.body["engine_id"].as_str().unwrap()
    }
    fn attempt(&self) -> &str {
        self.issue.body["attempt"].as_str().unwrap()
    }
    fn boot(&self) -> &str {
        self.issue.body["boot_id"].as_str().unwrap()
    }
    fn confirm_profile(&self, config: &Config) -> Result<()> {
        require(
            boot_id()? == self.boot()
                && self.issue.body["user"]
                    == format!(
                        "{}:{}",
                        rustix::process::geteuid().as_raw(),
                        rustix::process::getegid().as_raw()
                    )
                && self
                    .volume
                    .as_ref()
                    .map(|volume| storage::volume_dir(config, volume))
                    .transpose()?
                    == self.path,
            "PreconditionFailed",
            "OCI create kernel boot, execution identity or original volume changed",
        )
    }
    fn owned_sample(&self, cid: &str, result: &Value) -> Value {
        json!({
            "status":"CreatedNeverStarted","operation_ref":self.original.reference(),
            "issue_ref":self.issue.reference(),"application_ref":self.app.reference(),
            "volume_ref":self.volume.as_ref().map(Doc::reference),
            "container_id":cid,"engine_id":self.engine(),"boot_id":self.boot(),
            "isolation_digest":psios_api::sha256(&psios_api::canonical(&json!([
                result["HostConfig"],result["Mounts"],result["Config"]["User"],result["Config"]["Healthcheck"]
            ]))),
        })
    }
    async fn matching(
        &self,
        connection: &mut engine::Connection,
        cid: Option<&str>,
    ) -> Result<Vec<String>> {
        connection.require_linux_engine(self.engine()).await?;
        let name = format!("^/psios-{}$", self.app.id);
        let mut filters = vec![
            ("name", name),
            ("label", format!("io.psios.application={}", self.app.id)),
            ("label", format!("io.psios.operation={}", self.original.id)),
            (
                "label",
                format!("io.psios.create-attempt={}", self.attempt()),
            ),
        ];
        if let Some(cid) = cid {
            filters.push(("id", cid.to_owned()));
        }
        let expected = connection
            .matching_containers(filters[0].0, &filters[0].1)
            .await?;
        for (key, value) in &filters[1..] {
            let rows = connection.matching_containers(key, value).await?;
            require(
                expected == rows && expected.len() <= 1,
                "OutcomeUnknown",
                "Docker names and original attempt selectors disagree",
            )?;
        }
        connection.require_linux_engine(self.engine()).await?;
        Ok(expected)
    }
    async fn observe(&self, config: &Config, connection: &mut engine::Connection) -> Result<Value> {
        self.confirm_profile(config)?;
        let known = self
            .removal
            .as_ref()
            .and_then(|removal| removal.body["candidate"]["container_id"].as_str());
        let ids = self.matching(connection, known).await?;
        let Some(cid) = ids.first() else {
            return Ok(match &self.removal {
                Some(removal) => json!({
                    "status":"RemovalConfirmed","operation_ref":self.original.reference(),
                    "issue_ref":self.issue.reference(),"removal_ref":removal.reference(),
                    "container_id":known.unwrap(),"engine_id":self.engine(),"boot_id":self.boot(),
                }),
                None => {
                    json!({"status":"UnverifiedAbsence","operation_ref":self.original.reference(),
                    "issue_ref":self.issue.reference()})
                }
            });
        };
        let actual = connection.inspect_container(cid).await?;
        let labels = &actual["Config"]["Labels"];
        let state = &actual["State"];
        require(
            actual["Id"] == *cid
                && actual["Name"] == format!("/psios-{}", self.app.id)
                && actual["Image"] == self.launch.image_id
                && labels["io.psios.application"] == self.app.id
                && labels["io.psios.operation"] == self.original.id
                && labels["io.psios.create-attempt"] == self.attempt()
                && state["Status"] == "created"
                && state["Running"] == false
                && state["Paused"] == false
                && state["Restarting"] == false
                && state["Dead"] == false
                && state["Pid"] == 0
                && state["StartedAt"] == "0001-01-01T00:00:00Z"
                && actual["RestartCount"] == 0,
            "OutcomeUnknown",
            "Docker does not show the original never-started create attempt",
        )?;
        verify_static_isolation(
            self.path.as_deref(),
            &actual,
            &self.launch,
            self.issue.body["user"].as_str().unwrap(),
        )?;
        require(
            self.matching(connection, Some(cid)).await? == [cid.clone()],
            "OutcomeUnknown",
            "exact OCI create identity changed during inspection",
        )?;
        self.confirm_profile(config)?;
        Ok(self.owned_sample(cid, &actual))
    }
}

fn recoverable(db: &Db<'_>, original: &Doc) -> Result<journal::Journal> {
    require(
        original.kind == "Operation"
            && original.body["outcome_knowledge"] == "Unknown"
            && db.get(&original.id)?.reference() == original.reference(),
        "PreconditionFailed",
        "original OCI create is not currently unresolved",
    )?;
    let journal = journal::required(db.c, original)?;
    require(
        journal.intent.body["phase"] == "CreateIssued" && journal.start.is_none(),
        "PreconditionFailed",
        "original OCI create is outside the lost-acknowledgement boundary",
    )?;
    let task = original.spec["task_id"].as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "original OCI create has no task identity",
        )
    })?;
    let scheduled: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue WHERE task=?1 AND operation=?2 AND phase='Unknown')",
        params![task, original.id],
        |row| row.get(0),
    )?;
    require(
        scheduled,
        "RecoveryRequired",
        "original OCI create has no unresolved queue custody",
    )?;
    held(db.c, original, &journal.app, journal.volume.as_ref())?;
    Ok(journal)
}

pub(crate) fn inspection(db: &Db<'_>, original: &Doc) -> Result<Inspection> {
    let journal = recoverable(db, original)?;
    let path = journal
        .volume
        .as_ref()
        .map(|volume| storage::volume_dir(db.cfg, volume))
        .transpose()?;
    let launch = launch_from_history(db.c, &journal.app, |_, _| Ok(()))?;
    Ok(Inspection {
        original: original.clone(),
        app: journal.app,
        volume: journal.volume,
        issue: journal.issue.unwrap(),
        launch,
        path,
        removal: disposal::removal(db.c, &original.id)?,
    })
}

pub(crate) fn original(db: &Db<'_>, request: &Request, app: &Doc) -> Result<Doc> {
    let original = db.resolve(&request.operation_ref)?;
    require(
        recoverable(db, &original)?.app.reference() == app.reference(),
        "Conflict",
        "original OCI create belongs to a different application",
    )?;
    Ok(original)
}

pub(crate) fn audience(db: &Db<'_>, op: &Doc) -> Result<Vec<Value>> {
    let journal = recoverable(db, op)?;
    Ok(std::iter::once(journal.app.reference())
        .chain(journal.volume.as_ref().map(Doc::reference))
        .collect())
}

pub(crate) fn inspect(w: &Work) -> Result<Value> {
    let inspection = w
        .create_inspection
        .as_ref()
        .ok_or_else(|| err("Fenced", "OCI create inspection is unavailable"))?;
    engine::runtime()?.block_on(async {
        let mut connection = engine::Connection::open(&w.cfg.docker_socket).await?;
        inspection.observe(&w.cfg, &mut connection).await
    })
}

pub(crate) fn cleanup(
    w: &Work,
    mut checkpoint: impl FnMut(Checkpoint<'_>) -> Result<()>,
) -> Result<Value> {
    let prepared = w
        .create_cleanup
        .as_ref()
        .ok_or_else(|| err("Fenced", "OCI create cleanup is unavailable"))?;
    let inspection = &prepared.inspection;
    engine::runtime()?.block_on(async {
        let mut connection = engine::Connection::open(&w.cfg.docker_socket).await?;
        let sample = inspection.observe(&w.cfg, &mut connection).await?;
        require(
            sample == prepared.candidate,
            "OutcomeUnknown",
            "fresh unstarted candidate differs from the approved inspection",
        )?;
        let cid = sample["container_id"].as_str().unwrap();
        checkpoint(Checkpoint::OciCreateRemoval(&sample))?;
        connection.remove_unforced_container(cid).await?;
        require(
            inspection
                .matching(&mut connection, Some(cid))
                .await?
                .is_empty(),
            "OutcomeUnknown",
            "cannot confirm removal of exact OCI create attempt",
        )?;
        Ok(disposal::cleanup_receipt(&sample))
    })
}

pub(crate) use disposal::Prepared;
