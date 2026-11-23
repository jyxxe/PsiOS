use clap::{Parser, Subcommand};
use psios_api::{Client, StatusProvider, parse};
use psios_core::Config;
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::Duration,
};

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
    /// Verify and write the canonical JSON for an Artifact reference from a file or stdin.
    Artifact {
        #[arg(default_value = "-")]
        file: String,
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: usize,
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
        Cmd::ModelStep {
            session,
            key,
            decision_dir,
            local_model,
        } => {
            let local: Option<psios_api::LocalInventoryModel> = local_model
                .as_deref()
                .map(|path| {
                    let model: psios_api::LocalInventoryModel =
                        serde_json::from_value(input(path)?)?;
                    model.validate()?;
                    Ok::<_, Box<dyn std::error::Error>>(model)
                })
                .transpose()?;
            let provider: &dyn psios_api::SessionModel = match &local {
                Some(model) => model,
                None => &psios_api::NodeSurvey,
            };
            match decision_dir {
                Some(dir) => c.model_turn_durable(&session, &key, provider, &dir)?,
                None => c.model_turn(&session, &key, provider)?,
            }
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
        Cmd::List { kind } => c.call("Resource.List", json!({"kind": kind.unwrap_or_default()}))?,
        Cmd::Watch { cursor, wait_ms } => match wait_ms {
            Some(timeout_ms) => c.call(
                "Event.Wait",
                json!({"cursor":cursor,"timeout_ms":timeout_ms}),
            )?,
            None => c.call("Event.Poll", json!({"cursor":cursor}))?,
        },
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
