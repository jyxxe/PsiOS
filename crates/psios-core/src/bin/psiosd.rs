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
    /// Register a verified detached backup with a stopped local authority.
    ImportBackup {
        /// Directory containing the copied manifest and data blobs.
        #[arg(long)]
        directory: PathBuf,
        /// Trusted manifest digest obtained from the original authority.
        #[arg(long)]
        manifest_digest: String,
    },
    RecoverOwner,
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

fn read_reference(path: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    let limit = psios_api::MAX_FRAME;
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
            psios_core::err("InvalidArgument", "artifact reference exceeds size limit").into(),
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
            let reference = read_reference(&reference_file)?;
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
        Cmd::ImportBackup {
            directory,
            manifest_digest,
        } => {
            let report = Store::import_detached_backup(cfg()?, &directory, &manifest_digest)?;
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
