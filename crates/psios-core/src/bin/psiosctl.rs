use clap::{Parser, Subcommand};
use psios_api::{Client, LocalInventoryModel, ResourceRef, SessionModel, StatusProvider, parse};
use psios_core::Config;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
    Get {
        id: String,
    },
    List {
        kind: Option<String>,
        #[arg(long)]
        id: Option<String>,
    },
    Watch {
        cursor: String,
        #[arg(long)]
        wait_ms: Option<u64>,
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
        #[arg(default_value = "-")]
        file: String,
    },
    Wait {
        id: String,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
    },
    /// Verify and write stored JSON bytes for an Artifact reference from a file or stdin.
    Artifact {
        #[arg(default_value = "-")]
        file: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: usize,
    },
    /// Export a verified backup to a new directory under a private parent.
    BackupExport {
        #[arg(long)]
        output: PathBuf,
        /// JSON Backup ResourceRef from a file or stdin.
        #[arg(default_value = "-")]
        file: String,
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
        /// Existing private directory for this provider worker's pending decision.
        #[arg(long)]
        decision_dir: Option<PathBuf>,
        /// Use the configured local chat-completions server for an inventory Skill.
        #[arg(long)]
        local_model: Option<String>,
    },
    /// Serve opted-in sessions for a pinned model using a private decision journal.
    ModelWorker {
        /// Local file containing the exact ResourceRef returned by Model.Register.
        model: String,
        #[arg(long)]
        decision_dir: PathBuf,
        #[arg(long)]
        local_model: Option<String>,
        #[arg(long, default_value_t = 10_000, value_parser = clap::value_parser!(u64).range(1..=10_000))]
        wait_ms: u64,
        /// Process at most one decision and report its receipt or an empty queue.
        #[arg(long)]
        once: bool,
    },
    Node,
    Validate {
        contract: String,
        #[arg(default_value = "-")]
        file: String,
    },
}
fn input(path: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let reader: Box<dyn Read> = if path == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(fs::File::open(path)?)
    };
    let mut bytes = vec![];
    reader
        .take(psios_api::MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > psios_api::MAX_FRAME {
        return Err("input exceeds frame limit".into());
    }
    Ok(parse(&bytes)?)
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
        Cmd::Whoami => c.call("Principal.WhoAmI", json!({}))?,
        Cmd::Capabilities => c.call("Capability.Discover", json!({}))?,
        Cmd::Status { delegation } => c.call(
            "Tool.Call",
            json!({"tool": "system.status", "input": {}, "delegation_id": delegation}),
        )?,
        Cmd::Call { method, file } => c.call(&method, input(&file)?)?,
        Cmd::Get { id } => c.call("Resource.Get", json!({"id": id}))?,
        Cmd::List { kind, id } => {
            c.resource_snapshot(kind.as_deref(), id.as_deref(), None, None)?
        }
        Cmd::Watch { cursor, wait_ms } => c.watch_resources(&cursor, None, wait_ms)?,
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
            file,
        } => c.submit(&tool, input(&file)?, &key, &delegation)?,
        Cmd::Wait { id, seconds } => c.wait(&id, Duration::from_secs(seconds))?,
        Cmd::Artifact { file, max_bytes } => {
            let reference = serde_json::from_value(input(&file)?)?;
            std::io::stdout().write_all(&c.artifact_bytes(&reference, max_bytes)?)?;
            return Ok(());
        }
        Cmd::BackupExport { output, file } => {
            let reference = serde_json::from_value(input(&file)?)?;
            psios_core::download_backup(&c, &reference, &output)?
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
