//! Linux observation and the operator's explicit systemd allowlist.
pub mod block;
pub(crate) mod block_device;
pub mod journal;
pub(crate) mod mounts;
pub mod network;
pub mod pressure;
pub mod process;
pub(crate) mod service_control;
pub(crate) mod service_manager;
pub(crate) mod service_resources;
pub mod service_state;
pub(crate) mod work_admission;
use crate::{
    db::{Db, Doc},
    host::{Action, ServiceRestart, Work},
    policy, *,
};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::Path};

pub(crate) const RESTART_TOOL: &str = "service.restart";

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
        let registered = upsert(db, "HostService", &sid, spec, body)?;
        service_state::sync(db, &registered, boot.trim())?;
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
        let retired = db.status(&d, body)?;
        service_state::sync(db, &retired, boot.trim())?;
    }
    network::enrollment::sync(db)?;
    network::probe::sync(db, &id)
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
    Ok(
        json!({"reference":d.reference(),"state":d.body,"queue":groups,
        "admission":broker::admission::status(db)?,
        "oci_capture_staging":{"reserved_bytes":apps::capture::reserved_staging_bytes(db)?,
                               "max_bytes":apps::capture::STAGING_BYTES},
        "authority_storage":db.cfg.authority_storage()?}),
    )
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
pub(crate) fn service_target(db: &Db<'_>, id: &str, revision: &str) -> Result<Doc> {
    let service = db.of(id, "HostService")?;
    service.cas(revision)?;
    require_registered(&service)?;
    Ok(service)
}

fn require_registered(service: &Doc) -> Result<()> {
    require(
        service.body["lifecycle"] == "Registered",
        "PreconditionFailed",
        "unregistered system unit",
    )
}

fn configured_service(w: &Work) -> Result<&crate::config::ServiceConfig> {
    w.cfg
        .services
        .iter()
        .find(|s| {
            w.target.spec["unit"] == s.unit
                && serde_json::to_value(s).is_ok_and(|v| v == w.target.spec)
        })
        .ok_or_else(|| err("PermissionDenied", "service no longer configured"))
}

pub(crate) fn valid_invocation(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.simple().to_string() == value)
}

pub(crate) fn restarted(observation: &Value, request: &ServiceRestart) -> bool {
    observation["boot_id"] == request.expected_boot_id
        && observation["invocation_id"]
            .as_str()
            .is_some_and(|id| id != request.expected_invocation_id && valid_invocation(id))
}

