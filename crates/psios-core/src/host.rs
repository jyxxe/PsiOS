//! Typed effect boundary. All filesystem roots, runtime flags and identity come from the authority.
use crate::{
    apps,
    config::Config,
    db::{Db, Doc},
    policy, storage, *,
};
use psios_api::VERSION;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    pub expected_resource_version: String,
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
    Processes(node::ProcessQuery),
    #[serde(rename = "service.inspect")]
    ServiceInspect(Target),
    #[serde(rename = "service.start")]
    ServiceStart(Target),
    #[serde(rename = "service.stop")]
    ServiceStop(Target),
    #[serde(rename = "secret.mac")]
    Mac { secret_id: String, message: String },
    #[serde(rename = "volume.files")]
    Files(storage::files::Page),
    #[serde(rename = "volume.usage")]
    VolumeUsage(storage::usage::Query),
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
    #[serde(rename = "application.stage_release")]
    Stage { target: Target, release: Install },
    #[serde(rename = "application.select_release")]
    Select { target: Target, release_ref: Value },
    #[serde(rename = "application.refresh")]
    Refresh(Target),
    #[serde(rename = "application.install")]
    Install(Install),
    #[serde(rename = "application.start")]
    Start(Target),
    #[serde(rename = "application.stop")]
    Stop(Target),
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
            Self::StartAgentSession(blueprint) => blueprint.validate()?,
            Self::Processes(p) => p.validate()?,
            Self::Interfaces(q) => q.validate()?,
            Self::Routes(q) => q.validate()?,
            Self::RouteLookup(q) => q.validate()?,
            Self::RoutingRules(q) => q.validate()?,
            Self::BlockLayout(q) => q.validate()?,
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
            Self::Mac { message, .. } => require(
                message.len() <= 8192,
                "QuotaExceeded",
                "MAC input exceeds limit",
            )?,
            _ => {}
        }
        storage::files::validate(&a)?;
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
            Self::StartAgentSession(blueprint) => resources.push(blueprint.skill(db)?.reference()),
            Self::Start(_) | Self::Stop(_) if primary.spec["volume_id"].is_string() => {
                let volume = db.of(primary.spec["volume_id"].as_str().unwrap(), "Volume")?;
                policy::owned(actor, &volume)?;
                resources.push(volume.reference());
            }
            Self::Restore(_) => resources.push(db.of("local", "StoragePool")?.reference()),
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
            Self::NetworkProbe(target) => return node::network::probe::target(db, target),
            Self::Processes(_)
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
                let d = db.of(&t.id, "HostService")?;
                d.cas(&t.expected_resource_version)?;
                require(
                    d.body["lifecycle"] == "Registered",
                    "PreconditionFailed",
                    "unregistered system unit",
                )?;
                return Ok(d);
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
            Self::InspectBackupPublication(t) => {
                (&t.id, Some(&t.expected_resource_version), "Operation")
            }
            Self::Install(s) => match &s.volume {
                Some(t) => (&t.id, Some(&t.expected_resource_version), "Volume"),
                None => return db.get(policy::PROFILE),
            },
            Self::Stage { target: t, .. }
            | Self::Select { target: t, .. }
            | Self::Refresh(t)
            | Self::Start(t)
            | Self::Stop(t)
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
        "queue_depth": db.c.query_row("SELECT count(*) FROM queue WHERE phase IN ('Queued','Claimed','Dispatched','WaitingApproval')",[],|r|r.get::<_,u32>(0))?
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
}

