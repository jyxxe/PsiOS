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
    #[serde(rename = "secret.mac")]
    Mac { secret_id: String, message: String },
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
    #[serde(rename = "backup.delete")]
    DeleteBackup(Target),
    #[serde(rename = "backup.restore")]
    Restore(Target),
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
            Self::Create { name } => label(name)?,
            Self::Read(f) => storage::filename(&f.name)?,
            Self::Write(f) => {
                storage::filename(&f.name)?;
                require(
                    f.content.len() <= 64 * 1024,
                    "QuotaExceeded",
                    "file import exceeds 64 KiB",
                )?
            }
            Self::Install(s) => apps::validate(s)?,
            Self::Mac { message, .. } => require(
                message.len() <= 8192,
                "QuotaExceeded",
                "MAC input exceeds limit",
            )?,
            _ => {}
        }
        Ok(a)
    }
    pub fn target(&self, db: &Db<'_>, actor: &str) -> Result<Doc> {
        let (id, version, kind) = match self {
            Self::Status {} => return db.get(policy::PROFILE),
            Self::Create { .. } => return db.get("local"),
            Self::Mac { secret_id, .. } => (secret_id, None, "Secret"),
            Self::Read(f) => (&f.id, Some(&f.expected_resource_version), "Volume"),
            Self::Write(f) => (&f.id, Some(&f.expected_resource_version), "Volume"),
            Self::Activate(t) | Self::Delete(t) | Self::Backup(t) => {
                (&t.id, Some(&t.expected_resource_version), "Volume")
            }
            Self::Restore(t) | Self::DeleteBackup(t) => {
                (&t.id, Some(&t.expected_resource_version), "Backup")
            }
            Self::Install(s) => match &s.volume {
                Some(t) => (&t.id, Some(&t.expected_resource_version), "Volume"),
                None => return db.get(policy::PROFILE),
            },
            Self::Start(t)
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
    pub volume: Option<Doc>,
}
impl Work {
    pub fn perform(&self) -> Result<Value> {
        if let Some(v) = &self.inline {
            return Ok(v.clone());
        }
        match &self.action {
            Action::Start(_)
            | Action::Stop(_)
            | Action::Uninstall(_)
            | Action::Install(_)
            | Action::Health(_)
            | Action::Logs(_) => apps::perform(self),
            _ => storage::perform(self),
        }
    }
}
fn permit(db: &Db<'_>, task: &Doc, run: &Doc, op: &Doc, effect: &Doc, target: &Doc) -> Result<()> {
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
            && permit.body["audience_resource_refs"] == json!([target.reference()])
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["authorization_epoch"] == db.meta("epoch")?
            && permit.body["recovery_incarnation"] == db.incarnation
            && permit.body["security_recovery_epoch"] == "1"
            && proof.body["claim_id"] == run.body["claim_id"]
            && proof.body["claim_epoch"] == run.body["claim_epoch"]
            && proof.body["permit_id"] == permit.id
            && proof.body["audience"] == target.reference()
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
    permit(db, &task, &run, &op, &effect, &target)?;
    let inline = match &a {
        Action::Status {} => Some(status(db)?),
        Action::Mac { secret_id, message } => Some(policy::mac(db, actor, secret_id, message)?),
        _ => None,
    };
    let volume = if target.kind == "Volume" {
        Some(target.clone())
    } else if target.kind == "Application" {
        target.spec["volume_id"]
            .as_str()
            .map(|id| db.of(id, "Volume"))
            .transpose()?
    } else {
        None
    };
    let data = if let Some(v) = &volume
        && !matches!(
            a,
            Action::Stop(_) | Action::Logs(_) | Action::Health(_) | Action::Uninstall(_)
        ) {
        policy::owned(actor, v)?;
        Some(storage::volume_dir(db.cfg, v)?)
    } else {
        None
    };
    let launch = if matches!(a, Action::Start(_)) {
        Some(apps::launch_spec(db, &target)?)
    } else {
        None
    };
    let w = Work {
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
    storage::reserve(db, &w)?;
    apps::preflight(db, &w)?;
    Ok(w)
}
pub fn apply(db: &Db<'_>, w: &Work, receipt: &Value) -> Result<Value> {
    match w.action {
        Action::Install(_)
        | Action::Start(_)
        | Action::Stop(_)
        | Action::Health(_)
        | Action::Logs(_)
        | Action::Uninstall(_) => apps::apply(db, w, receipt),
        _ => storage::apply(db, w, receipt),
    }
}
