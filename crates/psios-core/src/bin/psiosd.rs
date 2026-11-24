use clap::{Parser, Subcommand};
use psios_core::{Config, Store, inspect_detached_backup, server};
use std::{
    path::PathBuf,
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
    },
    RecoverOwner,
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
        } => {
            let scratch = scratch_dir.unwrap_or_else(std::env::temp_dir);
            let report = Store::verify(&cfg()?, &scratch, u64::from(max_copy_mib) * 1024 * 1024)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Cmd::VerifyBackup {
            directory,
            manifest_digest,
            restore_to,
        } => {
            let report =
                inspect_detached_backup(&directory, &manifest_digest, restore_to.as_deref())?;
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
