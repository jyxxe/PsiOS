//! Typed effect boundary. All filesystem roots, runtime flags and identity come from the authority.
use crate::{
    apps,
    config::Config,
    db::{Db, Doc},
    policy, storage, *,
};
use psios_api::VERSION;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    pub expected_resource_version: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceRestart {
    pub id: String,
    pub expected_resource_version: String,
    pub expected_boot_id: String,
    pub expected_invocation_id: String,
}

impl ServiceRestart {
    fn validate(&self) -> Result<()> {
        require(
            uuid::Uuid::parse_str(&self.expected_boot_id)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == self.expected_boot_id)
                && node::valid_invocation(&self.expected_invocation_id),
            "InvalidArgument",
            "restart requires a Linux boot identity and a systemd invocation identity",
        )
    }
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAccess {
    pub id: String,
    pub expected_resource_version: String,
    pub name: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileWrite {
    pub id: String,
    pub expected_resource_version: String,
    pub name: String,
    pub content: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    pub image_id: String,
    pub argv: Vec<String>,
    pub name: String,
    pub release_version: String,
    pub license_expression: String,
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
    pub cpu_millicores: u32,
    pub memory_max_bytes: u64,
    pub pids_max: u32,
    pub ephemeral_max_bytes: u64,
    pub volume: Option<Target>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "tool", content = "input", deny_unknown_fields)]
