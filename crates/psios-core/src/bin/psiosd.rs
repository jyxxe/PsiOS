use clap::{Parser, Subcommand};
use psios_core::{
    Config, DEFAULT_BACKUP_MAX_BYTES, MAX_BACKUP_BYTES, Store, inspect_detached_backup_with_limit,
    server,
};
use std::{
    fs::File,
    io::{Read, Write, stdin, stdout},
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

#[derive(Parser)]
#[command(version, about = "PsiOS single-node system authority")]
struct Opts {
    #[arg(long, global = true, conflicts_with = "config")]
    state: Option<PathBuf>,
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}
#[derive(Subcommand)]
enum Cmd {
    Init,
    Serve,
    /// Open the authority normally, including supported migrations and witness finalization.
    Check,
    /// Verify a stopped authority on a private scratch copy, preserving the source.
    Verify {
        /// Maximum combined database and journal input size in MiB.
        #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
        max_copy_mib: u32,
        /// Parent directory for the temporary verification copy (defaults to TMPDIR).
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
        /// Run and verify the normal migration on that disposable copy.
        #[arg(long)]
        rehearse_migration: bool,
    },
    /// Inspect a stopped authority and prepare an exact SQLite compaction plan.
    PlanStateCompaction(CompactionOptions),
    /// Reclaim free SQLite pages on a stopped authority using an exact plan.
    CompactState {
        /// JSON emitted by plan-state-compaction, or standard input (-).
        #[arg(long, default_value = "-")]
        plan_file: PathBuf,
        #[command(flatten)]
        options: CompactionOptions,
    },
    /// Inventory exact retained legacy input references using a verified stopped-authority copy.
    InspectRetainedInputs {
        /// JSON array of exact legacy Input references, or standard input (-).
        #[arg(long, default_value = "-")]
        references_file: PathBuf,
        /// Maximum combined SQLite and journal copy size in MiB.
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..=256))]
        max_copy_mib: u32,
        /// Maximum scanned stored text and blob bytes in MiB.
        #[arg(long, default_value_t = 128, value_parser = clap::value_parser!(u32).range(1..=512))]
        max_scan_mib: u32,
        /// Parent for the private verification copy (defaults to TMPDIR).
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
    },
    /// Capture a verified database checkpoint from a stopped authority.
    Checkpoint {
        /// New checkpoint directory under a private, operator-owned parent.
        #[arg(long)]
        output: PathBuf,
        /// Maximum combined database and journal size in MiB.
        #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
        max_copy_mib: u32,
    },
    /// Verify a checkpoint without a running authority or its credentials.
    VerifyCheckpoint(CheckpointInput),
    /// Write the original bytes of a pinned artifact from a verified checkpoint.
    CheckpointArtifact {
        #[command(flatten)]
        checkpoint: CheckpointInput,
        /// JSON Artifact ResourceRef file or standard input.
        #[arg(long, default_value = "-")]
        reference_file: PathBuf,
        /// Maximum number of original payload bytes to extract.
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: usize,
    },
    /// Inspect a detached backup and optionally restore its verified files to a new directory.
    VerifyBackup {
        /// Directory containing the copied manifest and data blobs.
        #[arg(long)]
        directory: PathBuf,
        /// Trusted manifest digest obtained from the original authority.
        #[arg(long)]
        manifest_digest: String,
        /// New output directory under a private, operator-owned parent.
        #[arg(long)]
        restore_to: Option<PathBuf>,
        /// Maximum total payload bytes to verify or restore.
        #[arg(long, default_value_t = DEFAULT_BACKUP_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=MAX_BACKUP_BYTES))]
        max_bytes: u64,
    },
    /// Sign an exact native Backup for a named recipient using a stopped source authority.
    AttestBackupTransfer {
        /// JSON Backup ResourceRef, or standard input (-).
        #[arg(long, default_value = "-")]
        reference_file: PathBuf,
        /// Exact destination Node authority ID.
        #[arg(long)]
        recipient_authority: String,
        /// Private Ed25519 PKCS#8 DER file owned by the operator in mode 0600.
        #[arg(long)]
        signing_key_file: PathBuf,
        /// Maximum combined database and journal copy size in MiB.
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..=1024))]
        max_copy_mib: u32,
        /// Parent for the private verification copy (defaults to TMPDIR).
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
    },
    /// Inspect the exact final placement of a witnessed backup intake.
    InspectBackupIntake {
        /// Original detached manifest SHA-256 digest.
        #[arg(long)]
        manifest_digest: String,
        /// Maximum combined database and journal copy size in MiB.
        #[arg(long, default_value_t = 64, value_parser = clap::value_parser!(u32).range(1..=1024))]
        max_copy_mib: u32,
        /// Parent for the private verification copy (defaults to TMPDIR).
        #[arg(long)]
        scratch_dir: Option<PathBuf>,
        /// Maximum total payload bytes to verify.
        #[arg(long, default_value_t = DEFAULT_BACKUP_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=MAX_BACKUP_BYTES))]
        max_bytes: u64,
    },
    /// Register only an already published backup selected by stopped intake inspection.
    ReconcileBackupIntake {
        /// Exact recovery object from inspect-backup-intake, or standard input (-).
        #[arg(long, default_value = "-")]
        recovery_file: PathBuf,
        /// Maximum total payload bytes to verify.
        #[arg(long, default_value_t = DEFAULT_BACKUP_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=MAX_BACKUP_BYTES))]
        max_bytes: u64,
    },
    /// Register a verified detached backup with a stopped local authority.
    ImportBackup {
        /// Directory containing the copied manifest and data blobs.
        #[arg(long)]
        directory: PathBuf,
        /// Trusted manifest digest obtained from the original authority.
        #[arg(long)]
        manifest_digest: String,
        /// Detached signed transfer attestation JSON, or standard input (-).
        #[arg(long, requires_all = ["trusted_source_key", "expected_source_authority"])]
        attestation_file: Option<PathBuf>,
        /// Independently trusted source signing key (32 raw Ed25519 public bytes).
        #[arg(long, requires = "attestation_file")]
        trusted_source_key: Option<PathBuf>,
        /// Independently trusted exact source Node authority ID.
        #[arg(long, requires = "attestation_file")]
        expected_source_authority: Option<String>,
    },
    RecoverOwner,
}

