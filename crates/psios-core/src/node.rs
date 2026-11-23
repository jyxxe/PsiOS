//! Linux observation and the operator's explicit systemd allowlist.
use crate::{
    config::Manager,
    db::{Db, Doc},
    host::{Action, Work},
    policy, *,
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

fn upsert(db: &Db<'_>, kind: &str, id: &str, spec: Value, body: Value) -> Result<Doc> {
    match db.get(id) {
        Ok(d) if d.spec == spec && d.body == body => Ok(d),
        Ok(d) => db.update(&d, spec, body),
        Err(e) if e.0.code == "NotFound" => db.create(kind, id, policy::SYSTEM, spec, body),
        Err(e) => Err(e),
    }
}
pub fn sync(db: &Db<'_>) -> Result<()> {
    let id = db.meta("authority")?;
    let prior = db.get(&id);
    let scheduling = match prior {
        Ok(d) => d.body["scheduling"]
            .as_str()
            .unwrap_or("Enabled")
            .to_owned(),
        Err(e) if e.0.code == "NotFound" => "Enabled".into(),
        Err(e) => return Err(e),
    };
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let spec = json!({"node_id":id,"worker_count":db.cfg.workers,"queue_limit":db.cfg.queue_limit,"scheduling":scheduling});
    upsert(
        db,
        "Node",
        &id,
        spec.clone(),
        json!({"configuration":spec,"scheduling":scheduling,"boot_id":boot.trim(),"lifecycle":"Ready"}),
    )?;
    let mut active = std::collections::HashSet::new();
    for item in &db.cfg.services {
        let sid = format!("service-{}", item.unit);
        let spec = serde_json::to_value(item)?;
        let current = db.find(&sid)?;
        let observation = current
            .as_ref()
            .filter(|d| d.spec == spec && d.body["lifecycle"] == "Registered")
            .map(|d| d.body["observation"].clone())
            .unwrap_or(Value::Null);
        let mut body =
            json!({"lifecycle":"Registered","configuration":spec,"observation":observation});
        if let Some(start) = current
            .as_ref()
            .and_then(|d| d.body.get("start_operation_id"))
        {
            body["start_operation_id"] = start.clone();
        }
        upsert(db, "HostService", &sid, spec, body)?;
        active.insert(sid);
    }
    let known =
        db.c.prepare(
            "SELECT id FROM current WHERE kind='HostService' AND body->>'lifecycle'='Registered'",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in known.iter().filter(|id| !active.contains(*id)) {
        let d = db.get(id)?;
        let mut body = d.body.clone();
        body["lifecycle"] = json!("Unregistered");
        db.status(&d, body)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cordon {
    expected_resource_version: String,
    scheduling: String,
}
pub fn configure(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Cordon = decode(params)?;
    require(
        matches!(p.scheduling.as_str(), "Enabled" | "Cordoned"),
        "InvalidArgument",
        "unknown scheduling mode",
    )?;
    let d = db.of(&db.meta("authority")?, "Node")?;
    d.cas(&p.expected_resource_version)?;
    let mut spec = d.spec.clone();
    spec["scheduling"] = json!(p.scheduling);
    let mut b = d.body.clone();
    b["scheduling"] = json!(p.scheduling);
    b["configuration"] = spec.clone();
    Ok(db.update(&d, spec, b)?.reference())
}
pub fn get(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let _: Empty = decode(p)?;
    let d = db.of(&db.meta("authority")?, "Node")?;
    policy::can_target(db, actor, "node.inventory", &d.id)?;
    let mut groups = BTreeMap::new();
    for entry in
        db.c.prepare("SELECT phase,count(*) FROM queue GROUP BY phase")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
    {
        let (phase, count) = entry?;
        groups.insert(phase, count);
    }
    Ok(json!({"reference":d.reference(),"state":d.body,"queue":groups}))
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessQuery {
    pub after_pid: Option<u32>,
    pub limit: Option<usize>,
}
impl ProcessQuery {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=128).contains(&self.limit.unwrap_or(32)),
            "InvalidArgument",
            "process page limit must be 1 to 128",
        )
    }
}
fn scalar(path: impl AsRef<Path>) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}
fn inventory() -> Result<Value> {
    let mut mem = BTreeMap::new();
    for line in fs::read_to_string("/proc/meminfo")?.lines() {
        if let Some((k, v)) = line.split_once(':')
            && matches!(k, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree")
            && let Some(n) = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<u64>().ok())
        {
            mem.insert(k.to_owned(), n * 1024);
        }
    }
    Ok(
        json!({"boot_id":scalar("/proc/sys/kernel/random/boot_id")?,"kernel_release":scalar("/proc/sys/kernel/osrelease")?,
      "architecture":std::env::consts::ARCH,"available_cpus":std::thread::available_parallelism()?.get(),
      "memory_bytes":mem,"load_average":scalar("/proc/loadavg")?.split_whitespace().take(3).collect::<Vec<_>>(),
      "observed_at":now()}),
    )
}
fn processes(p: &ProcessQuery) -> Result<Value> {
    p.validate()?;
    let mut pids = fs::read_dir("/proc")?
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid > p.after_pid.unwrap_or(0))
        .collect::<Vec<_>>();
    pids.sort_unstable();
    let mut items = Vec::new();
    let boot = scalar("/proc/sys/kernel/random/boot_id")?;
    let limit = p.limit.unwrap_or(32);
    let mut last = 0;
    let mut more = false;
    for pid in pids {
        if items.len() == limit {
            more = true;
            break;
        }
        // One procfs read binds comm and identity (starttime, field 22) to the same process.
        let Ok(stat) = fs::read(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let stat = String::from_utf8_lossy(&stat);
        let Some((head, tail)) = stat.rsplit_once(") ") else {
            continue;
        };
        let Some((_, name)) = head.split_once('(') else {
            continue;
        };
        let fields: Vec<_> = tail.split_whitespace().collect();
        if fields.len() < 22 {
            continue;
        }
        items.push(
            json!({"pid":pid,"name":name,"state":fields[0],"parent_pid":fields[1],
            "start_ticks":fields[19],"resident_pages":fields[21]}),
        );
        last = pid;
    }
    Ok(
        json!({"boot_id":boot,"items":items,"next_after_pid":more.then_some(last),"observed_at":now()}),
    )
}
fn devices() -> Result<Value> {
    let mut items = Vec::new();
    for entry in fs::read_dir("/sys/class/block")? {
        let entry = entry?;
        let path = entry.path();
        let Ok(dev) = scalar(path.join("dev")) else {
            continue;
        };
        items.push(
            json!({"name":entry.file_name().to_string_lossy(),"major_minor":dev,
            "read_only":scalar(path.join("ro"))?=="1","sectors_512":scalar(path.join("size"))?,
            "logical_block_size":scalar(path.join("queue/logical_block_size")).ok()}),
        );
        require(
            items.len() <= 1024,
            "QuotaExceeded",
            "block device inventory exceeds profile",
        )?;
    }
    items.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({"devices":items,"observed_at":now()}))
}
fn service(w: &Work) -> Result<Value> {
    let cfg = w
        .cfg
        .services
        .iter()
        .find(|s| {
            w.target.spec["unit"] == s.unit
                && serde_json::to_value(s).is_ok_and(|v| v == w.target.spec)
        })
        .ok_or_else(|| err("PermissionDenied", "service no longer configured"))?;
    let manager = if cfg.manager == Manager::User {
        "--user"
    } else {
        "--system"
    };
    let command = |verb: &str, args: &[&str], effect: bool| -> Result<Vec<u8>> {
        let mut cmd = Command::new("/usr/bin/systemctl");
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("LC_ALL", "C");
        if cfg.manager == Manager::User {
            cmd.env(
                "XDG_RUNTIME_DIR",
                format!("/run/user/{}", rustix::process::geteuid().as_raw()),
            );
        }
        cmd.args([manager, "--no-pager", "--no-ask-password", verb])
            .args(args)
            .arg("--")
            .arg(&cfg.unit);
        let out = crate::command::capture(cmd, effect, "systemd", 30)?;
        require(
            !out.truncated,
            if effect {
                "OutcomeUnknown"
            } else {
                "QuotaExceeded"
            },
            "systemd response exceeds limit",
        )?;
        Ok(out.stdout)
    };
    let inspect = || -> Result<Value> {
        let bytes = command(
            "show",
            &["--property=Id,LoadState,ActiveState,SubState,MainPID,InvocationID,NeedDaemonReload"],
            false,
        )?;
        let mut props = BTreeMap::new();
        for line in std::str::from_utf8(&bytes)
            .map_err(|_| err("DependencyUnavailable", "invalid systemd response"))?
            .lines()
        {
            if let Some((k, v)) = line.split_once('=') {
                props.insert(k, v);
            }
        }
        require(
            props.get("Id") == Some(&cfg.unit.as_str())
                && props.get("LoadState") == Some(&"loaded"),
            "PreconditionFailed",
            "unit identity unavailable or aliased",
        )?;
        Ok(
            json!({"unit":cfg.unit,"active_state":props.get("ActiveState"),"sub_state":props.get("SubState"),
          "definition_digest":psios_api::sha256(&command("cat",&[],false)?),"needs_reload":props.get("NeedDaemonReload")!=Some(&"no"),"main_pid":props.get("MainPID"),"invocation_id":props.get("InvocationID"),"observed_at":now()}),
        )
    };
    let before = inspect()?;
    let (verb, desired) = match w.action {
        Action::ServiceStart(_) => ("start", "active"),
        Action::ServiceStop(_) => ("stop", "inactive"),
        _ => return Ok(before),
    };
    require(
        cfg.allow_control,
        "PermissionDenied",
        "unit registered for observation only",
    )?;
    require(
        before["needs_reload"] == false
            && before["definition_digest"].as_str() == cfg.definition_digest.as_deref(),
        "ApprovalInvalidated",
        "systemd definition changed or needs a reload",
    )?;
    command(verb, &[], true)?;
    let observed =
        inspect().map_err(|_| err("OutcomeUnknown", "unit change requires reconciliation"))?;
    require(
        observed["active_state"] == desired
            && observed["needs_reload"] == false
            && observed["definition_digest"] == before["definition_digest"],
        "OutcomeUnknown",
        "unit did not confirm requested state",
    )?;
    Ok(observed)
}
pub fn perform(w: &Work) -> Result<Value> {
    match &w.action {
        Action::Inventory {} => inventory(),
        Action::Processes(p) => processes(p),
        Action::Devices {} => devices(),
        _ => service(w),
    }
}
pub fn reserve(db: &Db<'_>, w: &Work) -> Result<()> {
    if w.target.kind == "HostService" {
        require(
            w.target.body["lifecycle"] == "Registered",
            "PreconditionFailed",
            "service is not registered",
        )?;
        if !matches!(w.action, Action::ServiceInspect(_)) {
            require(
                w.target.spec["allow_control"] == true,
                "PermissionDenied",
                "service control is disabled",
            )?;
            require(
                db.c.execute(
                    "INSERT OR IGNORE INTO holds VALUES(?1,?2)",
                    rusqlite::params![w.target.id, w.op.id],
                )? == 1,
                "Conflict",
                "service already has a pending operation",
            )?;
        }
    }
    Ok(())
}
pub fn observed_effect(db: &Db<'_>, service: &Doc, op: &str, observation: &Value) -> Result<Doc> {
    let mut b = service.body.clone();
    b["observation"] = observation.clone();
    if observation["active_state"] == "active" {
        b["start_operation_id"] = json!(op)
    } else {
        b.as_object_mut().unwrap().remove("start_operation_id");
    }
    db.status(service, b)
}
pub fn apply(db: &Db<'_>, w: &Work, r: &Value) -> Result<Value> {
    Ok(json!({"service_ref":observed_effect(db,&w.target,&w.op.id,r)?.reference(),"observation":r}))
}
pub fn enabled(db: &Db<'_>) -> Result<bool> {
    let mode:Option<String>=db.c.query_row("SELECT body->>'scheduling' FROM current WHERE kind='Node' AND id=(SELECT value FROM meta WHERE key='authority')",[],|r|r.get(0)).optional()?;
    Ok(mode.as_deref() == Some("Enabled"))
}