pub enum Action {
    #[serde(rename = "system.status")]
    Status {},
    #[serde(rename = "agent.session.start")]
    StartAgentSession(agent::SessionBlueprint),
    #[serde(rename = "workflow.start")]
    StartWorkflow(workflow::StartBlueprint),
    #[serde(rename = "node.inventory")]
    Inventory {},
    #[serde(rename = "node.pressure")]
    Pressure {},
    #[serde(rename = "node.interfaces")]
    Interfaces(node::network::Query),
    #[serde(rename = "node.routes")]
    Routes(node::network::RouteQuery),
    #[serde(rename = "node.route_lookup")]
    RouteLookup(node::network::LookupQuery),
    #[serde(rename = "node.routing_rules")]
    RoutingRules(node::network::RuleQuery),
    #[serde(rename = "node.network_probe")]
    NetworkProbe(Target),
    #[serde(rename = "node.devices")]
    Devices {},
    #[serde(rename = "node.block_layout")]
    BlockLayout(node::block::Query),
    #[serde(rename = "node.mounts")]
    Mounts {},
    #[serde(rename = "node.processes")]
    Processes(node::process::Page),
    #[serde(rename = "node.process_inspect")]
    InspectProcess(node::process::Identity),
    #[serde(rename = "service.inspect")]
    ServiceInspect(Target),
    #[serde(rename = "service.observe")]
    ServiceObserve(node::service_state::Query),
    #[serde(rename = "service.logs")]
    ServiceLogs(node::journal::Query),
    #[serde(rename = "service.start")]
    ServiceStart(Target),
    #[serde(rename = "service.stop")]
    ServiceStop(Target),
    #[serde(rename = "service.restart")]
    ServiceRestart(ServiceRestart),
    #[serde(rename = "secret.mac")]
    Mac { secret_id: String, message: String },
    #[serde(rename = "volume.files")]
    Files(storage::files::Page),
    #[serde(rename = "volume.usage")]
    VolumeUsage(storage::usage::Query),
    #[serde(rename = "volume.verify_file")]
    VerifyVolumeFile(storage::file_integrity::Query),
    #[serde(rename = "volume.rename_file")]
    RenameFile(storage::file_move::Request),
    #[serde(rename = "volume.inspect_rename")]
    InspectFileRename(Target),
    #[serde(rename = "volume.read_chunk")]
    Chunk(storage::files::ReadChunk),
    #[serde(rename = "volume.append")]
    Append(storage::files::Append),
    #[serde(rename = "volume.mkdir")]
    MakeDir(storage::files::PathInput),
    #[serde(rename = "volume.remove_entry")]
    RemoveEntry(storage::files::PathInput),
    #[serde(rename = "volume.create")]
    Create { name: String },
    #[serde(rename = "volume.read")]
    Read(FileAccess),
    #[serde(rename = "volume.write")]
    Write(FileWrite),
    #[serde(rename = "volume.activate")]
    Activate(Target),
    #[serde(rename = "volume.delete")]
    Delete(Target),
    #[serde(rename = "backup.create")]
    Backup(Target),
    #[serde(rename = "backup.inspect_publication")]
    InspectBackupPublication(Target),
    #[serde(rename = "backup.verify")]
    VerifyBackup(Target),
    #[serde(rename = "backup.delete")]
    DeleteBackup(Target),
    #[serde(rename = "backup.restore")]
    Restore(Target),
    #[serde(rename = "volume.inspect_publication")]
    InspectRestoredVolume(Target),
    #[serde(rename = "application.stage_release")]
    Stage { target: Target, release: Install },
    #[serde(rename = "application.stage_artifact_release")]
    StageArtifactRelease(apps::artifact_install::StageRequest),
    #[serde(rename = "application.releases")]
    ListAppReleases(apps::inventory::Request),
    #[serde(rename = "application.select_release")]
    Select { target: Target, release_ref: Value },
    #[serde(rename = "application.retire_release")]
    RetireRelease(apps::release_retirement::Request),
    #[serde(rename = "application.refresh")]
    Refresh(Target),
    #[serde(rename = "application.install")]
    Install(Install),
    #[serde(rename = "application.install_artifact")]
    InstallArtifact(apps::artifact_install::Request),
    #[serde(rename = "application.capture_oci_image")]
    CaptureOciImage(apps::capture::Request),
    #[serde(rename = "application.restore_release_image")]
    RestoreReleaseImage(apps::restore_image::Request),
    #[serde(rename = "application.inspect_release_image_restore")]
    InspectReleaseImageRestore(Target),
    #[serde(rename = "artifact.publish_session_summary")]
    PublishSessionSummary(artifact::session_output::Request),
    #[serde(rename = "application.start")]
    Start(Target),
    #[serde(rename = "application.stop")]
    Stop(Target),
    #[serde(rename = "application.continue_stop")]
    ContinueOciStop(apps::stop::Request),
    #[serde(rename = "application.restart")]
    RestartApp(apps::restart::Request),
    #[serde(rename = "application.inspect_instance")]
    InspectOciInstance(Target),
    #[serde(rename = "application.uninstall")]
    Uninstall(Target),
    #[serde(rename = "application.health")]
    Health(Target),
    #[serde(rename = "application.logs")]
    Logs(Target),
}
impl Action {
    pub fn parse(tool: &str, input: &Value) -> Result<Self> {
        require(
            policy::TOOLS.contains(&tool),
            "UnsupportedCapability",
            "tool is not registered",
        )?;
        let a: Self = decode(&json!({"tool": tool, "input": input}))?;
        match &a {
            Self::RestartApp(request) => request.validate()?,
            Self::RestoreReleaseImage(request) => request.validate()?,
            Self::StartAgentSession(blueprint) => blueprint.validate()?,
            Self::StartWorkflow(blueprint) => blueprint.validate()?,
            Self::Processes(p) => p.validate()?,
            Self::InspectProcess(p) => p.validate()?,
            Self::Interfaces(q) => q.validate()?,
            Self::Routes(q) => q.validate()?,
            Self::RouteLookup(q) => q.validate()?,
            Self::RoutingRules(q) => q.validate()?,
            Self::BlockLayout(q) => q.validate()?,
            Self::ServiceLogs(q) => q.validate()?,
            Self::ServiceObserve(q) => q.validate()?,
            Self::ServiceRestart(restart) => restart.validate()?,
            Self::Create { name } => label(name)?,
            Self::Read(f) => storage::filename(&f.name)?,
            Self::VolumeUsage(query) => query.validate()?,
            Self::Write(f) => {
                storage::filename(&f.name)?;
                require(
                    f.content.len() <= 64 * 1024,
                    "QuotaExceeded",
                    "file import exceeds 64 KiB",
                )?
            }
            Self::Install(s) => apps::validate(s)?,
            Self::CaptureOciImage(request) => request.validate()?,
            Self::ListAppReleases(request) => request.validate()?,
            Self::PublishSessionSummary(request) => request.validate()?,
            Self::Stage { release, .. } => {
                apps::validate(release)?;
                require(
                    release.volume.is_none(),
                    "InvalidArgument",
                    "staged release inherits the application volume",
                )?;
            }
            Self::Select { release_ref, .. } => {
                let _: psios_api::ResourceRef = decode(release_ref)?;
            }
            Self::RetireRelease(request) => request.validate()?,
            Self::Mac { message, .. } => require(
                message.len() <= 8192,
                "QuotaExceeded",
                "MAC input exceeds limit",
            )?,
            _ => {}
        }
        storage::files::validate(&a)?;
        match &a {
            Self::VerifyVolumeFile(query) => query.validate()?,
            Self::RenameFile(request) => request.validate()?,
            _ => {}
        }
        Ok(a)
    }
    /// The primary target is first. A mounted volume and allocation pool are independent policy subjects.
    pub fn audience(
        &self,
        db: &Db<'_>,
        actor: &str,
        delegation: &Value,
        primary: &Doc,
    ) -> Result<Vec<Value>> {
        let mut resources = vec![primary.reference()];
        match self {
            Self::StartAgentSession(blueprint) => {
                resources.push(blueprint.skill(db)?.reference());
                resources.extend(blueprint.fallback_models(db)?.iter().map(Doc::reference));
            }
            Self::Start(_)
            | Self::Stop(_)
            | Self::ContinueOciStop(_)
            | Self::RestartApp(_)
            | Self::InspectOciInstance(_)
                if primary.spec["volume_id"].is_string() =>
            {
                let volume = db.of(primary.spec["volume_id"].as_str().unwrap(), "Volume")?;
                policy::owned(actor, &volume)?;
                resources.push(volume.reference());
            }
            Self::Restore(_) => resources.push(db.of("local", "StoragePool")?.reference()),
            Self::InstallArtifact(input) => {
                let source = apps::artifact_install::source(db, actor, &input.reference)?;
                apps::artifact_install::add_source_audience(
                    db,
                    actor,
                    delegation,
                    &source,
                    input.signature.as_ref(),
                    &mut resources,
                )?;
            }
            Self::PublishSessionSummary(input) => {
                resources.push(artifact::session_output::source(db, actor, input)?.reference());
            }
            Self::StageArtifactRelease(input) => {
                let source = apps::artifact_install::stage_source(db, actor, input, primary)?;
                apps::artifact_install::add_source_audience(
                    db,
                    actor,
                    delegation,
                    &source,
                    input.signature.as_ref(),
                    &mut resources,
                )?;
            }
            Self::RetireRelease(input) => {
                resources
                    .push(apps::release_retirement::candidate(db, primary, input)?.reference());
            }
            Self::RestoreReleaseImage(input) => {
                apps::restore_image::audience(db, primary, input, &mut resources)?;
            }
            Self::InspectReleaseImageRestore(_) => {
                apps::restore_image::inspection_audience(db, primary, &mut resources)?;
            }
            _ => {}
        }
        for r in &resources {
            policy::tool_scope(db, delegation, r["resource_id"].as_str().unwrap())?;
        }
        Ok(resources)
    }
    pub fn target(&self, db: &Db<'_>, actor: &str) -> Result<Doc> {
        let (id, version, kind) = match self {
            Self::Status {} => return db.get(policy::PROFILE),
            Self::StartAgentSession(blueprint) => return blueprint.model(db),
            Self::StartWorkflow(blueprint) => return blueprint.definition(db, actor),
            Self::NetworkProbe(target) => return node::network::probe::target(db, target),
            Self::PublishSessionSummary(_) => return db.of(&db.meta("authority")?, "Node"),
            Self::CaptureOciImage(request) => {
                apps::capture::authorize(db, actor)?;
                request.check_ceiling(db.cfg)?;
                return db.of(&db.meta("authority")?, "Node");
            }
            Self::Processes(_)
            | Self::InspectProcess(_)
            | Self::Interfaces(_)
            | Self::Routes(_)
            | Self::RouteLookup(_)
            | Self::RoutingRules(_)
            | Self::Inventory {}
            | Self::Pressure {}
            | Self::Mounts {}
            | Self::BlockLayout(_)
            | Self::Devices {} => {
                return db.of(&db.meta("authority")?, "Node");
            }
            Self::ServiceInspect(t) | Self::ServiceStart(t) | Self::ServiceStop(t) => {
                return node::service_target(db, &t.id, &t.expected_resource_version);
            }
            Self::ServiceLogs(q) => {
                return node::service_target(db, &q.id, &q.expected_resource_version);
            }
            Self::ServiceObserve(q) => return q.target(db),
            Self::ServiceRestart(r) => {
                return node::service_target(db, &r.id, &r.expected_resource_version);
            }
            Self::Create { .. } => return db.get("local"),
            Self::Mac { secret_id, .. } => (secret_id, None, "Secret"),
            Self::Files(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::VolumeUsage(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::VerifyVolumeFile(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::RenameFile(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::Chunk(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::Append(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::MakeDir(p) | Self::RemoveEntry(p) => (
                &p.target.id,
                Some(&p.target.expected_resource_version),
                "Volume",
            ),
            Self::Read(f) => (&f.id, Some(&f.expected_resource_version), "Volume"),
            Self::Write(f) => (&f.id, Some(&f.expected_resource_version), "Volume"),
            Self::Activate(t) | Self::Delete(t) | Self::Backup(t) => {
                (&t.id, Some(&t.expected_resource_version), "Volume")
            }
            Self::Restore(t) | Self::VerifyBackup(t) | Self::DeleteBackup(t) => {
                (&t.id, Some(&t.expected_resource_version), "Backup")
            }
            Self::InspectBackupPublication(t)
            | Self::InspectRestoredVolume(t)
            | Self::InspectFileRename(t) => {
                (&t.id, Some(&t.expected_resource_version), "Operation")
            }
            Self::RestartApp(p) => (&p.id, Some(&p.expected_resource_version), "Application"),
            Self::ContinueOciStop(p) => {
                policy::admin(actor)?;
                (
                    &p.target.id,
                    Some(&p.target.expected_resource_version),
                    "Application",
                )
            }
            Self::RestoreReleaseImage(p) => {
                policy::admin(actor)?;
                artifact::ingress::authorize(db, actor, artifact::ingress::READ)?;
                (
                    &p.target.id,
                    Some(&p.target.expected_resource_version),
                    "Application",
                )
            }
            Self::InspectReleaseImageRestore(p) => {
                policy::admin(actor)?;
                (&p.id, Some(&p.expected_resource_version), "Operation")
            }
            Self::Install(s) => return install_target(db, actor, s),
            Self::InstallArtifact(input) => {
                let install = apps::artifact_install::specification(db, actor, &input.reference)?;
                return install_target(db, actor, &install);
            }
            Self::StageArtifactRelease(input) => (
                &input.target.id,
                Some(&input.target.expected_resource_version),
                "Application",
            ),
            Self::ListAppReleases(input) => (&input.application_id, None, "Application"),
            Self::RetireRelease(input) => (
                &input.target.id,
                Some(&input.target.expected_resource_version),
                "Application",
            ),
            Self::Stage { target: t, .. }
            | Self::Select { target: t, .. }
            | Self::Refresh(t)
            | Self::Start(t)
            | Self::Stop(t)
            | Self::InspectOciInstance(t)
            | Self::Uninstall(t)
            | Self::Health(t)
            | Self::Logs(t) => (&t.id, Some(&t.expected_resource_version), "Application"),
        };
        let d = db.of(id, kind)?;
        policy::owned(actor, &d)?;
        if let Some(v) = version {
            d.cas(v)?
        }
        Ok(d)
    }
}

pub(crate) fn install_target(db: &Db<'_>, actor: &str, install: &Install) -> Result<Doc> {
    let Some(target) = &install.volume else {
        return db.get(policy::PROFILE);
    };
    let volume = db.of(&target.id, "Volume")?;
    policy::owned(actor, &volume)?;
    volume.cas(&target.expected_resource_version)?;
    Ok(volume)
}

pub fn status(db: &Db<'_>) -> Result<Value> {
    let uptime = std::fs::read_to_string("/proc/uptime")?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_owned();
    Ok(json!({
        "schema_version": VERSION,
        "authority_id": db.meta("authority")?,
        "execution_profile_ref": db.get(policy::RUNTIME_PROFILE)?.reference(),
        "recovery_incarnation": db.incarnation,
        "kernel_release": std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(),
        "uptime_seconds": uptime,
        "queue_depth": db.c.query_row("SELECT count(*) FROM queue WHERE phase IN ('Queued','Claimed','Dispatched','WaitingApproval','WaitingResource')",[],|r|r.get::<_,u32>(0))?
    }))
}
pub struct Work {
    pub task: Doc,
    pub run: Doc,
    pub op: Doc,
    pub effect: Doc,
    pub action: Action,
    pub target: Doc,
    pub cfg: Config,
    pub data: Option<PathBuf>,
    pub inline: Option<Value>,
    pub launch: Option<Install>,
    pub start_checkpoint: Option<Doc>,
    pub publication_checkpoint: Option<Doc>,
    pub volume: Option<Doc>,
    pub(crate) restore_image: Option<apps::restore_image::Prepared>,
    pub(crate) restore_image_inspection: Option<apps::restore_image::Recorded>,
    pub(crate) stop_cleanup: Option<apps::stop::Prepared>,
}
pub(crate) enum Output {
    Receipt(Value),
    OciImage(Box<apps::capture::Captured>),
}
pub(crate) enum Checkpoint<'a> {
    OciStart {
        engine: &'a str,
        container: &'a str,
    },
    BackupPublication {
        receipt: &'a Value,
        device: u64,
        inode: u64,
    },
    StorageDeletion(&'a storage::retirement::Location),
    RestoredVolume {
        receipt: &'a Value,
        placement: &'a storage::restore_publication::Placement,
    },
    OciRestart(&'a Value),
    OciStop(&'a Value),
    OciStopRemoval(&'a Value),
    OciStopCleanup(&'a Value),
    OciImageRestore(&'a apps::restore_image::Dispatch),
    FileRename(&'a storage::file_move::Witness),
    ServiceControl(&'a Value),
}

impl Work {
    fn checkpoint_authority(&self, db: &Db<'_>) -> Result<Doc> {
        let t = db.of(&self.task.id, "Task")?;
        require(
            t.body["current_run_id"] == self.run.id
                && t.body["lifecycle"] == "Running"
                && t.body["outcome_knowledge"] == "InFlight",
            "Fenced",
            "host effect is no longer current",
        )?;
        let target = self.action.target(db, &t.owner)?;
        let audience = self
            .action
            .audience(db, &t.owner, &t.spec["delegation_ref"], &target)?;
        permit(db, &t, &self.run, &self.op, &self.effect, &audience)?;
        Ok(target)
    }

    pub(crate) fn check_absent_stop_settlement(&self, db: &Db<'_>) -> Result<()> {
        require(
            matches!(self.action, Action::Stop(_)),
            "Fenced",
            "result has no OCI stop authority",
        )?;
        self.checkpoint_authority(db)?;
        Ok(())
    }

    pub(crate) fn check_streaming(&self, db: &Db<'_>) -> Result<()> {
        require(
            matches!(
                self.action,
                Action::Backup(_)
                    | Action::VerifyVolumeFile(_)
                    | Action::RenameFile(_)
                    | Action::InspectFileRename(_)
                    | Action::CaptureOciImage(_)
            ),
            "Fenced",
            "host data operation is outside its permit",
        )?;
        self.checkpoint_authority(db)?;
        Ok(())
    }

    pub(crate) fn check_result_publication(&self, db: &Db<'_>) -> Result<()> {
        if let Action::Select { release_ref, .. } = &self.action {
            let target = self.checkpoint_authority(db)?;
            return apps::check_release_selection(db, &target, release_ref);
        }
        if matches!(
            self.action,
            Action::VerifyVolumeFile(_)
                | Action::InspectFileRename(_)
                | Action::InspectProcess(_)
                | Action::ListAppReleases(_)
                | Action::InstallArtifact(_)
                | Action::StageArtifactRelease(_)
                | Action::RetireRelease(_)
                | Action::CaptureOciImage(_)
                | Action::InspectReleaseImageRestore(_)
                | Action::Logs(_)
        ) {
            self.checkpoint_authority(db)?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&self, db: &Db<'_>, event: Checkpoint<'_>) -> Result<()> {
        let target = self.checkpoint_authority(db)?;
        match (&self.action, event) {
            (Action::Start(_), Checkpoint::OciStart { engine, container }) => {
                apps::check_selected_publisher(db, &target)?;
                let evidence = json!({"start_operation_id":self.op.id, "effect_ref":self.effect.reference(),
                    "application_ref":target.reference(),"volume_ref":self.volume.as_ref().map(Doc::reference),
                    "engine_id":engine, "container_id":container});
                db.create(
                    "OCIStartCheckpoint",
                    &format!("oci-start-{}", self.op.id),
                    &self.task.owner,
                    json!({}),
                    evidence,
                )?;
                Ok(())
            }
            (
                Action::Backup(_),
                Checkpoint::BackupPublication {
                    receipt,
                    device,
                    inode,
                },
            ) => storage::publication::record(db, self, receipt, device, inode),
            (Action::Restore(_), Checkpoint::RestoredVolume { receipt, placement }) => {
                storage::restore_publication::record(db, self, receipt, placement)
            }
            (Action::RenameFile(request), Checkpoint::FileRename(before)) => {
                storage::file_move::record(db, self, request, before)
            }
            (Action::RestartApp(request), Checkpoint::OciRestart(before)) => {
                apps::check_selected_publisher(db, &target)?;
                apps::restart::record(db, self, request, before)
            }
            (Action::Stop(_), Checkpoint::OciStop(before)) => {
                apps::stop::record(db, self, before, false)
            }
            (Action::Stop(_), Checkpoint::OciStopRemoval(exited)) => {
                apps::stop::record(db, self, exited, true)
            }
            (Action::ContinueOciStop(request), Checkpoint::OciStopCleanup(exited)) => {
                apps::stop::record_cleanup(db, self, request, exited)
            }
            (Action::RestoreReleaseImage(request), Checkpoint::OciImageRestore(dispatch)) => {
                apps::restore_image::record(db, self, &target, request, dispatch)
            }
            (action, Checkpoint::ServiceControl(before))
                if node::service_control::is_control(action) =>
            {
                node::service_control::record(db, self, &target, before)
            }
            (
                Action::Delete(_) | Action::DeleteBackup(_),
                Checkpoint::StorageDeletion(location),
            ) => storage::retirement::prepare(db, self, location),
            _ => Err(err("Fenced", "checkpoint does not match effect")),
        }
    }
    #[cfg(test)]
    pub(crate) fn perform(
        &self,
        checkpoint: impl FnMut(Checkpoint<'_>) -> Result<()>,
    ) -> Result<Value> {
        self.perform_receipt(checkpoint, || Ok(()))
    }

    pub(crate) fn perform_checked(
        &self,
        checkpoint: impl FnMut(Checkpoint<'_>) -> Result<()>,
        mut check_streaming: impl FnMut() -> Result<()>,
    ) -> Result<Output> {
        if let Action::CaptureOciImage(request) = &self.action {
            return apps::capture::export(self, request, &mut check_streaming)
                .map(|capture| Output::OciImage(Box::new(capture)));
        }
        self.perform_receipt(checkpoint, check_streaming)
            .map(Output::Receipt)
    }

    fn perform_receipt(
        &self,
        mut checkpoint: impl FnMut(Checkpoint<'_>) -> Result<()>,
        mut check_streaming: impl FnMut() -> Result<()>,
    ) -> Result<Value> {
        if let Some(v) = &self.inline {
            return Ok(v.clone());
        }
        match &self.action {
            Action::ServiceInspect(_)
            | Action::ServiceObserve(_)
            | Action::ServiceLogs(_)
            | Action::ServiceStart(_)
            | Action::ServiceStop(_)
            | Action::ServiceRestart(_)
            | Action::Inventory {}
            | Action::Pressure {}
            | Action::Processes(_)
            | Action::InspectProcess(_)
            | Action::Interfaces(_)
            | Action::Routes(_)
            | Action::RouteLookup(_)
            | Action::RoutingRules(_)
            | Action::NetworkProbe(_)
            | Action::Mounts {}
            | Action::BlockLayout(_)
            | Action::Devices {} => node::perform(self, |before| {
                checkpoint(Checkpoint::ServiceControl(before))
            }),
            Action::Start(_) => apps::start(self, |engine, container| {
                checkpoint(Checkpoint::OciStart { engine, container })
            }),
            Action::Backup(_) => storage::backup(
                self,
                |receipt, device, inode| {
                    checkpoint(Checkpoint::BackupPublication {
                        receipt,
                        device,
                        inode,
                    })
                },
                &mut check_streaming,
            ),
            Action::VerifyVolumeFile(query) => {
                storage::file_integrity::verify(self, query, &mut check_streaming)
            }
            Action::RenameFile(request) => storage::file_move::rename(
                self,
                request,
                |before| checkpoint(Checkpoint::FileRename(before)),
                &mut check_streaming,
            ),
            Action::InspectFileRename(_) => storage::file_move::inspect(self, &mut check_streaming),
            Action::Restore(_) => storage::restore(self, |receipt, placement| {
                checkpoint(Checkpoint::RestoredVolume { receipt, placement })
            }),
            Action::RestartApp(request) => apps::restart::run(self, request, |before| {
                checkpoint(Checkpoint::OciRestart(before))
            }),
            Action::Stop(_) => apps::stop::run(self, checkpoint),
            Action::ContinueOciStop(_) => apps::stop::cleanup(self, checkpoint),
            Action::RestoreReleaseImage(_) => apps::restore_image::run(self, |dispatch| {
                checkpoint(Checkpoint::OciImageRestore(dispatch))
            }),
            Action::InspectReleaseImageRestore(_) => apps::restore_image::inspect(self),
            Action::Delete(_) | Action::DeleteBackup(_) => {
                storage::retirement::retire(self, |location| {
                    checkpoint(Checkpoint::StorageDeletion(location))
                })
            }
            Action::Stage { .. }
            | Action::StageArtifactRelease(_)
            | Action::Select { .. }
            | Action::Refresh(_)
            | Action::Uninstall(_)
            | Action::Install(_)
            | Action::InstallArtifact(_)
            | Action::RetireRelease(_)
            | Action::Health(_)
            | Action::InspectOciInstance(_)
            | Action::Logs(_) => apps::perform(self),
            _ => storage::perform(self),
        }
    }
}
fn permit(
    db: &Db<'_>,
    task: &Doc,
    run: &Doc,
    op: &Doc,
    effect: &Doc,
    audience: &[Value],
) -> Result<()> {
    let permit = db.resolve(&effect.spec["permit_ref"])?;
    let proof = db.resolve(&permit.body["auth_proof"]["record_ref"])?;
    let plan = db.resolve(&op.body["plan_ref"])?;
    let tool = task.spec["task_type"].as_str().unwrap();
    let delegation = db.resolve(&permit.body["authorization_ref"])?;
    policy::active(db, &task.owner)?;
    policy::delegation(db, &task.owner, &delegation.id, tool)?;
    require(
        permit.kind == "ExecutionPermit"
            && permit.owner == task.owner
            && proof.kind == "PermitBinding"
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["audience_resource_refs"] == json!(audience)
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["authorization_epoch"] == db.meta("epoch")?
            && permit.body["recovery_incarnation"] == db.incarnation
            && permit.body["security_recovery_epoch"] == "1"
            && proof.body["claim_id"] == run.body["claim_id"]
            && proof.body["claim_epoch"] == run.body["claim_epoch"]
            && proof.body["permit_id"] == permit.id
            && proof.body["audience_resource_refs"] == json!(audience)
            && plan.body["allowed_effects"][0]["action"] == tool
            && !policy::deadline::Clock::sample()?.expired(
                db,
                &permit.id,
                permit.body["not_after"].as_str().unwrap(),
            )?,
        "Fenced",
        "host permit does not authorize this execution",
    )?;
    if policy::approval(tool) {
        let decision = db.resolve(&permit.body["decision_ref"])?;
        let bound = db.of(&format!("use-{}", decision.id), "ApprovalUse")?;
        require(
            bound.body["operation_id"] == op.id
                && bound.body["validity_status"] == "Valid"
                && decision.body["decision"] == "Approved"
                && decision.body["plan_digest"] == permit.body["plan_digest"],
            "ApprovalInvalidated",
            "host requires a bound single-use decision",
        )?;
    }
    Ok(())
}
pub fn work(db: &Db<'_>, task: Doc, run: Doc, op: Doc, effect: Doc) -> Result<Work> {
    let input = db.resolve(&task.spec["input_ref"])?;
    let actor = &task.owner;
    let a = Action::parse(
        task.spec["task_type"].as_str().unwrap(),
        crate::broker::dependencies::tool_input(&input)?,
    )?;
    let target = a.target(db, actor)?;
    let audience = a.audience(db, actor, &task.spec["delegation_ref"], &target)?;
    permit(db, &task, &run, &op, &effect, &audience)?;
    let inline = match &a {
        Action::Status {} => Some(status(db)?),
        Action::Mac { secret_id, message } => Some(policy::mac(db, actor, secret_id, message)?),
        Action::ListAppReleases(input) => Some(apps::inventory::page(db, actor, input, &target)?),
        Action::Logs(_) if target.body["lifecycle"] == "Stopped" => {
            Some(apps::log_tail::retained(db, &target)?)
        }
        _ => None,
    };
    let rename_inspection = if matches!(a, Action::InspectFileRename(_)) {
        Some(storage::file_move::pending(db, &target)?)
    } else {
        None
    };
    let volume = if let Some((volume, _)) = &rename_inspection {
        Some(volume.clone())
    } else if target.kind == "Volume" {
        Some(target.clone())
    } else if target.kind == "Application"
        && !matches!(
            a,
            Action::Stage { .. }
                | Action::StageArtifactRelease(_)
                | Action::ListAppReleases(_)
                | Action::Select { .. }
                | Action::Refresh(_)
                | Action::RetireRelease(_)
                | Action::RestoreReleaseImage(_)
        )
    {
        target.spec["volume_id"]
            .as_str()
            .map(|id| db.of(id, "Volume"))
            .transpose()?
    } else {
        None
    };
    let start_checkpoint =
        if matches!(a, Action::Health(_)) && target.body["lifecycle"] == "Stopped" {
            apps::pending_start(db, &target)?
        } else {
            None
        };
    let publication_checkpoint = match a {
        Action::InspectFileRename(_) => rename_inspection.map(|(_, checkpoint)| checkpoint),
        Action::InspectBackupPublication(_) => Some(storage::publication::pending(db, &target)?),
        Action::InspectRestoredVolume(_) => {
            Some(storage::restore_publication::pending(db, &target)?)
        }
        _ => None,
    };
    let data = if let Some(v) = &volume
        && (start_checkpoint.is_some()
            || !matches!(
                a,
                Action::Stop(_)
                    | Action::ContinueOciStop(_)
                    | Action::Logs(_)
                    | Action::Health(_)
                    | Action::Uninstall(_)
            )) {
        policy::owned(actor, v)?;
        Some(storage::volume_dir(db.cfg, v)?)
    } else {
        None
    };
    let launch = match &a {
        Action::InstallArtifact(input) => Some(apps::artifact_install::specification(
            db,
            actor,
            &input.reference,
        )?),
        Action::StageArtifactRelease(input) => Some(apps::artifact_install::specification(
            db,
            actor,
            &input.reference,
        )?),
        Action::Start(_) | Action::RestartApp(_) => Some(apps::launch_spec(db, &target, true)?),
        Action::InspectOciInstance(_) => Some(apps::launch_spec(db, &target, false)?),
        _ if start_checkpoint.is_some() => Some(apps::launch_spec(db, &target, false)?),
        _ => None,
    };
    let restore_image = if let Action::RestoreReleaseImage(input) = &a {
        Some(apps::restore_image::prepare(db, &target, input)?)
    } else {
        None
    };
    let restore_image_inspection = if matches!(a, Action::InspectReleaseImageRestore(_)) {
        Some(apps::restore_image::pending(db, &target)?)
    } else {
        None
    };
    let stop_cleanup = if let Action::ContinueOciStop(request) = &a {
        Some(apps::stop::prepare(db, &target, request)?)
    } else {
        None
    };
    let w = Work {
        start_checkpoint,
        publication_checkpoint,
        launch,
        task,
        run,
        op,
        effect,
        action: a,
        target,
        cfg: db.cfg.clone(),
        data,
        inline,
        volume,
        restore_image,
        restore_image_inspection,
        stop_cleanup,
    };
    node::reserve(db, &w)?;
    storage::reserve(db, &w)?;
    if matches!(w.action, Action::Backup(_)) {
        storage::publication::begin(db, &w)?;
    }
    if matches!(w.action, Action::Restore(_)) {
        storage::restore_publication::begin(db, &w)?;
    }
    if matches!(w.action, Action::RestartApp(_)) {
        apps::restart::begin(db, &w)?;
    }
    if matches!(w.action, Action::Stop(_)) {
        apps::stop::begin(db, &w)?;
    }
    if let Some(prepared) = &w.stop_cleanup {
        apps::stop::begin_cleanup(db, &w, prepared)?;
    }
    if let Some(image) = &w.restore_image {
        apps::restore_image::begin(db, &w, image)?;
    }
    if matches!(w.action, Action::RenameFile(_)) {
        storage::file_move::begin(db, &w)?;
    }
    if storage::retirement::deletion(&w.action) {
        storage::retirement::begin(db, &w)?;
    }
    apps::preflight(db, &w)?;
    Ok(w)
}
pub fn apply(db: &Db<'_>, w: &Work, receipt: &Value) -> Result<Value> {
    match w.action {
        Action::ServiceObserve(_) => node::service_state::publish(db, w, receipt),
        Action::ServiceStart(_) | Action::ServiceStop(_) | Action::ServiceRestart(_) => {
            node::apply(db, w, receipt)
        }
        Action::RestoreReleaseImage(_) => apps::restore_image::settle(db, w, receipt),
        Action::ContinueOciStop(_) => apps::stop::settle_cleanup(db, w, receipt),
        Action::Stage { .. }
        | Action::StageArtifactRelease(_)
        | Action::Select { .. }
        | Action::Refresh(_)
        | Action::Install(_)
        | Action::InstallArtifact(_)
        | Action::RetireRelease(_)
        | Action::Start(_)
        | Action::RestartApp(_)
        | Action::Stop(_)
        | Action::Health(_)
        | Action::Logs(_)
        | Action::Uninstall(_) => apps::apply(db, w, receipt),
        _ => storage::apply(db, w, receipt),
    }
}
