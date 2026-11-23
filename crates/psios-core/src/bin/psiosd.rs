use clap::{Parser, Subcommand};
use psios_core::{Config, Store, server};
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
    Check,
    RecoverOwner,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let o = Opts::parse();
    let cfg = if let Some(file) = o.config {
        Config::load(&file)?
    } else {
        Config::local(&std::path::absolute(
            o.state.unwrap_or_else(|| "/var/lib/psios".into()),
        )?)
    };
    match o.cmd {
        Cmd::Init => {
            Store::init(&cfg)?;
            println!(
                "Initialized {}. Owner credential file {}",
                cfg.state_dir.display(),
                cfg.witness_dir.join("owner.token").display()
            );
        }
        Cmd::RecoverOwner => {
            Store::open(cfg)?.recover_owner()?;
            println!("Local owner credential rotated");
        }
        Cmd::Check => {
            let _authority = Store::open(cfg)?;
            println!("State version and witness verified");
        }
        Cmd::Serve => {
            let stop = Arc::new(AtomicBool::new(false));
            for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
                signal_hook::flag::register(sig, stop.clone())?;
            }
            server::serve(cfg, stop)?;
        }
    }
    Ok(())
}