impl Work {
    pub(crate) fn checkpoint(&self, db: &Db<'_>, event: Checkpoint<'_>) -> Result<()> {
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
        match (&self.action, event) {
            (Action::Start(_), Checkpoint::OciStart { engine, container }) => {
                let evidence = json!({"start_operation_id":self.op.id, "effect_ref":self.effect.reference(),
                    "application_ref":target.reference(),"volume_ref":self.volume.as_ref().map(Doc::reference),
                    "engine_id":engine, "container_id":container});
                db.create(
                    "OCIStartCheckpoint",
                    &format!("oci-start-{}", self.op.id),
                    &t.owner,
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
            _ => Err(err("Fenced", "checkpoint does not match effect")),
        }
    }
    pub(crate) fn perform(
        &self,
        checkpoint: impl FnOnce(Checkpoint<'_>) -> Result<()>,
    ) -> Result<Value> {
        if let Some(v) = &self.inline {
            return Ok(v.clone());
        }
        match &self.action {
            Action::ServiceInspect(_)
            | Action::ServiceStart(_)
            | Action::ServiceStop(_)
            | Action::Inventory {}
            | Action::Pressure {}
            | Action::Processes(_)
            | Action::Interfaces(_)
            | Action::Routes(_)
            | Action::RouteLookup(_)
            | Action::RoutingRules(_)
            | Action::NetworkProbe(_)
            | Action::Mounts {}
            | Action::BlockLayout(_)
            | Action::Devices {} => node::perform(self),
            Action::Start(_) => apps::start(self, |engine, container| {
                checkpoint(Checkpoint::OciStart { engine, container })
            }),
            Action::Backup(_) => storage::backup(self, |receipt, device, inode| {
                checkpoint(Checkpoint::BackupPublication {
                    receipt,
                    device,
                    inode,
                })
            }),
            Action::Stage { .. }
            | Action::Select { .. }
            | Action::Refresh(_)
            | Action::Stop(_)
            | Action::Uninstall(_)
            | Action::Install(_)
            | Action::Health(_)
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
            && !expired(permit.body["not_after"].as_str().unwrap())?,
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
    let a = Action::parse(task.spec["task_type"].as_str().unwrap(), &input.body)?;
    let target = a.target(db, actor)?;
    let audience = a.audience(db, actor, &task.spec["delegation_ref"], &target)?;
    permit(db, &task, &run, &op, &effect, &audience)?;
    let inline = match &a {
        Action::Status {} => Some(status(db)?),
        Action::Mac { secret_id, message } => Some(policy::mac(db, actor, secret_id, message)?),
        _ => None,
    };
    let volume = if target.kind == "Volume" {
        Some(target.clone())
    } else if target.kind == "Application"
        && !matches!(
            a,
            Action::Stage { .. } | Action::Select { .. } | Action::Refresh(_)
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
    let publication_checkpoint = if matches!(a, Action::InspectBackupPublication(_)) {
        Some(storage::publication::pending(db, &target)?)
    } else {
        None
    };
    let data = if let Some(v) = &volume
        && (start_checkpoint.is_some()
            || !matches!(
                a,
                Action::Stop(_) | Action::Logs(_) | Action::Health(_) | Action::Uninstall(_)
            )) {
        policy::owned(actor, v)?;
        Some(storage::volume_dir(db.cfg, v)?)
    } else {
        None
    };
    let launch = if matches!(a, Action::Start(_)) || start_checkpoint.is_some() {
        Some(apps::launch_spec(db, &target)?)
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
    };
    node::reserve(db, &w)?;
    storage::reserve(db, &w)?;
    if matches!(w.action, Action::Backup(_)) {
        storage::publication::begin(db, &w)?;
    }
    apps::preflight(db, &w)?;
    Ok(w)
}
pub fn apply(db: &Db<'_>, w: &Work, receipt: &Value) -> Result<Value> {
    match w.action {
        Action::ServiceStart(_) | Action::ServiceStop(_) => node::apply(db, w, receipt),
        Action::Stage { .. }
        | Action::Select { .. }
        | Action::Refresh(_)
        | Action::Install(_)
        | Action::Start(_)
        | Action::Stop(_)
        | Action::Health(_)
        | Action::Logs(_)
        | Action::Uninstall(_) => apps::apply(db, w, receipt),
        _ => storage::apply(db, w, receipt),
    }
}