#[derive(clap::Args)]
struct CompactionOptions {
    /// Maximum combined database and journal input size for verification, in MiB.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_copy_mib: u32,
    /// Parent for the private verification copy (defaults to TMPDIR).
    #[arg(long)]
    scratch_dir: Option<PathBuf>,
}

#[derive(clap::Args)]
struct CheckpointInput {
    /// Private directory containing the checkpoint manifest and SQLite members.
    #[arg(long)]
    directory: PathBuf,
    /// Trusted manifest digest recorded when the checkpoint was published.
    #[arg(long)]
    manifest_digest: String,
    /// Maximum combined database and journal size in MiB.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(1..))]
    max_copy_mib: u32,
    /// Parent directory for the temporary verification copy (defaults to TMPDIR).
    #[arg(long)]
    scratch_dir: Option<PathBuf>,
}

fn read_document(path: &Path, name: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    read_bounded_document(path, name, psios_api::MAX_FRAME)
}

fn read_bounded_document(
    path: &Path,
    name: &str,
    limit: usize,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        stdin()
            .lock()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
    } else {
        File::open(path)?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() > limit {
        return Err(
            psios_core::err("InvalidArgument", format!("{name} exceeds size limit")).into(),
        );
    }
    Ok(psios_api::parse(&bytes)?)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = Opts::parse();
    let cfg = || Config::resolve(o.config.as_deref(), o.state.as_deref());
    match o.cmd {
        Cmd::Init => {
            let cfg = cfg()?;
            Store::init(&cfg)?;
            println!(
                "Initialized {}. Owner credential file {}",
                cfg.state_dir.display(),
                cfg.witness_dir.join("owner.token").display()
            );
        }
        Cmd::RecoverOwner => {
            Store::open(cfg()?)?.recover_owner()?;
            println!("Local owner credential rotated");
        }
        Cmd::Verify {
            max_copy_mib,
            scratch_dir,
            rehearse_migration,
        } => {
            let scratch = scratch_dir.unwrap_or_else(std::env::temp_dir);
            let cfg = cfg()?;
            let max_bytes = u64::from(max_copy_mib) * 1024 * 1024;
            let report = if rehearse_migration {
                Store::rehearse_migration(&cfg, &scratch, max_bytes)?
            } else {
                Store::verify(&cfg, &scratch, max_bytes)?
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::PlanStateCompaction(options) => {
            let cfg = cfg()?;
            let scratch = options.scratch_dir.unwrap_or_else(std::env::temp_dir);
            let max_bytes = u64::from(options.max_copy_mib) * 1024 * 1024;
            let plan = Store::plan_state_compaction(&cfg, &scratch, max_bytes)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        Cmd::CompactState { plan_file, options } => {
            let cfg = cfg()?;
            let scratch = options.scratch_dir.unwrap_or_else(std::env::temp_dir);
            let max_bytes = u64::from(options.max_copy_mib) * 1024 * 1024;
            let approved = read_document(&plan_file, "state compaction plan")?;
            let report = Store::compact_state(&cfg, &scratch, max_bytes, &approved)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::InspectRetainedInputs {
            references_file,
            max_copy_mib,
            max_scan_mib,
            scratch_dir,
        } => {
            let references = read_document(&references_file, "retained input reference list")?;
            let scratch = scratch_dir.unwrap_or_else(std::env::temp_dir);
            let report = Store::inspect_retained_inputs(
                &cfg()?,
                &scratch,
                u64::from(max_copy_mib) * 1024 * 1024,
                u64::from(max_scan_mib) * 1024 * 1024,
                &references,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Checkpoint {
            output,
            max_copy_mib,
        } => {
            let report =
                Store::checkpoint(&cfg()?, &output, u64::from(max_copy_mib) * 1024 * 1024)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::VerifyCheckpoint(input) => {
            let scratch = input.scratch_dir.unwrap_or_else(std::env::temp_dir);
            let report = Store::verify_checkpoint(
                &input.directory,
                &input.manifest_digest,
                &scratch,
                u64::from(input.max_copy_mib) * 1024 * 1024,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::CheckpointArtifact {
            checkpoint,
            reference_file,
            max_bytes,
        } => {
            let scratch = checkpoint.scratch_dir.unwrap_or_else(std::env::temp_dir);
            let reference = read_document(&reference_file, "artifact reference")?;
            let bytes = Store::checkpoint_artifact(
                &checkpoint.directory,
                &checkpoint.manifest_digest,
                &scratch,
                u64::from(checkpoint.max_copy_mib) * 1024 * 1024,
                &reference,
                max_bytes,
            )?;
            stdout().lock().write_all(&bytes)?;
        }
        Cmd::VerifyBackup {
            directory,
            manifest_digest,
            restore_to,
            max_bytes,
        } => {
            let report = inspect_detached_backup_with_limit(
                &directory,
                &manifest_digest,
                restore_to.as_deref(),
                max_bytes,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::AttestBackupTransfer {
            reference_file,
            recipient_authority,
            signing_key_file,
            max_copy_mib,
            scratch_dir,
        } => {
            let reference = read_bounded_document(&reference_file, "backup reference", 4096)?;
            let scratch = scratch_dir.unwrap_or_else(std::env::temp_dir);
            let envelope = Store::attest_backup_transfer(
                &cfg()?,
                &scratch,
                u64::from(max_copy_mib) * 1024 * 1024,
                &reference,
                &recipient_authority,
                &signing_key_file,
            )?;
            println!("{}", serde_json::to_string_pretty(&envelope)?);
        }
        Cmd::InspectBackupIntake {
            manifest_digest,
            max_copy_mib,
            scratch_dir,
            max_bytes,
        } => {
            let scratch = scratch_dir.unwrap_or_else(std::env::temp_dir);
            let report = Store::inspect_backup_intake(
                &cfg()?,
                &scratch,
                u64::from(max_copy_mib) * 1024 * 1024,
                &manifest_digest,
                max_bytes,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::ReconcileBackupIntake {
            recovery_file,
            max_bytes,
        } => {
            let approval = read_bounded_document(&recovery_file, "backup intake recovery", 4096)?;
            let report = Store::reconcile_backup_intake(cfg()?, &approval, max_bytes)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::ImportBackup {
            directory,
            manifest_digest,
            attestation_file,
            trusted_source_key,
            expected_source_authority,
        } => {
            let cfg = cfg()?;
            let report = match (attestation_file, trusted_source_key, expected_source_authority) {
                (None, None, None) => Store::import_detached_backup(cfg, &directory, &manifest_digest)?,
                (Some(path), Some(key), Some(source)) => {
                    let envelope = read_bounded_document(&path, "backup transfer attestation", 16384)?;
                    Store::import_signed_detached_backup(
                        cfg, &directory, &manifest_digest, &envelope, &key, &source,
                    )?
                }
                _ => return Err(psios_core::err("InvalidArgument", "signed import requires the attestation, trusted public key and expected source authority").into()),
            };
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::Check => {
            let _authority = Store::open(cfg()?)?;
            println!("State version and witness verified");
        }
        Cmd::Serve => {
            let cfg = cfg()?;
            let stop = Arc::new(AtomicBool::new(false));
            for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
                signal_hook::flag::register(sig, stop.clone())?;
            }
            server::serve(cfg, stop)?;
        }
    }
    Ok(())
}