fn service(w: &Work, checkpoint: impl FnOnce(&Value) -> Result<()>) -> Result<Value> {
    let cfg = configured_service(w)?;
    let command = |verb: &str, args: &[&str], effect: bool| -> Result<Vec<u8>> {
        let mut cmd = service_manager::command(&cfg.manager, verb);
        cmd.args(args).arg("--").arg(&cfg.unit);
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
    let manager_sample = || service_manager::sample(&cfg.manager);
    let inspect = || -> Result<Value> {
        let started = manager_sample()?;
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
            props.get("Id") == Some(&cfg.unit.as_str()),
            "PreconditionFailed",
            "unit identity unavailable or aliased",
        )?;
        let load_state = props.get("LoadState").copied().unwrap_or_default();
        let definition = if load_state == "loaded" {
            Some(psios_api::sha256(&command("cat", &[], false)?))
        } else {
            require(
                matches!(w.action, Action::ServiceObserve(_))
                    && matches!(load_state, "not-found" | "masked" | "bad-setting" | "error"),
                "PreconditionFailed",
                "unit is not loaded",
            )?;
            None
        };
        let final_sample = manager_sample()?;
        require(
            started.is_some() == final_sample.is_some(),
            "DependencyUnavailable",
            "systemd manager startup time was unavailable during unit observation",
        )?;
        require(
            started == final_sample,
            "PreconditionFailed",
            "systemd manager changed during unit observation",
        )?;
        let boot = service_control::boot_id()?;
        let mut observation = json!({"unit":cfg.unit,"manager":cfg.manager,"manager_uid":service_control::manager_uid(&cfg.manager),
              "boot_id":boot,"load_state":load_state,"active_state":props.get("ActiveState"),"sub_state":props.get("SubState"),
          "definition_digest":definition,"needs_reload":props.get("NeedDaemonReload")!=Some(&"no"),"main_pid":props.get("MainPID"),"invocation_id":props.get("InvocationID"),"observed_at":now()});
        if let Some(started) = started {
            observation[service_manager::STARTED] = json!(started);
        }
        Ok(observation)
    };
    let before = inspect()?;
    let Some(control) = service_control::Control::from_action(&w.action) else {
        return Ok(before);
    };
    require(
        service_control::expected_invocation(&w.action, &before),
        "PreconditionFailed",
        "the approved active service invocation changed",
    )?;
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
    let check_manager = || -> Result<()> {
        let current = manager_sample()?;
        let expected = service_manager::timestamp(&before);
        require(
            expected.is_some() && current.is_some(),
            "DependencyUnavailable",
            "systemd manager startup time is unavailable",
        )?;
        require(
            expected == current.as_deref(),
            "PreconditionFailed",
            "systemd manager changed before unit control",
        )
    };
    check_manager()?;
    checkpoint(&before)?;
    check_manager().map_err(|_| {
        err(
            "OutcomeUnknown",
            "checkpointed systemd manager could not be confirmed before unit control",
        )
    })?;
    command(control.verb(), &[], true)?;
    let observed =
        inspect().map_err(|_| err("OutcomeUnknown", "unit change requires reconciliation"))?;
    require(
        service_control::confirms(control, &w.target, &before, &observed),
        "OutcomeUnknown",
        "unit did not confirm the checkpointed manager, boot and postcondition",
    )?;
    Ok(observed)
}
pub fn perform(w: &Work, checkpoint: impl FnOnce(&Value) -> Result<()>) -> Result<Value> {
    match &w.action {
        Action::Inventory {} => inventory(),
        Action::Pressure {} => pressure::observe(w),
        Action::Interfaces(q) => network::interfaces(q),
        Action::Routes(q) => network::observe_routes(q, w),
        Action::RouteLookup(q) => network::observe_lookup(q),
        Action::RoutingRules(q) => network::observe_rules(q, w),
        Action::NetworkProbe(_) => network::probe::perform(w),
        Action::Processes(p) => process::page(&w.target.id, p),
        Action::InspectProcess(expected) => process::inspect(&w.target.id, expected),
        Action::Devices {} => devices(),
        Action::BlockLayout(query) => block::observe(w, query),
        Action::TrackBlockDevice(query) => block_device::enroll(query),
        Action::ObserveBlockDevice(_) => block_device::observe(w),
        Action::Mounts {} => mounts::observe(),
        Action::ServiceLogs(query) => journal::read(w, query),
        Action::ServiceLogsSince(query) => journal::read_since(w, query),
        Action::ServiceResources(query) => service_resources::observe(w, query),
        _ => service(w, checkpoint),
    }
}
pub fn reserve(db: &Db<'_>, w: &Work) -> Result<()> {
    if w.target.kind == "HostService" {
        require(
            w.target.body["lifecycle"] == "Registered",
            "PreconditionFailed",
            "service is not registered",
        )?;
        if service_control::is_control(&w.action) {
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
            service_control::begin(db, w)?;
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
    let checkpoint = service_control::confirmed(db, &w.op, &w.target, &w.action, r)?
        .ok_or_else(|| err("RecoveryRequired", "new systemd result lacks a checkpoint"))?;
    Ok(
        json!({"service_ref":observed_effect(db,&w.target,&w.op.id,r)?.reference(),"observation":r,
        "checkpoint_ref":checkpoint.reference()}),
    )
}
pub fn enabled(db: &Db<'_>) -> Result<bool> {
    let mode:Option<String>=db.c.query_row("SELECT body->>'scheduling' FROM current WHERE kind='Node' AND id=(SELECT value FROM meta WHERE key='authority')",[],|r|r.get(0)).optional()?;
    Ok(mode.as_deref() == Some("Enabled"))
}
