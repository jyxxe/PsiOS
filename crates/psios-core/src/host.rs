//! Typed effect boundary. All filesystem roots, runtime flags and identity come from the authority.
use crate::{
    apps,
    config::Config,
    db::{Db, Doc},
    policy, storage, *,
};
use psios_api::{BlockInventoryQuery, VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::File, path::PathBuf};

pub(crate) const READ_PUBLICATION_FORMAT: u32 = agent::TARGET_AUTHORIZATION_FORMAT + 1;

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
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum VolumeAccess {
    #[default]
    ReadWrite,
    ReadOnly,
}

impl VolumeAccess {
    pub(crate) fn read_only(self) -> bool {
        self == Self::ReadOnly
    }
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
    pub(crate) volume_access: Option<VolumeAccess>,
    pub(crate) health_check: Option<apps::health::Probe>,
}

impl Install {
    pub(crate) fn volume_access(&self) -> VolumeAccess {
        self.volume_access.unwrap_or_default()
    }
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
    #[serde(rename = "project.submit_evidence")]
    SubmitWorkItemEvidence(project::agent_tool::Input),
    #[serde(rename = "project.list_assigned_work")]
    ListAssignedWork(project::assigned::Input),
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
    #[serde(rename = "node.observe_network_probe")]
    ObserveNetworkProbe(Target),
    #[serde(rename = "network.trial")]
    NetworkTrial(Target),
    #[serde(rename = "network.inspect_trial")]
    InspectNetworkTrial(Target),
    #[serde(rename = "node.devices")]
    Devices(BlockInventoryQuery),
    #[serde(rename = "node.block_layout")]
    BlockLayout(node::block::Query),
    #[serde(rename = "node.track_block_device")]
    TrackBlockDevice(node::block_device::Enrollment),
    #[serde(rename = "device.block_layout")]
    ObserveBlockDevice(Target),
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
    #[serde(rename = "service.resources")]
    ServiceResources(node::service_resources::Query),
    #[serde(rename = "service.logs")]
    ServiceLogs(node::journal::Query),
    #[serde(rename = "service.logs_since")]
    ServiceLogsSince(node::journal::SinceQuery),
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
    #[serde(rename = "backup.estimate")]
    EstimateBackup(Target),
    #[serde(rename = "backup.inspect_publication")]
    InspectBackupPublication(Target),
    #[serde(rename = "backup.verify")]
    VerifyBackup(Target),
    #[serde(rename = "backup.delete")]
    DeleteBackup(Target),
    #[serde(rename = "backup.restore")]
    Restore(Target),
    #[serde(rename = "volume.clone")]
    CloneVolume(storage::cloning::Input),
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
    #[serde(rename = "application.rebind_restored_volume")]
    RebindRestoredVolume(apps::volume_rebind::Input),
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
    #[serde(rename = "application.inspect_start_create")]
    InspectOciCreate(Target),
    #[serde(rename = "application.discard_start_create")]
    DiscardOciCreate(apps::create_recovery::Request),
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
    pub(crate) fn is_network_probe(&self) -> bool {
        matches!(self, Self::NetworkProbe(_) | Self::ObserveNetworkProbe(_))
    }

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
            Self::SubmitWorkItemEvidence(input) => input.validate()?,
            Self::ListAssignedWork(input) => input.validate()?,
            Self::Processes(p) => p.validate()?,
            Self::InspectProcess(p) => p.validate()?,
            Self::Interfaces(q) => q.validate()?,
            Self::Routes(q) => q.validate()?,
            Self::RouteLookup(q) => q.validate()?,
            Self::RoutingRules(q) => q.validate()?,
            Self::Devices(q) => node::block::validate_inventory(q)?,
            Self::BlockLayout(q) => q.validate()?,
            Self::TrackBlockDevice(q) => q.validate()?,
            Self::ServiceLogs(q) => q.validate()?,
            Self::ServiceLogsSince(q) => q.validate()?,
            Self::ServiceObserve(q) => q.validate()?,
            Self::ServiceResources(q) => q.validate()?,
            Self::ServiceRestart(restart) => restart.validate()?,
            Self::Create { name } => label(name)?,
            Self::CloneVolume(input) => input.validate()?,
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
                agent::session_inputs::add_task_audience(
                    db,
                    actor,
                    delegation,
                    blueprint,
                    &mut resources,
                )?;
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
            Self::Health(_)
                if primary.body["lifecycle"] == "Stopped"
                    && primary.spec["volume_id"].is_string() =>
            {
                if let Some(checkpoint) = apps::pending_start(db, primary)?
                    && (db.cfg.oci_effective_volume_observation
                        || apps::mount_observation::required(&checkpoint))
                {
                    let volume = db.of(primary.spec["volume_id"].as_str().unwrap(), "Volume")?;
                    policy::owned(actor, &volume)?;
                    resources.push(volume.reference());
                }
            }
            Self::InspectOciCreate(_) => {
                policy::tool_scope(db, delegation, &primary.id)?;
                resources.extend(apps::create_recovery::audience(db, primary)?);
            }
            Self::DiscardOciCreate(request) => {
                policy::tool_scope(db, delegation, &primary.id)?;
                let operation = request.operation_ref["resource_id"]
                    .as_str()
                    .ok_or_else(|| {
                        err(
                            "InvalidArgument",
                            "expected an exact original operation reference",
                        )
                    })?;
                policy::tool_scope(db, delegation, operation)?;
                let original = apps::create_recovery::original(db, request, primary)?;
                resources.push(original.reference());
                resources.extend(
                    apps::create_recovery::audience(db, &original)?
                        .into_iter()
                        .skip(1),
                );
            }
            Self::Restore(_) | Self::CloneVolume(_) => {
                resources.push(db.of("local", "StoragePool")?.reference())
            }
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
            Self::RebindRestoredVolume(input) => {
                let (source, restored) = input.volumes(db, actor)?;
                resources.extend([source.reference(), restored.reference()]);
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
            Self::SubmitWorkItemEvidence(input) => return input.target(db, actor),
            Self::ListAssignedWork(input) => return input.target(db, actor),
            Self::NetworkProbe(target) | Self::ObserveNetworkProbe(target) => {
                return node::network::probe::target(db, target);
            }
            Self::NetworkTrial(target) => return node::network::trial::target(db, actor, target),
            Self::InspectNetworkTrial(target) => {
                policy::admin(actor)?;
                (
                    &target.id,
                    Some(&target.expected_resource_version),
                    "Operation",
                )
            }
            Self::PublishSessionSummary(_) => return db.of(&db.meta("authority")?, "Node"),
            Self::TrackBlockDevice(query) => {
                return node::block_device::enrollment_target(db, actor, query);
            }
            Self::ObserveBlockDevice(query) => return node::block_device::target(db, actor, query),
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
            | Self::Devices(_) => {
                return db.of(&db.meta("authority")?, "Node");
            }
            Self::ServiceInspect(t) | Self::ServiceStart(t) | Self::ServiceStop(t) => {
                return node::service_target(db, &t.id, &t.expected_resource_version);
            }
            Self::ServiceLogs(q) => {
                return node::service_target(db, &q.id, &q.expected_resource_version);
            }
            Self::ServiceLogsSince(q) => {
                return node::service_target(db, &q.id, &q.expected_resource_version);
            }
            Self::ServiceObserve(q) => return q.target(db),
            Self::ServiceResources(q) => {
                policy::can_target(db, actor, node::service_resources::TOOL, &q.id)?;
                return node::service_target(db, &q.id, &q.expected_resource_version);
            }
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
            Self::CloneVolume(input) => {
                policy::admin(actor)?;
                (
                    &input.target.id,
                    Some(&input.target.expected_resource_version),
                    "Volume",
                )
            }
            Self::Activate(t) | Self::Delete(t) | Self::Backup(t) | Self::EstimateBackup(t) => {
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
            Self::InspectOciCreate(p) => {
                policy::admin(actor)?;
                (&p.id, Some(&p.expected_resource_version), "Operation")
            }
            Self::DiscardOciCreate(p) => {
                policy::admin(actor)?;
                (
                    &p.target.id,
                    Some(&p.target.expected_resource_version),
                    "Application",
                )
            }
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
            Self::RebindRestoredVolume(input) => (
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
        require(
            !install.volume_access().read_only(),
            "InvalidArgument",
            "read-only volume access requires an approved volume",
        )?;
        return db.get(policy::PROFILE);
    };
    let volume = db.of(&target.id, "Volume")?;
    policy::owned(actor, &volume)?;
    volume.cas(&target.expected_resource_version)?;
    Ok(volume)
}

pub fn status(db: &Db<'_>) -> Result<Value> {
    let millis = crate::linux_clock::elapsed_millis()?;
    let uptime = format!("{}.{:02}", millis / 1000, millis % 1000 / 10);
    let kernel = crate::linux_proc::kernel_release(&crate::linux_proc::root()?)?;
    Ok(json!({
        "schema_version": VERSION,
        "authority_id": db.meta("authority")?,
        "execution_profile_ref": db.get(policy::RUNTIME_PROFILE)?.reference(),
        "recovery_incarnation": db.incarnation,
        "kernel_release": kernel,
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
    pub(crate) oci_health_check: Option<apps::health::Probe>,
    pub start_checkpoint: Option<Doc>,
    pub publication_checkpoint: Option<Doc>,
    pub volume: Option<Doc>,
    pub(crate) attached_read: Option<apps::attached_read::AttachedRead>,
    pub(crate) restore_image: Option<apps::restore_image::Prepared>,
    pub(crate) restore_image_inspection: Option<apps::restore_image::Recorded>,
    pub(crate) stop_cleanup: Option<apps::stop::Prepared>,
    pub(crate) create_inspection: Option<apps::create_recovery::Inspection>,
    pub(crate) create_cleanup: Option<apps::create_recovery::Prepared>,
    pub(crate) volume_rebind: Option<apps::volume_rebind::Prepared>,
    pub(crate) mount_observation_required: bool,
    pub(crate) service_resources_node_id: Option<String>,
    network_trial_inspection: Option<node::network::trial::Inspection>,
}
pub(crate) enum Output {
    Receipt(Value),
    OciImage(Box<apps::capture::Captured>),
}
pub(crate) enum Checkpoint<'a> {
    OciCreateIssued {
        engine: &'a str,
        attempt: &'a str,
        boot: &'a str,
    },
    OciCreateAcknowledged(&'a str),
    OciCreateRemoval(&'a Value),
    OciStart {
        engine: &'a str,
        container: &'a str,
    },
    BackupPublication {
        receipt: &'a Value,
        placement: &'a storage::publication::Placement,
    },
    StorageDeletion(&'a storage::retirement::Location),
    EntryRetirement(&'a storage::entry_retirement::Location),
    DirectoryAllocation(&'a storage::directory_create::Parents),
    DirectoryCreation(&'a storage::directory_create::Location),
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
    NetworkTrial(node::network::trial::Boundary<'a>),
    ServiceControl(&'a Value),
}

impl Work {
    pub(crate) fn start_host_effect(&self, db: &Db<'_>) -> Result<Doc> {
        let effect = db.of(&self.effect.id, "Effect")?;
        let operation = db.of(&self.op.id, "Operation")?;
        let task = db.of(&self.task.id, "Task")?;
        let run = db.of(&self.run.id, "Run")?;
        require(
            effect.reference() == self.effect.reference()
                && effect.body["phase"] == "PermitBound"
                && operation.body["effect_refs"] == json!([effect.reference()])
                && task.body["operation_refs"] == json!([operation.reference()])
                && run.body["operation_refs"] == json!([operation.reference()]),
            "Fenced",
            "host effect permit has already been used or replaced",
        )?;
        let mut body = effect.body.clone();
        body["phase"] = json!("EffectStarted");
        let started = db.status(&effect, body)?;
        let mut body = operation.body.clone();
        body["effect_refs"] = json!([started.reference()]);
        let operation = db.status(&operation, body)?;
        for document in [run, task] {
            let mut body = document.body.clone();
            body["operation_refs"] = json!([operation.reference()]);
            db.status(&document, body)?;
        }
        Ok(started)
    }

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
        let audience_target = if matches!(self.action, Action::ServiceObserve(_)) {
            // A newer status of the same registered service generation may
            // supersede this sample. Its permit still names the sampled revision.
            require(
                target.id == self.target.id && target.generation == self.target.generation,
                "Fenced",
                "registered service changed during observation",
            )?;
            &self.target
        } else {
            &target
        };
        let audience =
            self.action
                .audience(db, &t.owner, &t.spec["delegation_ref"], audience_target)?;
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
            self.action.is_network_probe()
                || matches!(
                    self.action,
                    Action::Backup(_)
                        | Action::EstimateBackup(_)
                        | Action::CloneVolume(_)
                        | Action::VerifyVolumeFile(_)
                        | Action::RenameFile(_)
                        | Action::InspectFileRename(_)
                        | Action::CaptureOciImage(_)
                        | Action::RebindRestoredVolume(_)
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
        let tool = self.task.spec["task_type"].as_str().unwrap_or_default();
        let read_only = policy::readonly(tool);
        if read_only {
            agent::check_dispatch(db, &self.task)?;
        }
        if read_only
            || apps::catalog::is_tool(tool)
            || matches!(
                self.action,
                Action::InstallArtifact(_)
                    | Action::StageArtifactRelease(_)
                    | Action::RetireRelease(_)
                    | Action::RebindRestoredVolume(_)
                    | Action::CaptureOciImage(_)
            )
        {
            self.checkpoint_authority(db)?;
        }
        Ok(())
    }

    pub(crate) fn checkpoint(&self, db: &Db<'_>, event: Checkpoint<'_>) -> Result<()> {
        if let Checkpoint::NetworkTrial(boundary) = event {
            require(
                matches!(self.action, Action::NetworkTrial(_)),
                "Fenced",
                "checkpoint does not match effect",
            )?;
            return node::network::trial::record(db, self, boundary);
        }
        if let Checkpoint::OciCreateAcknowledged(container) = event {
            require(
                matches!(self.action, Action::Start(_)),
                "Fenced",
                "create acknowledgement is outside its operation",
            )?;
            return apps::create_recovery::acknowledge(db, self, container);
        }
        let target = self.checkpoint_authority(db)?;
        match (&self.action, event) {
            (
                Action::Start(_),
                Checkpoint::OciCreateIssued {
                    engine,
                    attempt,
                    boot,
                },
            ) => {
                apps::check_selected_publisher(db, &target)?;
                apps::create_recovery::issue(db, self, engine, attempt, boot)
            }
            (Action::DiscardOciCreate(request), Checkpoint::OciCreateRemoval(sample)) => {
                apps::create_recovery::record_cleanup(db, self, request, sample)
            }
            (Action::Start(_), Checkpoint::OciStart { engine, container }) => {
                apps::check_selected_publisher(db, &target)?;
                let acknowledged = apps::create_recovery::allow_start(db, self, engine, container)?;
                let evidence = json!({"start_operation_id":self.op.id, "effect_ref":acknowledged.body["started_effect_ref"],
                    "create_ack_ref":acknowledged.reference(),
                    "application_ref":target.reference(),"volume_ref":self.volume.as_ref().map(Doc::reference),
                    "engine_id":engine, "container_id":container,"mount_observation_required":self.mount_observation_required});
                db.create(
                    "OCIStartCheckpoint",
                    &format!("oci-start-{}", self.op.id),
                    &self.task.owner,
                    json!({}),
                    evidence,
                )?;
                Ok(())
            }
            (Action::Backup(_), Checkpoint::BackupPublication { receipt, placement }) => {
                storage::publication::record(db, self, receipt, placement)
            }
            (
                Action::Create { .. } | Action::Restore(_) | Action::CloneVolume(_),
                Checkpoint::RestoredVolume { receipt, placement },
            ) => storage::restore_publication::record(db, self, receipt, placement),
            (Action::RemoveEntry(request), Checkpoint::EntryRetirement(location)) => {
                storage::entry_retirement::prepare(db, self, &request.path, location)
            }
            (Action::MakeDir(_), Checkpoint::DirectoryAllocation(parents)) => {
                storage::directory_create::reserve_stage(db, self, parents)
            }
            (Action::MakeDir(_), Checkpoint::DirectoryCreation(location)) => {
                storage::directory_create::prepare(db, self, location)
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
            Action::SubmitWorkItemEvidence(_) => Err(err(
                "RecoveryRequired",
                "work item evidence requires the authority transaction",
            )),
            Action::ListAssignedWork(_) => Err(err(
                "RecoveryRequired",
                "assigned work discovery requires the authority transaction",
            )),
            Action::ServiceInspect(_)
            | Action::ServiceObserve(_)
            | Action::ServiceResources(_)
            | Action::ServiceLogs(_)
            | Action::ServiceLogsSince(_)
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
            | Action::Mounts {}
            | Action::BlockLayout(_)
            | Action::TrackBlockDevice(_)
            | Action::ObserveBlockDevice(_)
            | Action::Devices(_) => node::perform(self, |before| {
                checkpoint(Checkpoint::ServiceControl(before))
            }),
            Action::Start(_) => apps::start(self, checkpoint),
            Action::InspectOciCreate(_) => apps::create_recovery::inspect(self),
            Action::DiscardOciCreate(_) => apps::create_recovery::cleanup(self, checkpoint),
            Action::Backup(_) => storage::backup(
                self,
                |receipt, placement| {
                    checkpoint(Checkpoint::BackupPublication { receipt, placement })
                },
                &mut check_streaming,
            ),
            Action::EstimateBackup(_) => storage::estimate::run(self, &mut check_streaming),
            Action::VerifyVolumeFile(query) => {
                storage::file_integrity::verify(self, query, &mut check_streaming)
            }
            Action::RemoveEntry(request) => {
                storage::entry_retirement::retire(self, &request.path, |location| {
                    checkpoint(Checkpoint::EntryRetirement(location))
                })
            }
            Action::MakeDir(request) => {
                storage::directory_create::publish(self, &request.path, checkpoint)
            }
            Action::RenameFile(request) => storage::file_move::rename(
                self,
                request,
                |before| checkpoint(Checkpoint::FileRename(before)),
                &mut check_streaming,
            ),
            Action::InspectFileRename(_) => storage::file_move::inspect(self, &mut check_streaming),
            Action::Create { .. } => storage::create_volume(
                self,
                None::<fn(&File) -> Result<Value>>,
                |receipt, placement| checkpoint(Checkpoint::RestoredVolume { receipt, placement }),
            ),
            Action::CloneVolume(_) => storage::cloning::run(
                self,
                |receipt, placement| checkpoint(Checkpoint::RestoredVolume { receipt, placement }),
                &mut check_streaming,
            ),
            Action::Restore(_) => storage::restore(self, |receipt, placement| {
                checkpoint(Checkpoint::RestoredVolume { receipt, placement })
            }),
            Action::RebindRestoredVolume(_) => {
                apps::volume_rebind::observe(self, &mut check_streaming)
            }
            Action::RestartApp(request) => apps::restart::run(self, request, |before| {
                checkpoint(Checkpoint::OciRestart(before))
            }),
            Action::NetworkProbe(_) | Action::ObserveNetworkProbe(_) => {
                node::network::probe::perform(self, &mut check_streaming)
            }
            Action::NetworkTrial(_) => node::network::trial::perform(&self.target, |boundary| {
                checkpoint(Checkpoint::NetworkTrial(boundary))
            }),
            Action::InspectNetworkTrial(_) => self
                .network_trial_inspection
                .as_ref()
                .ok_or_else(|| {
                    err(
                        "RecoveryRequired",
                        "network trial inspection has no custody",
                    )
                })?
                .artifact(),
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
pub(crate) fn permit(
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
    policy::delegation(db, &task.owner, &delegation.id, "task.submit")?;
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
                    | Action::RebindRestoredVolume(_)
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
    let mount_observation_required = if volume.is_some() {
        let observe = db.cfg.oci_effective_volume_observation
            || launch
                .as_ref()
                .is_some_and(|release| release.volume_access().read_only());
        match a {
            Action::Start(_) | Action::RestartApp(_) => observe,
            Action::InspectOciInstance(_) => {
                observe || apps::restart::pending_mount_requirement(db, &target)?
            }
            Action::Health(_) => start_checkpoint
                .as_ref()
                .is_some_and(|checkpoint| observe || apps::mount_observation::required(checkpoint)),
            _ => false,
        }
    } else {
        false
    };
    let restore_image = if let Action::RestoreReleaseImage(input) = &a {
        Some(apps::restore_image::prepare(db, &target, input)?)
    } else {
        None
    };
    let oci_health_check = if matches!(a, Action::Health(_) | Action::Refresh(_))
        && target.body["lifecycle"] == "Running"
    {
        apps::health::selected(db, &target)?
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
    let volume_rebind = if let Action::RebindRestoredVolume(input) = &a {
        Some(input.prepare(db, actor, &target)?)
    } else {
        None
    };
    let network_trial_inspection = if matches!(a, Action::InspectNetworkTrial(_)) {
        Some(node::network::trial::inspect(db, &target)?)
    } else {
        None
    };
    let service_resources_node_id = matches!(a, Action::ServiceResources(_))
        .then(|| db.meta("authority"))
        .transpose()?;
    let create_inspection = if matches!(a, Action::InspectOciCreate(_)) {
        Some(apps::create_recovery::inspection(db, &target)?)
    } else {
        None
    };
    let create_cleanup = if let Action::DiscardOciCreate(request) = &a {
        Some(apps::create_recovery::prepare(db, &target, request)?)
    } else {
        None
    };
    let mut w = Work {
        start_checkpoint,
        publication_checkpoint,
        launch,
        oci_health_check,
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
        attached_read: None,
        restore_image,
        restore_image_inspection,
        stop_cleanup,
        create_inspection,
        create_cleanup,
        volume_rebind,
        mount_observation_required,
        service_resources_node_id,
        network_trial_inspection,
    };
    node::reserve(db, &w)?;
    storage::reserve(db, &mut w)?;
    if matches!(w.action, Action::Backup(_)) {
        storage::publication::begin(db, &w)?;
    }
    if matches!(w.action, Action::NetworkTrial(_)) {
        node::network::trial::begin(db, &w)?;
    }
    if matches!(
        w.action,
        Action::Create { .. } | Action::Restore(_) | Action::CloneVolume(_)
    ) {
        storage::restore_publication::begin(db, &w)?;
    }
    if matches!(w.action, Action::Start(_)) {
        apps::create_recovery::begin(db, &w)?;
    }
    if let Some(prepared) = &w.create_cleanup {
        apps::create_recovery::begin_cleanup(db, &w, prepared)?;
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
    if let Action::RemoveEntry(request) = &w.action {
        storage::entry_retirement::begin(db, &w, &request.path)?;
    }
    if let Action::MakeDir(request) = &w.action {
        storage::directory_create::begin(db, &w, &request.path)?;
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
        Action::SubmitWorkItemEvidence(_) | Action::ListAssignedWork(_) => Ok(receipt.clone()),
        Action::ObserveNetworkProbe(_) => node::network::probe::state::publish(db, w, receipt),
        Action::ServiceObserve(_) => node::service_state::publish(db, w, receipt),
        Action::TrackBlockDevice(_) | Action::ObserveBlockDevice(_) => {
            node::block_device::publish(db, w, receipt)
        }
        Action::ServiceStart(_) | Action::ServiceStop(_) | Action::ServiceRestart(_) => {
            node::apply(db, w, receipt)
        }
        Action::RestoreReleaseImage(_) => apps::restore_image::settle(db, w, receipt),
        Action::RebindRestoredVolume(_) => apps::volume_rebind::settle(db, w, receipt),
        Action::ContinueOciStop(_) => apps::stop::settle_cleanup(db, w, receipt),
        Action::InspectOciCreate(_) => Ok(receipt.clone()),
        Action::DiscardOciCreate(_) => apps::create_recovery::settle_cleanup(db, w, receipt),
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
