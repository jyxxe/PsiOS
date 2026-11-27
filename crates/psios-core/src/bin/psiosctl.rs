use clap::{Parser, Subcommand, ValueEnum};
use psios_api::{
    Client, LocalInventoryModel, MAX_SECRET_BYTES, OciImageCaptureRequest, OciPublisherSignature,
    ResourceRef, SessionModel, StatusProvider, TaskListQuery, TaskOutputRequirement,
    WorkAdmissionMode, WorkItemPriority, parse,
};
use psios_core::{Config, DEFAULT_BACKUP_MAX_BYTES, MAX_BACKUP_BYTES};
use serde_json::{Value, json};
use std::{
    fs,
    io::{IsTerminal, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use zeroize::Zeroizing;

#[path = "shell/mod.rs"]
mod shell;

#[derive(Parser)]
#[command(version, about = "PsiOS local system client")]
struct Opts {
    /// Use the daemon configuration for the socket and owner credential paths.
    #[arg(long, global = true, conflicts_with = "state")]
    config: Option<PathBuf>,
    /// Use an isolated state directory and its sibling witness (same layout as psiosd).
    #[arg(long, global = true)]
    state: Option<PathBuf>,
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[arg(long, global = true)]
    token_file: Option<PathBuf>,
    #[arg(long, global = true)]
    service_uid: Option<u32>,
    #[command(subcommand)]
    cmd: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    Whoami,
    Capabilities,
    /// Serve a paired, read-only PsiOS Shell on this machine.
    Shell,
    Status {
        #[arg(long, default_value = "owner-delegation")]
        delegation: String,
    },
    /// JSON from a file or stdin (-). Credentials and secrets need not enter shell history.
    Call {
        method: String,
        #[arg(default_value = "-")]
        file: String,
    },
    /// Import, rotate or revoke an explicitly authorized local Secret.
    Secret {
        /// Reuse this client-generated key with the same input after an ambiguous response.
        #[arg(long, global = true)]
        idempotency_key: Option<String>,
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// Renew a completed interval schedule using a JSON request from a file or stdin.
    ScheduleRenew {
        #[arg(default_value = "-")]
        file: String,
    },
    Get {
        id: String,
    },
    List {
        kind: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    /// List exact Task revisions with an optional owner, tool and lifecycle filter.
    TaskList {
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        tool: Option<String>,
        /// Use the Task API spelling, for example WaitingApproval.
        #[arg(long)]
        lifecycle: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        cursor: Option<String>,
    },
    Watch {
        cursor: String,
        #[arg(long)]
        wait_ms: Option<u64>,
    },
    /// Read your confirmed event offset and revision.
    EventOffset {
        consumer: String,
    },
    /// Confirm a processed watch position against the previous confirmed cursor.
    EventAck {
        consumer: String,
        cursor: String,
        #[arg(long)]
        previous: Option<String>,
        /// Guard the advance with a revision from event-offset.
        #[arg(long, requires = "previous")]
        expected_revision: Option<String>,
    },
    /// List confirmed event consumer revisions.
    EventOffsets {
        /// Revoked principal ID for local-owner maintenance.
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        after: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Remove the exact confirmed event offset revision.
    EventOffsetDelete {
        consumer: String,
        #[arg(long)]
        expected_revision: String,
        /// Revoked principal ID for local-owner maintenance.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Read a host-enrolled spare network link.
    NetworkLink {
        id: String,
    },
    /// Request a local confirmation challenge for an exact trial.
    NetworkTrialConfirmation {
        operation: String,
        #[arg(long)]
        revision: String,
    },
    /// Confirm a trial with the challenge JSON from a file or stdin.
    NetworkTrialConfirm {
        operation: String,
        #[arg(long)]
        revision: String,
        #[arg(default_value = "-")]
        file: String,
    },
    /// Request immediate rollback of an exact active trial.
    NetworkTrialRollback {
        operation: String,
        #[arg(long)]
        revision: String,
    },
    /// Manage a local Project and its members.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage a Project milestone and its grouped WorkItems.
    Milestone {
        #[command(subcommand)]
        command: MilestoneCommand,
    },
    /// Manage an assigned WorkItem and review actual Task results.
    WorkItem {
        #[command(subcommand)]
        command: WorkItemCommand,
    },
    /// Read unacknowledged signals from a mailbox, optionally waiting for delivery.
    Inbox {
        id: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long)]
        wait_ms: Option<u64>,
    },
    Submit {
        tool: String,
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "owner-delegation")]
        delegation: String,
        /// JSON array of producing Task IDs and required output digests.
        #[arg(long, value_name = "FILE")]
        requires: Option<String>,
        #[arg(default_value = "-")]
        file: String,
    },
    Wait {
        id: String,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
    /// Show durable Task admission usage for this principal.
    TaskAdmission {
        /// Include this principal's retained Task preparation usage.
        #[arg(long)]
        include_preparations: bool,
    },
    /// Discard the exact prepared request in a JSON file or stdin.
    TaskDiscardPreparation {
        #[arg(default_value = "-")]
        file: String,
    },
    /// Read a Task's available result references and status.
    TaskResults {
        id: String,
    },
    /// Preview reclamation for exact Task references in a JSON file or stdin.
    TaskResultEvictionPlan {
        #[arg(default_value = "-")]
        file: String,
    },
    /// List unpinned result candidates using a JSON query from a file or stdin.
    TaskResultEvictionCandidates {
        #[arg(default_value = "-")]
        file: String,
    },
    /// Apply a result-eviction plan from a JSON file or stdin.
    TaskResultEvictionApply {
        #[arg(default_value = "-")]
        file: String,
    },
    /// Admit an explicitly delayed result-eviction plan from a file or stdin.
    TaskResultEvictionSchedule {
        #[arg(default_value = "-")]
        file: String,
    },
    /// Show a result-eviction schedule and its current revision.
    TaskResultEvictionScheduleGet {
        id: String,
    },
    /// Cancel a result-eviction schedule using its exact pending revision.
    TaskResultEvictionScheduleCancel {
        id: String,
        revision: String,
    },
    /// Verify and write stored bytes for an Artifact reference from a file or stdin.
    Artifact {
        #[arg(default_value = "-")]
        file: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: usize,
    },
    /// Stage, verify and publish a local file as an opaque Artifact.
    ArtifactUpload {
        source: PathBuf,
        /// Reuse the same key and source to resume a previous attempt.
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "application/octet-stream")]
        declared_media_type: String,
        #[arg(long, default_value_t = 600)]
        ttl_seconds: u32,
        /// Exact upload Delegation ResourceRef JSON file.
        #[arg(long)]
        delegation_ref: Option<String>,
    },
    /// Prepare a bounded key for the supplied upload metadata.
    ArtifactPrepareUpload {
        /// JSON upload metadata from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Show upload staging, retained bytes and durable receipt capacity.
    ArtifactUploadCapacity,
    /// Register an Ed25519 public key for an existing publisher principal.
    ArtifactPublisherKeyRegister {
        principal_id: String,
        /// Standard Base64 of exactly 32 raw public key bytes.
        #[arg(long)]
        public_key_base64: String,
    },
    /// Revoke a publisher key at its current resource revision.
    ArtifactPublisherKeyRevoke {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        reason: String,
    },
    /// Prepare a Task to install an uploaded OCI declaration.
    ArtifactPrepareInstall {
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON file containing key_ref and signature_base64.
        #[arg(long, value_name = "FILE")]
        signature: Option<PathBuf>,
        /// JSON Artifact ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Prepare a Task to stage an uploaded OCI declaration for an existing application.
    ArtifactPrepareRelease {
        /// JSON Application ResourceRef from a file.
        #[arg(long, value_name = "FILE")]
        application: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON file containing key_ref and signature_base64.
        #[arg(long, value_name = "FILE")]
        signature: Option<PathBuf>,
        /// JSON Artifact ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Prepare a single-copy local volume clone. Submit and approve it separately.
    VolumePrepareClone {
        #[arg(long)]
        name: String,
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON source Volume ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Prepare a Task to capture a local OCI image. Submit and approve it separately.
    ApplicationPrepareImageCapture {
        /// Full SHA-256 configuration digest of the local image.
        #[arg(long)]
        image_id: String,
        /// Identity of the configured local Docker Engine.
        #[arg(long)]
        engine_id: String,
        /// Maximum archive bytes for the approved Task, defaulting to 8 MiB.
        #[arg(long)]
        max_archive_bytes: Option<u32>,
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
    },
    /// Prepare the Task to restore a signed release image. Submit and approve it separately.
    ApplicationPrepareImageRestore {
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON with application_ref, release_ref, image_archive_ref, publisher_key_ref and expected_engine_id.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Prepare a read-only inspection of an unresolved image restore operation.
    ApplicationPrepareImageRestoreInspection {
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON ResourceRef for the original Operation, from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Prepare removal of a verified stopped OCI container. Submit and approve separately.
    ApplicationPrepareStopContinuation {
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
        #[arg(long)]
        deadline_at: Option<String>,
        /// JSON with application_ref, operation_ref and evidence_ref, from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// List the selected, staged, previous and retired releases of an application.
    ApplicationReleases {
        id: String,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Repair the Artifact from a previously exported, verified JSON file.
    ArtifactRepair {
        source: PathBuf,
        /// JSON Artifact ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: u64,
    },
    /// Verify and record the availability of a settled Task's pinned results.
    TaskVerifyResults {
        id: String,
        /// Current task_revision returned by Task.Get.
        #[arg(long)]
        revision: String,
        /// Cap bytes read from SQLite (default 8 MiB, maximum 64 MiB).
        #[arg(long)]
        max_bytes: Option<u64>,
    },
    /// Release a settled read-only Task's verified unavailable results.
    TaskAbandonResults {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        reason: String,
    },
    /// Release a damaged upload by its immutable Artifact reference.
    ArtifactAbandonUpload {
        #[arg(long)]
        reason: String,
        /// JSON Artifact ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Export a verified backup to a new directory under a private parent.
    BackupExport {
        #[arg(long)]
        output: PathBuf,
        /// JSON Backup ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
        /// Maximum total payload bytes to download.
        #[arg(long, default_value_t = DEFAULT_BACKUP_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=MAX_BACKUP_BYTES))]
        max_bytes: u64,
        /// Keep and resume a private staged export for this output path and reference.
        #[arg(long)]
        resume: bool,
    },
    Agent {
        #[arg(long)]
        key: String,
        #[arg(long)]
        delegation: String,
    },
    /// Process one durable model turn with the local node inventory provider.
    ModelStep {
        session: String,
        #[arg(long)]
        key: String,
        /// Existing private directory for a pending decision or failure report.
        #[arg(long)]
        decision_dir: Option<PathBuf>,
        /// Use the configured local chat-completions server for an inventory Skill.
        #[arg(long)]
        local_model: Option<String>,
    },
    /// Serve opted-in sessions for a pinned model using a private journal.
    ModelWorker {
        /// Local file containing the exact ResourceRef returned by Model.Register.
        model: String,
        #[arg(long)]
        decision_dir: PathBuf,
        #[arg(long)]
        local_model: Option<String>,
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=10_000))]
        wait_ms: u64,
        /// Process at most one model turn and print its receipt or an empty queue.
        #[arg(long)]
        once: bool,
    },
    /// Continue a Session on a model already authorized when the Session opened.
    SessionUseFallback {
        id: String,
        #[arg(long)]
        revision: String,
        /// JSON Model ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
    },
    /// Pause a Session immediately or when its current Task settles.
    SessionPause {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        reason: String,
    },
    /// Resume an operator-paused Session or recoverable model or tool content failure.
    SessionResume {
        id: String,
        #[arg(long)]
        revision: String,
    },
    /// Show model calls, lease-time charges, and provider-reported tokens for a Session.
    SessionModelUsage {
        id: String,
    },
    /// Wait for a Session to pause, finish, or advance past a selected revision.
    SessionWait {
        id: String,
        #[arg(long)]
        after_version: Option<String>,
        #[arg(long, default_value_t = 5_000, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        wait_ms: u32,
    },
    Node,
    /// Read the state of a specifically authorized enrolled block device.
    BlockDevice {
        id: String,
    },
    /// Retire an exact block enrollment revision as the local owner.
    BlockDeviceRetire {
        id: String,
        #[arg(long)]
        revision: String,
    },
    /// Show the local work admission mode and accepted work counts.
    NodeWorkAdmission,
    /// Change the exact local work admission revision as the local owner.
    NodeWorkAdmissionSet {
        revision: String,
        #[arg(value_enum)]
        mode: WorkAdmissionCommandMode,
        reason: String,
    },
    /// Inspect exact queued work, holds and leases at the observed admission revision.
    NodeWorkAdmissionBlockers {
        revision: String,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Observe Core work at a draining revision, optionally waiting for it to clear.
    NodeWorkAdmissionQuiescence {
        revision: String,
        #[arg(long)]
        limit: Option<u32>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=10_000))]
        wait_ms: Option<u32>,
    },
    Validate {
        contract: String,
        #[arg(default_value = "-")]
        file: String,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum WorkAdmissionCommandMode {
    Open,
    Draining,
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Import UTF-8 bytes from a private file or piped standard input.
    Put {
        name: String,
        #[arg(long, default_value = "-")]
        value_file: PathBuf,
    },
    /// Replace one exact active Secret revision.
    Rotate {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long, default_value = "-")]
        value_file: PathBuf,
    },
    /// Revoke one exact Secret revision.
    Revoke {
        id: String,
        #[arg(long)]
        revision: String,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Create a Project using a JSON declaration from a file or stdin.
    Create {
        #[arg(default_value = "-")]
        file: String,
    },
    Get {
        id: String,
    },
    /// List projects for which you have active membership and an exact read grant.
    List {
        #[arg(long)]
        after_id: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Set a member's role by revision. Supply a null role to remove a member.
    SetMember {
        #[arg(default_value = "-")]
        file: String,
    },
}

#[derive(Subcommand)]
enum MilestoneCommand {
    /// Create a milestone from a JSON declaration on stdin or in a file.
    Create {
        #[arg(default_value = "-")]
        file: String,
    },
    Get {
        id: String,
    },
    List {
        project_id: String,
        #[arg(long)]
        after_id: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Accept the milestone once all grouped WorkItems are closed.
    Close {
        id: String,
        #[arg(long)]
        revision: String,
    },
    /// End the milestone without accepting its WorkItems.
    Abandon {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum WorkItemCommand {
    /// Create an assigned WorkItem using a JSON declaration from a file or stdin.
    Create {
        #[arg(default_value = "-")]
        file: String,
    },
    Get {
        id: String,
    },
    /// Read a fixed ascending page of immutable WorkItem revisions.
    History {
        id: String,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Read a historical revision from its exact JSON reference on stdin or in a file.
    Revision {
        #[arg(default_value = "-")]
        file: String,
    },
    /// List the direct children of this work item.
    Children {
        id: String,
        #[arg(long)]
        after_id: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// List work assigned to you using a JSON Project query on stdin or in a file.
    Assigned {
        #[arg(default_value = "-")]
        file: String,
    },
    List {
        project_id: String,
        #[arg(long)]
        after_id: Option<String>,
        #[arg(long)]
        limit: Option<u32>,
    },
    /// Submit exact Task and Artifact references from a JSON file or stdin.
    SubmitEvidence {
        #[arg(default_value = "-")]
        file: String,
    },
    Review {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(value_enum)]
        decision: ReviewDecision,
        /// Exact review Signal reference from a JSON file or stdin.
        #[arg(long)]
        request_signal: Option<String>,
    },
    /// Send the current pending WorkItem reference from a JSON file or stdin.
    RequestReview {
        mailbox: String,
        #[arg(long)]
        key: String,
        #[arg(default_value = "-")]
        file: String,
    },
    /// Fetch a committed reply for an exact review Signal reference.
    ReviewReceipt {
        #[arg(default_value = "-")]
        file: String,
    },
    Close {
        id: String,
        #[arg(long)]
        revision: String,
    },
    /// Assign an unsubmitted work item to a scoped contributor.
    Reassign {
        id: String,
        #[arg(long)]
        revision: String,
        assignee: String,
    },
    /// Change the priority of a ready work item at its current revision.
    Reprioritize {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(value_enum)]
        priority: PriorityInput,
    },
    /// End open work with a recorded reason.
    Abandon {
        id: String,
        #[arg(long)]
        revision: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ReviewDecision {
    Accept,
    Reject,
}

#[derive(Clone, Copy, ValueEnum)]
enum PriorityInput {
    Low,
    Normal,
    High,
    Urgent,
}

impl From<PriorityInput> for WorkItemPriority {
    fn from(priority: PriorityInput) -> Self {
        match priority {
            PriorityInput::Low => Self::Low,
            PriorityInput::Normal => Self::Normal,
            PriorityInput::High => Self::High,
            PriorityInput::Urgent => Self::Urgent,
        }
    }
}

fn input(path: &str) -> Result<Value, Box<dyn std::error::Error>> {
    if path == "-" {
        read_json(std::io::stdin())
    } else {
        read_json(fs::File::open(path)?)
    }
}

fn read_json(reader: impl Read) -> Result<Value, Box<dyn std::error::Error>> {
    let mut bytes = vec![];
    reader
        .take(psios_api::MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > psios_api::MAX_FRAME {
        return Err("input exceeds frame limit".into());
    }
    Ok(parse(&bytes)?)
}

fn secret_input(path: &Path) -> Result<Zeroizing<String>, Box<dyn std::error::Error>> {
    let limit = MAX_SECRET_BYTES as u64 + 1;
    let mut bytes = Zeroizing::new(Vec::new());
    if path == Path::new("-") {
        let input = std::io::stdin();
        if input.is_terminal() {
            return Err("secret input requires a pipe or a private value file".into());
        }
        input.take(limit).read_to_end(&mut bytes)?;
    } else {
        use rustix::fs::OFlags;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err("expected a private regular secret value file owned by the caller".into());
        }
        file.take(limit).read_to_end(&mut bytes)?;
    }
    if !(1..=MAX_SECRET_BYTES).contains(&bytes.len()) {
        return Err("secret size outside limit".into());
    }
    let value = std::str::from_utf8(&bytes).map_err(|_| "secret input must be valid UTF-8")?;
    Ok(Zeroizing::new(value.to_owned()))
}

fn detached_signature(path: &Path) -> Result<OciPublisherSignature, Box<dyn std::error::Error>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err("expected a regular publisher signature file".into());
    }
    serde_json::from_value(read_json(file)?)
        .map_err(|_| "expected publisher signature fields key_ref and signature_base64".into())
}

fn model_provider(
    path: Option<&str>,
    expected_model: Option<&ResourceRef>,
) -> Result<Box<dyn SessionModel>, Box<dyn std::error::Error>> {
    if let Some(path) = path {
        let model: LocalInventoryModel = serde_json::from_value(input(path)?)?;
        model.validate()?;
        if expected_model.is_some_and(|reference| model.model_ref != *reference) {
            return Err(
                "local inference configuration must name the worker's Model revision".into(),
            );
        }
        Ok(Box::new(model))
    } else {
        Ok(Box::new(psios_api::NodeSurvey))
    }
}

fn model_worker(
    client: &Client,
    model: &ResourceRef,
    provider: &dyn SessionModel,
    dir: &Path,
    wait: Duration,
    once: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    while !stop.load(Ordering::Acquire) {
        let key = psios_api::id("provider");
        let result = client.model_next_durable(model, &key, provider, dir, wait);
        if once {
            println!("{}", result?.unwrap_or_else(|| json!({"available":false})));
            break;
        }
        match result {
            Ok(Some(receipt)) => writeln!(std::io::stdout(), "{receipt}")?,
            Ok(None) => {}
            Err(e)
                if e.retryable
                    || matches!(
                        e.code.as_str(),
                        "Fenced" | "OutcomeUnknown" | "DependencyUnavailable" | "DeadlineExceeded"
                    ) =>
            {
                eprintln!("model worker: {}", e.code);
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let o = Opts::parse();
    if let Cmd::Validate { contract, file } = &o.cmd {
        psios_api::contract(contract, &input(file)?)?;
        println!("valid");
        return Ok(());
    }
    // A caller that supplies a standalone socket is independent of the installed site.
    let cfg = if o.socket.is_some() && o.config.is_none() && o.state.is_none() {
        Config::default()
    } else {
        Config::resolve(o.config.as_deref(), o.state.as_deref())?
    };
    let socket = o.socket.unwrap_or(cfg.socket);
    let token_file = o
        .token_file
        .unwrap_or_else(|| cfg.witness_dir.join("owner.token"));
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(token_file)?;
    let m = f.metadata()?;
    if !m.is_file() || m.mode() & 0o077 != 0 || m.len() > 128 {
        return Err("expected a private credential file".into());
    }
    let mut token = String::new();
    f.take(129).read_to_string(&mut token)?;
    let c = Client::new(socket, token.trim()).with_service_uid(
        o.service_uid
            .unwrap_or_else(|| rustix::process::geteuid().as_raw()),
    );
    let v = match o.cmd {
        Cmd::Shell => return shell::run(c),
        Cmd::ModelStep {
            session,
            key,
            decision_dir,
            local_model,
        } => {
            let provider = model_provider(local_model.as_deref(), None)?;
            match decision_dir {
                Some(dir) => c.model_turn_durable(&session, &key, &*provider, &dir)?,
                None => c.model_turn(&session, &key, &*provider)?,
            }
        }
        Cmd::ModelWorker {
            model,
            decision_dir,
            local_model,
            wait_ms,
            once,
        } => {
            let reference = serde_json::from_value(input(&model)?)?;
            let provider = model_provider(local_model.as_deref(), Some(&reference))?;
            return model_worker(
                &c,
                &reference,
                &*provider,
                &decision_dir,
                Duration::from_millis(wait_ms),
                once,
            );
        }
        Cmd::Node => c.call("Node.Get", json!({}))?,
        Cmd::BlockDevice { id } => c.block_device(&id)?,
        Cmd::BlockDeviceRetire { id, revision } => c.retire_block_device(&id, &revision)?,
        Cmd::NodeWorkAdmission => serde_json::to_value(c.node_work_admission()?)?,
        Cmd::NodeWorkAdmissionSet {
            revision,
            mode,
            reason,
        } => {
            let mode = match mode {
                WorkAdmissionCommandMode::Open => WorkAdmissionMode::Open,
                WorkAdmissionCommandMode::Draining => WorkAdmissionMode::Draining,
            };
            serde_json::to_value(c.set_node_work_admission(&revision, mode, &reason)?)?
        }
        Cmd::NodeWorkAdmissionBlockers { revision, limit } => {
            serde_json::to_value(c.node_work_admission_blockers(&revision, limit)?)?
        }
        Cmd::NodeWorkAdmissionQuiescence {
            revision,
            limit,
            wait_ms,
        } => serde_json::to_value(match wait_ms {
            Some(timeout) => c.wait_node_work_quiescence(&revision, limit, timeout)?,
            None => c.node_work_quiescence(&revision, limit)?,
        })?,
        Cmd::Whoami => c.call("Principal.WhoAmI", json!({}))?,
        Cmd::Capabilities => c.call("Capability.Discover", json!({}))?,
        Cmd::Status { delegation } => c.call(
            "Tool.Call",
            json!({"tool": "system.status", "input": {}, "delegation_id": delegation}),
        )?,
        Cmd::Call { method, file } => c.call(&method, input(&file)?)?,
        Cmd::Secret {
            idempotency_key,
            command,
        } => serde_json::to_value(match command {
            SecretCommand::Put { name, value_file } => c.put_secret_with_key(
                &name,
                &secret_input(&value_file)?,
                idempotency_key.as_deref(),
            )?,
            SecretCommand::Rotate {
                id,
                revision,
                value_file,
            } => c.rotate_secret_with_key(
                &id,
                &revision,
                &secret_input(&value_file)?,
                idempotency_key.as_deref(),
            )?,
            SecretCommand::Revoke { id, revision } => {
                c.revoke_secret_with_key(&id, &revision, idempotency_key.as_deref())?
            }
        })?,
        Cmd::ScheduleRenew { file } => c.renew_schedule(&serde_json::from_value(input(&file)?)?)?,
        Cmd::Get { id } => c.call("Resource.Get", json!({"id": id}))?,
        Cmd::List { kind, id } => {
            c.resource_snapshot(kind.as_deref(), id.as_deref(), None, None)?
        }
        Cmd::TaskList {
            owner,
            tool,
            lifecycle,
            limit,
            cursor,
        } => serde_json::to_value(
            c.task_list(&TaskListQuery {
                owner_principal_id: owner,
                task_type: tool,
                lifecycle: lifecycle
                    .map(|value| serde_json::from_value(Value::String(value)))
                    .transpose()?,
                limit,
                cursor,
            })?,
        )?,
        Cmd::Watch { cursor, wait_ms } => c.watch_resources(&cursor, None, wait_ms)?,
        Cmd::EventOffset { consumer } => c.event_offset(&consumer)?,
        Cmd::EventAck {
            consumer,
            cursor,
            previous,
            expected_revision,
        } => match expected_revision {
            Some(revision) => c.acknowledge_events_checked(
                &consumer,
                &cursor,
                previous.as_deref().ok_or("missing previous event cursor")?,
                &revision,
            )?,
            None => c.acknowledge_events(&consumer, &cursor, previous.as_deref())?,
        },
        Cmd::EventOffsets {
            owner,
            after,
            limit,
        } => c.event_offsets(owner.as_deref(), after.as_deref(), limit)?,
        Cmd::EventOffsetDelete {
            consumer,
            expected_revision,
            owner,
        } => c.delete_event_offset(&consumer, &expected_revision, owner.as_deref())?,
        Cmd::NetworkLink { id } => c.network_link(&id)?,
        Cmd::NetworkTrialConfirmation {
            operation,
            revision,
        } => c.network_trial_confirmation(&operation, &revision)?,
        Cmd::NetworkTrialConfirm {
            operation,
            revision,
            file,
        } => {
            let challenge = input(&file)?;
            let token = challenge["confirmation_token"]
                .as_str()
                .ok_or("missing network confirmation token")?;
            c.confirm_network_trial(&operation, &revision, token)?
        }
        Cmd::NetworkTrialRollback {
            operation,
            revision,
        } => c.rollback_network_trial(&operation, &revision)?,
        Cmd::Project { command } => {
            let record = match command {
                ProjectCommand::Create { file } => {
                    c.create_project(&serde_json::from_value(input(&file)?)?)?
                }
                ProjectCommand::Get { id } => c.project(&id)?,
                ProjectCommand::List { after_id, limit } => {
                    let page = c.projects(after_id.as_deref(), limit)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                ProjectCommand::SetMember { file } => {
                    c.set_project_member(&serde_json::from_value(input(&file)?)?)?
                }
            };
            serde_json::to_value(record)?
        }
        Cmd::Milestone { command } => {
            let record = match command {
                MilestoneCommand::Create { file } => {
                    c.create_milestone(&serde_json::from_value(input(&file)?)?)?
                }
                MilestoneCommand::Get { id } => c.milestone(&id)?,
                MilestoneCommand::List {
                    project_id,
                    after_id,
                    limit,
                } => {
                    let page = c.milestones(&project_id, after_id.as_deref(), limit)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                MilestoneCommand::Close { id, revision } => c.close_milestone(&id, &revision)?,
                MilestoneCommand::Abandon {
                    id,
                    revision,
                    reason,
                } => c.abandon_milestone(&id, &revision, &reason)?,
            };
            serde_json::to_value(record)?
        }
        Cmd::WorkItem { command } => {
            let record = match command {
                WorkItemCommand::Create { file } => {
                    c.create_work_item(&serde_json::from_value(input(&file)?)?)?
                }
                WorkItemCommand::Get { id } => c.work_item(&id)?,
                WorkItemCommand::History { id, cursor, limit } => {
                    let page = c.work_item_history(&id, cursor.as_deref(), limit)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                WorkItemCommand::Revision { file } => {
                    c.work_item_revision(&serde_json::from_value(input(&file)?)?)?
                }
                WorkItemCommand::Children {
                    id,
                    after_id,
                    limit,
                } => {
                    let page = c.work_item_children(&id, after_id.as_deref(), limit)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                WorkItemCommand::Assigned { file } => {
                    let page = c.assigned_work(&serde_json::from_value(input(&file)?)?)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                WorkItemCommand::List {
                    project_id,
                    after_id,
                    limit,
                } => {
                    let page = c.work_items(&project_id, after_id.as_deref(), limit)?;
                    println!("{}", serde_json::to_string_pretty(&page)?);
                    return Ok(());
                }
                WorkItemCommand::SubmitEvidence { file } => {
                    c.submit_work_item_evidence(&serde_json::from_value(input(&file)?)?)?
                }
                WorkItemCommand::Review {
                    id,
                    revision,
                    decision,
                    request_signal,
                } => {
                    let accept = matches!(decision, ReviewDecision::Accept);
                    if let Some(file) = request_signal {
                        let signal = serde_json::from_value(input(&file)?)?;
                        let reply =
                            c.review_work_item_from_signal(&id, &revision, accept, &signal)?;
                        println!("{}", serde_json::to_string_pretty(&reply)?);
                        return Ok(());
                    }
                    c.review_work_item(&id, &revision, accept)?
                }
                WorkItemCommand::RequestReview { mailbox, key, file } => {
                    let reply = c.request_work_item_review(
                        &mailbox,
                        &key,
                        &serde_json::from_value(input(&file)?)?,
                    )?;
                    println!("{}", serde_json::to_string_pretty(&reply)?);
                    return Ok(());
                }
                WorkItemCommand::ReviewReceipt { file } => {
                    let reply =
                        c.work_item_review_receipt(&serde_json::from_value(input(&file)?)?)?;
                    println!("{}", serde_json::to_string_pretty(&reply)?);
                    return Ok(());
                }
                WorkItemCommand::Close { id, revision } => c.close_work_item(&id, &revision)?,
                WorkItemCommand::Reassign {
                    id,
                    revision,
                    assignee,
                } => c.reassign_work_item(&id, &revision, &assignee)?,
                WorkItemCommand::Reprioritize {
                    id,
                    revision,
                    priority,
                } => c.reprioritize_work_item(&id, &revision, priority.into())?,
                WorkItemCommand::Abandon {
                    id,
                    revision,
                    reason,
                } => c.abandon_work_item(&id, &revision, &reason)?,
            };
            serde_json::to_value(record)?
        }
        Cmd::Inbox { id, limit, wait_ms } => match wait_ms {
            Some(timeout_ms) => c.call(
                "Mailbox.Wait",
                json!({"id":id,"limit":limit,"timeout_ms":timeout_ms}),
            )?,
            None => c.call("Mailbox.Read", json!({"id":id,"limit":limit}))?,
        },
        Cmd::Submit {
            tool,
            key,
            delegation,
            requires,
            file,
        } => {
            let requirements: Vec<TaskOutputRequirement> = match requires {
                Some(path) => serde_json::from_value(input(&path)?)?,
                None => Vec::new(),
            };
            c.submit_with_requirements(&tool, input(&file)?, &key, &delegation, &requirements)?
        }
        Cmd::Wait { id, seconds } => c.wait(&id, Duration::from_secs(seconds))?,
        Cmd::TaskAdmission {
            include_preparations,
        } => {
            if include_preparations {
                c.task_admission_with_preparations()?
            } else {
                c.task_admission()?
            }
        }
        Cmd::TaskDiscardPreparation { file } => c.discard_task_preparation(input(&file)?)?,
        Cmd::SessionUseFallback { id, revision, file } => {
            c.use_session_fallback(&id, &revision, &serde_json::from_value(input(&file)?)?)?
        }
        Cmd::SessionPause {
            id,
            revision,
            reason,
        } => c.pause_session(&id, &revision, &reason)?,
        Cmd::SessionResume { id, revision } => c.resume_session(&id, &revision)?,
        Cmd::SessionModelUsage { id } => serde_json::to_value(c.session_model_usage(&id)?)?,
        Cmd::SessionWait {
            id,
            after_version,
            wait_ms,
        } => c.wait_session(&id, after_version.as_deref(), wait_ms)?,
        Cmd::TaskResults { id } => c.task_results(&id)?,
        Cmd::TaskResultEvictionPlan { file } => serde_json::to_value(
            c.prepare_result_eviction_batch(&serde_json::from_value(input(&file)?)?)?,
        )?,
        Cmd::TaskResultEvictionCandidates { file } => serde_json::to_value(
            c.result_eviction_candidates(&serde_json::from_value(input(&file)?)?)?,
        )?,
        Cmd::TaskResultEvictionApply { file } => {
            let plan = input(&file)?;
            let token = plan["plan_token"]
                .as_str()
                .ok_or("missing eviction plan token")?;
            serde_json::to_value(c.apply_result_eviction_batch(token)?)?
        }
        Cmd::TaskResultEvictionSchedule { file } => {
            let plan = input(&file)?;
            let token = plan["plan_token"]
                .as_str()
                .ok_or("missing eviction plan token")?;
            serde_json::to_value(c.schedule_result_eviction_batch(token)?)?
        }
        Cmd::TaskResultEvictionScheduleGet { id } => {
            serde_json::to_value(c.get_result_eviction_schedule(&id)?)?
        }
        Cmd::TaskResultEvictionScheduleCancel { id, revision } => {
            serde_json::to_value(c.cancel_result_eviction_schedule(&id, &revision)?)?
        }
        Cmd::Artifact { file, max_bytes } => {
            let reference = serde_json::from_value(input(&file)?)?;
            std::io::stdout().write_all(&c.artifact_bytes(&reference, max_bytes)?)?;
            return Ok(());
        }
        Cmd::ArtifactUpload {
            source,
            key,
            declared_media_type,
            ttl_seconds,
            delegation_ref,
        } => {
            let mut source = fs::File::open(source)?;
            let receipt = if let Some(path) = delegation_ref {
                let reference = serde_json::from_value(input(&path)?)?;
                c.upload_artifact_with_delegation(
                    &key,
                    &declared_media_type,
                    ttl_seconds,
                    &reference,
                    &mut source,
                )?
            } else {
                c.upload_artifact(&key, &declared_media_type, ttl_seconds, &mut source)?
            };
            serde_json::to_value(receipt)?
        }
        Cmd::ArtifactUploadCapacity => serde_json::to_value(c.artifact_upload_capacity()?)?,
        Cmd::ArtifactPrepareUpload { file } => serde_json::to_value(
            c.prepare_artifact_stage(&serde_json::from_value(input(&file)?)?)?,
        )?,
        Cmd::ArtifactPublisherKeyRegister {
            principal_id,
            public_key_base64,
        } => serde_json::to_value(
            c.register_artifact_publisher_key(&principal_id, &public_key_base64)?,
        )?,
        Cmd::ArtifactPublisherKeyRevoke {
            id,
            revision,
            reason,
        } => serde_json::to_value(c.revoke_artifact_publisher_key(&id, &revision, &reason)?)?,
        Cmd::ArtifactPrepareInstall {
            key,
            delegation,
            deadline_at,
            signature,
            file,
        } => {
            let signature = signature.as_deref().map(detached_signature).transpose()?;
            c.prepare_artifact_install(
                &serde_json::from_value(input(&file)?)?,
                &key,
                &delegation,
                deadline_at.as_deref(),
                signature.as_ref(),
            )?
        }
        Cmd::ArtifactPrepareRelease {
            application,
            key,
            delegation,
            deadline_at,
            signature,
            file,
        } => {
            let signature = signature.as_deref().map(detached_signature).transpose()?;
            c.prepare_artifact_release_with_signature(
                &serde_json::from_value(input(&application)?)?,
                &serde_json::from_value(input(&file)?)?,
                &key,
                &delegation,
                deadline_at.as_deref(),
                signature.as_ref(),
            )?
        }
        Cmd::VolumePrepareClone {
            name,
            key,
            delegation,
            deadline_at,
            file,
        } => c.prepare_volume_clone(
            &serde_json::from_value(input(&file)?)?,
            &name,
            &key,
            &delegation,
            deadline_at.as_deref(),
        )?,
        Cmd::ApplicationPrepareImageCapture {
            image_id,
            engine_id,
            max_archive_bytes,
            key,
            delegation,
            deadline_at,
        } => c.prepare_oci_image_capture(
            &OciImageCaptureRequest {
                image_id,
                expected_engine_id: engine_id,
                max_archive_bytes,
            },
            &key,
            &delegation,
            deadline_at.as_deref(),
        )?,
        Cmd::ApplicationPrepareImageRestore {
            key,
            delegation,
            deadline_at,
            file,
        } => c.prepare_release_image_restore(
            &serde_json::from_value(input(&file)?)?,
            &key,
            &delegation,
            deadline_at.as_deref(),
        )?,
        Cmd::ApplicationPrepareImageRestoreInspection {
            key,
            delegation,
            deadline_at,
            file,
        } => c.prepare_release_image_restore_inspection(
            &serde_json::from_value(input(&file)?)?,
            &key,
            &delegation,
            deadline_at.as_deref(),
        )?,
        Cmd::ApplicationPrepareStopContinuation {
            key,
            delegation,
            deadline_at,
            file,
        } => c.prepare_oci_stop_continuation(
            &serde_json::from_value(input(&file)?)?,
            &key,
            &delegation,
            deadline_at.as_deref(),
        )?,
        Cmd::ApplicationReleases { id, cursor, limit } => {
            c.application_releases(&id, cursor.as_deref(), limit)?
        }
        Cmd::ArtifactRepair {
            source,
            file,
            max_bytes,
        } => {
            let reference = serde_json::from_value(input(&file)?)?;
            let mut source = fs::File::open(source)?;
            serde_json::to_value(c.repair_artifact(&reference, &mut source, max_bytes)?)?
        }
        Cmd::TaskVerifyResults {
            id,
            revision,
            max_bytes,
        } => c.verify_task_results(&id, &revision, max_bytes)?,
        Cmd::TaskAbandonResults {
            id,
            revision,
            reason,
        } => c.abandon_task_results(&id, &revision, &reason)?,
        Cmd::ArtifactAbandonUpload { reason, file } => {
            c.abandon_uploaded_artifact(&serde_json::from_value(input(&file)?)?, &reason)?
        }
        Cmd::BackupExport {
            output,
            file,
            max_bytes,
            resume,
        } => {
            let reference = serde_json::from_value(input(&file)?)?;
            if resume {
                psios_core::resume_backup_with_limit(&c, &reference, &output, max_bytes)?
            } else {
                psios_core::download_backup_with_limit(&c, &reference, &output, max_bytes)?
            }
        }
        Cmd::Agent { key, delegation } => c.execute_agent(&StatusProvider, &key, &delegation)?,
        Cmd::Validate { .. } => unreachable!(),
    };
    println!("{}", serde_json::to_string_pretty(&v)?);
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detached_signatures_require_bounded_regular_json() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let key = json!({"resource_id":"oci-key-test","version":"r1","digest":psios_api::sha256(b"test")});
        let valid = json!({"key_ref":key,"signature_base64":"opaque"});
        fs::write(file.path(), serde_json::to_vec(&valid).unwrap()).unwrap();
        let signature = detached_signature(file.path()).unwrap();
        assert_eq!(serde_json::to_value(signature).unwrap(), valid);

        fs::write(
            file.path(),
            format!(
                r#"{{"key_ref":{key},"signature_base64":"first","signature_base64":"second"}}"#
            ),
        )
        .unwrap();
        assert!(detached_signature(file.path()).is_err());

        file.as_file()
            .set_len(psios_api::MAX_FRAME as u64 + 1)
            .unwrap();
        assert_eq!(
            detached_signature(file.path()).unwrap_err().to_string(),
            "input exceeds frame limit"
        );
        assert!(detached_signature(Path::new("/dev/null")).is_err());
    }
}
