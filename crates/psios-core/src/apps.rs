//! OCI adapter for a trusted local Docker Engine. No registry pulls, shell or client-provided host paths.
use crate::{
    db::{Db, Doc},
    host::{Action, Install, Work},
    policy, *,
};
use psios_api::{VERSION, id};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Command};

pub(crate) mod artifact_install;
pub(crate) mod capture;
mod cgroup;
pub(crate) mod health;
pub(crate) mod inventory;
pub(crate) mod log_tail;
pub(crate) mod mount_observation;
pub(crate) mod publisher;
pub(crate) mod release_archive;
pub(crate) mod release_retirement;
pub(crate) mod restart;
pub(crate) mod restore_archive;
pub(crate) mod restore_image;
pub(crate) mod stop;
pub(crate) mod volume_rebind;

fn environment(env: &BTreeMap<String, String>) -> Result<()> {
    require(
        env.len() <= 32
            && env.iter().all(|(k, v)| {
                !k.is_empty()
                    && k.len() < 128
                    && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && v.len() <= 1024
                    && !v.contains('\0')
            }),
        "InvalidArgument",
        "invalid non-secret environment",
    )
}
fn valid_image_id(image: &str) -> bool {
    image.len() == 71
        && image.starts_with("sha256:")
        && image[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn validate(s: &Install) -> Result<()> {
    label(&s.name)?;
    label(&s.release_version)?;
    label(&s.license_expression)?;
    require(
        valid_image_id(&s.image_id),
        "InvalidArgument",
        "expected immutable local OCI image configuration digest",
    )?;
    require(
        !s.argv.is_empty()
            && s.argv.len() <= 64
            && s.argv[0].starts_with('/')
            && s.argv.iter().all(|a| a.len() <= 4096 && !a.contains('\0')),
        "InvalidArgument",
        "invalid OCI argv",
    )?;
    require(
        (10..=4000).contains(&s.cpu_millicores)
            && (16 * 1024 * 1024..=2 * 1024 * 1024 * 1024).contains(&s.memory_max_bytes)
            && (4..=256).contains(&s.pids_max)
            && (4096..=64 * 1024 * 1024).contains(&s.ephemeral_max_bytes),
        "InvalidArgument",
        "resource limits outside local profile",
    )?;
    if let Some(probe) = &s.health_check {
        probe.validate()?;
    }
    environment(&s.environment)
}
pub fn profiles(db: &Db<'_>) -> Result<()> {
    let profiles = [
        (
            "oci-local",
            "RuntimeProfile",
            json!({
                "driver": "Docker",
                "image": "local-config-digest",
                "rootfs": "ReadOnly",
                "restart": "Never"
            }),
        ),
        (
            "oci-user",
            "ExecutionIdentityProfile",
            json!({
                "uid": rustix::process::geteuid().as_raw(),
                "gid": rustix::process::getegid().as_raw(),
                "capabilities": [],
                "no_new_privileges": true
            }),
        ),
        (
            "oci-namespaces",
            "NamespaceProfile",
            json!({
                "pid": "private",
                "ipc": "none",
                "network": "none",
                "user_mapping": "engine-default",
                "seccomp": "engine-default"
            }),
        ),
        (
            "oci-io",
            "IOProfile",
            json!({
                "ephemeral": "bounded tmpfs",
                "persistent": "explicit volume",
                "block_io_quota": "Unsupported"
            }),
        ),
        (
            "oci-network",
            "NetworkPolicy",
            json!({"driver": "none", "ports": [], "egress": false}),
        ),
        (
            "oci-health",
            "HealthCheck",
            json!({
                "probe": "inspect container ID, image, application label and State.Running",
                "scope": "process liveness"
            }),
        ),
        (
            "local-migration",
            "MigrationPlan",
            json!({"online_migrations": "Unsupported", "data_format": "application-managed"}),
        ),
        (
            "local-rollback",
            "RollbackPolicy",
            json!({"automatic": false, "restore": "separate offline volume clone"}),
        ),
        (
            "local-backup",
            "BackupPolicy",
            json!({
                "scope": "single offline volume",
                "consistency": "crash_consistent",
                "restore": "new incarnation"
            }),
        ),
        (
            "local-files",
            "VolumeSchema",
            json!({"format": "directory", "attachment": "single_node_rw"}),
        ),
    ];
    for (id, kind, body) in profiles {
        db.create(kind, id, policy::SYSTEM, body.clone(), body)?;
    }
    Ok(())
}
pub fn launch_spec(db: &Db<'_>, app: &Doc, authorizing_start: bool) -> Result<Install> {
    let deployment = db.resolve(&app.spec["deployment_ref"])?;
    require(
        deployment.kind == "Deployment"
            && deployment.spec["application_id"] == app.id
            && deployment.spec["release_ref"] == app.spec["release_ref"]
            && deployment.spec["volume_id"] == app.spec["volume_id"]
            && deployment.spec["data_incarnation"] == app.spec["data_incarnation"],
        "PreconditionFailed",
        "invalid deployment selection",
    )?;
    let release = db.resolve(&deployment.spec["release_ref"])?;
    let spec = db.resolve(&release.body["release_spec_ref"])?;
    let image = db.resolve(&release.body["artifact_ref"])?;
    require(
        release.kind == "ApplicationRelease"
            && release.body["application_id"] == app.id
            && spec.kind == "ApplicationReleaseSpec"
            && image.kind == "OCIImage"
            && app.spec["image_id"] == image.body["config_digest"],
        "PreconditionFailed",
        "release references do not describe this deployment",
    )?;
    release_retirement::available(db, &release)?;
    artifact_install::check_release_source(db, &release)?;
    if authorizing_start {
        publisher::check_launch(db, &release, &spec)?;
    }
    let r = &spec.body["resources"];
    let count = |k: &str| {
        r[k].as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| err("InvalidArgument", "invalid release resource limit"))
    };
    let launch = decode(&json!({
        "name": app.spec["name"],
        "release_version": release.body["release_version"],
        "image_id": image.body["config_digest"],
        "argv": spec.body["execution"]["argv"],
        "environment": deployment.spec["environment"],
        "license_expression": spec.body["provenance"]["license_expression"],
        "cpu_millicores": count("cpu_millicores")?,
        "memory_max_bytes": count("memory_max_bytes")?,
        "pids_max": count("pids_max")?,
        "ephemeral_max_bytes": count("ephemeral_max_bytes")?,
        "health_check": health::release(db.c, &release, &spec, &image)?
    }))?;
    validate(&launch)?;
    Ok(launch)
}
pub(crate) fn check_selected_publisher(db: &Db<'_>, app: &Doc) -> Result<()> {
    let release = db.resolve(&app.spec["release_ref"])?;
    let spec = db.resolve(&release.body["release_spec_ref"])?;
    artifact_install::check_release_source(db, &release)?;
    publisher::check_launch(db, &release, &spec)
}

pub(crate) fn check_release_selection(db: &Db<'_>, app: &Doc, reference: &Value) -> Result<()> {
    let release = release_retirement::associated(db, app, reference)?;
    release_retirement::available(db, &release)?;
    artifact_install::check_release_source(db, &release)?;
    let spec = db.resolve(&release.body["release_spec_ref"])?;
    publisher::check_launch(db, &release, &spec)?;
    require(
        app.body["lifecycle"] == "Stopped",
        "PreconditionFailed",
        "release selection requires a stopped application and its own immutable release",
    )
}

fn stage_precondition(app: &Doc, release: &Install) -> Result<()> {
    require(
        app.body["lifecycle"] != "Uninstalled" && app.spec["name"] == release.name,
        "PreconditionFailed",
        "staged release must belong to an installed application with the same name",
    )
}

pub fn preflight(db: &Db<'_>, w: &Work) -> Result<()> {
    match w.action {
        Action::Stage { ref release, .. } => stage_precondition(&w.target, release)?,
        Action::Select {
            ref release_ref, ..
        } => check_release_selection(db, &w.target, release_ref)?,
        Action::Start(_) | Action::Uninstall(_) => require(
            w.target.body["lifecycle"] == "Stopped",
            "PreconditionFailed",
            "application must be stopped",
        )?,
        Action::Stop(_)
        | Action::ContinueOciStop(_)
        | Action::RestartApp(_)
        | Action::InspectOciInstance(_) => require(
            w.target.body["lifecycle"] == "Running",
            "PreconditionFailed",
            "application has no active deployment",
        )?,
        Action::Refresh(_) | Action::Logs(_) | Action::Health(_) => require(
            w.target.body["lifecycle"] != "Uninstalled",
            "PreconditionFailed",
            "application uninstalled",
        )?,
        _ => {}
    }
    if matches!(
        w.action,
        Action::Start(_) | Action::RestartApp(_) | Action::InspectOciInstance(_)
    ) && let Some(v) = &w.volume
    {
        require(
            w.target.spec["data_incarnation"] == v.body["data_incarnation"],
            "PreconditionFailed",
            "volume incarnation changed",
        )?;
    }
    if matches!(w.action, Action::Logs(_)) && w.target.body["lifecycle"] == "Running" {
        log_tail::check_live(db, &w.target)?;
    }
    Ok(())
}
struct Engine<'a>(&'a Config);
struct EngineProfile {
    id: String,
    unified_cgroups: bool,
}
impl EngineProfile {
    fn observe_limits(&self, cid: &str, state: &Value, release: &Install) -> Result<Value> {
        if self.unified_cgroups {
            cgroup::observe(cid, state, release)
        } else {
            Ok(json!({"source": "DockerCgroupV1"}))
        }
    }
}
impl Engine<'_> {
    fn observe_mount(
        &self,
        work: &Work,
        engine: &EngineProfile,
        container: &str,
        start_op: &str,
        state: &Value,
        report: &mut Value,
    ) -> Result<()> {
        if !work.mount_observation_required {
            return Ok(());
        }
        require(
            engine.unified_cgroups,
            "UnsupportedCapability",
            "effective OCI mount observation requires a local cgroup v2 engine",
        )?;
        let volume = work.volume.as_ref().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "effective OCI mount has no approved volume",
            )
        })?;
        let proof = mount_observation::observe(volume, &engine.id, container, state, || {
            let latest = self.managed(work, container, start_op)?;
            require(
                self.identity()? == engine.id,
                "PreconditionFailed",
                "OCI engine changed during effective mount observation",
            )?;
            Ok(latest["State"].clone())
        })?;
        report[mount_observation::FIELD] = proof;
        require(
            mount_observation::matches(volume, report),
            "PreconditionFailed",
            "effective OCI mount disagrees with the observed process",
        )
    }
    fn command_line(&self, args: &[String]) -> Command {
        let mut cmd = Command::new("/usr/bin/docker");
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("--host")
            .arg(format!("unix://{}", self.0.docker_socket.display()))
            .args(args);
        cmd
    }
    fn command(&self, args: &[String], effect: bool) -> Result<crate::command::Output> {
        crate::command::capture(self.command_line(args), effect, "Docker", 20)
    }
    fn inspect(&self, kind: &str, id: &str) -> Result<Value> {
        let args = [kind, "inspect", "--format", "{{json .}}", id].map(String::from);
        let out = self.command(&args, false)?;
        require(
            !out.truncated,
            "QuotaExceeded",
            "OCI metadata exceeds limit",
        )?;
        serde_json::from_slice(&out.stdout)
            .map_err(|_| err("DependencyUnavailable", "unreadable OCI metadata"))
    }
    fn image(&self, image: &str) -> Result<Value> {
        self.inspect_image(image, None)
    }
    fn inspect_image(&self, image: &str, probe: Option<&health::Probe>) -> Result<Value> {
        let v = self.inspect("image", image)?;
        require(
            v["Id"] == image
                && v["Os"] == "linux"
                && v["Config"]["Volumes"]
                    .as_object()
                    .is_none_or(|o| o.is_empty()),
            "UnsupportedCapability",
            "expected local Linux image without implicit volumes",
        )?;
        let mut inspection = json!({
            "config_digest": image,
            "os": v["Os"],
            "architecture": v["Architecture"],
            "created": v["Created"],
            "layers": v["RootFS"]["Layers"],
            "inspected_at": now()
        });
        if let Some(probe) = probe {
            probe.require_image(&v)?;
            inspection["health_check"] = json!(probe);
        }
        Ok(inspection)
    }
    fn check_engine(&self) -> Result<EngineProfile> {
        let out = self.command(&["info", "--format", "{{json .}}"].map(String::from), false)?;
        let v: Value = serde_json::from_slice(&out.stdout)
            .map_err(|_| err("DependencyUnavailable", "cannot inspect engine limits"))?;
        require(
            v["MemoryLimit"] == true
                && v["SwapLimit"] == true
                && v["CpuCfsQuota"] == true
                && v["PidsLimit"] == true
                && v["SecurityOptions"].as_array().is_some_and(|a| {
                    a.iter()
                        .any(|x| x.as_str().is_some_and(|x| x.contains("seccomp")))
                }),
            "UnsupportedCapability",
            "engine lacks required cgroup or seccomp enforcement",
        )?;
        let engine = v["ID"]
            .as_str()
            .filter(|id| (16..=128).contains(&id.len()))
            .ok_or_else(|| err("UnsupportedCapability", "engine identity unavailable"))?;
        let unified_cgroups = match v["CgroupVersion"].as_str() {
            Some("1") => false,
            Some("2") => {
                cgroup::root()?;
                true
            }
            _ => {
                return Err(err(
                    "UnsupportedCapability",
                    "engine cgroup version unavailable",
                ));
            }
        };
        Ok(EngineProfile {
            id: engine.into(),
            unified_cgroups,
        })
    }
    fn identity(&self) -> Result<String> {
        let out = self.command(&["info", "--format", "{{.ID}}"].map(String::from), false)?;
        let value = std::str::from_utf8(&out.stdout)
            .ok()
            .map(str::trim)
            .filter(|id| !out.truncated && (16..=128).contains(&id.len()))
            .ok_or_else(|| err("DependencyUnavailable", "engine identity unavailable"))?;
        Ok(value.into())
    }
    /// Only successful full listings from the same engine can prove removal.
    fn removed(&self, w: &Work, cid: &str) -> Result<bool> {
        let Some(engine) = w.target.body["engine_id"].as_str() else {
            return Ok(false);
        };
        self.absent(w, cid, engine)
    }
    fn absent(&self, w: &Work, cid: &str, engine: &str) -> Result<bool> {
        self.application_absent(&w.target.id, Some(cid), engine)
    }
    fn application_absent(&self, app: &str, cid: Option<&str>, engine: &str) -> Result<bool> {
        require(
            self.identity()? == engine,
            "PreconditionFailed",
            "OCI engine identity changed",
        )?;
        for filter in [
            cid.map(|cid| format!("id={cid}")),
            Some(format!("label=io.psios.application={app}")),
        ]
        .into_iter()
        .flatten()
        {
            let args = [
                "container",
                "ls",
                "--all",
                "--no-trunc",
                "--format",
                "{{.ID}}",
                "--filter",
                &filter,
            ]
            .map(String::from);
            let listing = self.command(&args, false)?;
            if listing.truncated || !listing.stdout.iter().all(u8::is_ascii_whitespace) {
                return Ok(false);
            }
        }
        require(
            self.identity()? == engine,
            "PreconditionFailed",
            "OCI engine changed during observation",
        )?;
        Ok(true)
    }
    fn discard_unstarted(&self, w: &Work, cid: &str, engine: &str) -> Result<bool> {
        if self.identity()? != engine {
            return Ok(false);
        }
        let created = self.managed(w, cid, &w.op.id)?;
        if created["State"]["Status"] != "created" || created["State"]["Running"] != false {
            return Ok(false);
        }
        self.command(&["container", "rm", cid].map(String::from), true)?;
        self.absent(w, cid, engine)
    }
    fn managed(&self, w: &Work, id: &str, start_op: &str) -> Result<Value> {
        if let Some(engine) = w.target.body["engine_id"].as_str() {
            require(
                self.identity()? == engine,
                "PreconditionFailed",
                "instance belongs to a different engine",
            )?;
        }
        let v = self.inspect("container", id)?;
        require(
            v["Id"] == id
                && v["Config"]["Labels"]["io.psios.application"] == w.target.id
                && v["Image"] == w.target.spec["image_id"]
                && v["Config"]["Labels"]["io.psios.operation"] == start_op,
            "PreconditionFailed",
            "container identity mismatch",
        )?;
        Ok(v)
    }
}
// The checkpoint is an immutable authority resource. A container name or label
// found after a crash cannot substitute for the ID returned by the original engine.
pub fn pending_start(db: &Db<'_>, app: &Doc) -> Result<Option<Doc>> {
    let operation: Option<String> =
        db.c.query_row(
            "SELECT h.holder FROM holds h JOIN current o ON o.id=h.holder
         JOIN current t ON t.id=o.spec->>'task_id'
         WHERE h.volume=?1 AND t.spec->>'task_type'='application.start'",
            [&app.id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(op) = operation else { return Ok(None) };
    let checkpoint = db.find(&format!("oci-start-{op}"))?.ok_or_else(|| {
        err(
            "PreconditionFailed",
            "start has no verified container checkpoint",
        )
    })?;
    require(
        checkpoint.kind == "OCIStartCheckpoint"
            && checkpoint.body["application_ref"] == app.reference()
            && checkpoint.body["start_operation_id"] == op,
        "RecoveryRequired",
        "OCI checkpoint no longer matches the application",
    )?;
    Ok(Some(checkpoint))
}

pub fn start(w: &Work, checkpoint: impl FnOnce(&str, &str) -> Result<()>) -> Result<Value> {
    let e = Engine(&w.cfg);
    let s = w
        .launch
        .as_ref()
        .ok_or_else(|| err("PreconditionFailed", "missing validated release"))?;
    e.inspect_image(&s.image_id, s.health_check.as_ref())?;
    let engine = e.check_engine()?;
    require(
        !w.mount_observation_required || engine.unified_cgroups,
        "UnsupportedCapability",
        "effective OCI mount observation requires a local cgroup v2 engine",
    )?;
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    require(
        uid != 0,
        "UnsupportedCapability",
        "OCI profile requires non-root service identity",
    )?;
    let user = format!("{uid}:{gid}");
    let cname = format!("psios-{}", w.target.id);
    let mut args = [
        "create",
        "--pull=never",
        "--read-only",
        "--network=none",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--restart=no",
        "--ipc=none",
        "--log-driver=local",
        "--log-opt=max-size=1m",
        "--log-opt=max-file=1",
        "--log-opt=compress=false",
        "--stop-signal=SIGTERM",
        "--workdir=/",
        "--name",
        &cname,
        "--user",
        &user,
    ]
    .map(String::from)
    .to_vec();
    if s.health_check.is_none() {
        args.push("--no-healthcheck".into());
    }
    args.extend([
        format!("--label=io.psios.application={}", w.target.id),
        format!("--label=io.psios.operation={}", w.op.id),
        format!("--pids-limit={}", s.pids_max),
        format!("--memory={}", s.memory_max_bytes),
        format!("--memory-swap={}", s.memory_max_bytes),
        format!(
            "--cpus={}.{:03}",
            s.cpu_millicores / 1000,
            s.cpu_millicores % 1000
        ),
        format!("--tmpfs=/tmp:{}", tmpfs_options(s.ephemeral_max_bytes)),
    ]);
    if let Some(data) = &w.data {
        let path = data
            .to_str()
            .ok_or_else(|| err("UnsupportedCapability", "non-UTF8 volume root"))?;
        require(
            !path.contains([',', '"', '\n']),
            "UnsupportedCapability",
            "volume path unsupported by OCI adapter",
        )?;
        args.push(format!("--mount=type=bind,src={path},dst=/data,bind-propagation=rprivate,bind-recursive=disabled"));
    }
    for (k, v) in &s.environment {
        args.push(format!("--env={k}={v}"))
    }
    args.extend([format!("--entrypoint={}", s.argv[0]), s.image_id.clone()]);
    args.extend(s.argv[1..].iter().cloned());
    let created = e.command(&args, true)?;
    let cid = String::from_utf8_lossy(&created.stdout).trim().to_owned();
    require(
        !created.truncated && cid.len() == 64 && cid.bytes().all(|b| b.is_ascii_hexdigit()),
        "OutcomeUnknown",
        "invalid OCI create receipt",
    )?;
    let v = e.managed(w, &cid, &w.op.id).map_err(|_| {
        err(
            "OutcomeUnknown",
            "created instance identity requires reconciliation",
        )
    })?;
    if let Err(error) = verify_isolation(w, &v, s, &user) {
        if error.0.code == "PreconditionFailed"
            && e.discard_unstarted(w, &cid, &engine.id).unwrap_or(false)
        {
            return Err(error);
        }
        return Err(err("OutcomeUnknown", error.0.message));
    }
    require(
        e.identity().is_ok_and(|id| id == engine.id),
        "OutcomeUnknown",
        "engine changed after container creation",
    )?;
    if let Err(denial) = checkpoint(&engine.id, &cid) {
        if matches!(
            denial.0.code.as_str(),
            "PermissionDenied" | "RecoveryRequired" | "ResultEvicted"
        ) && e.discard_unstarted(w, &cid, &engine.id).unwrap_or(false)
        {
            return Err(denial);
        }
        return Err(err(
            "OutcomeUnknown",
            "created container could not be authorized and checkpointed before start",
        ));
    }
    e.command(&["start".into(), cid.clone()], true)
        .map_err(|_| err("OutcomeUnknown", "created instance start unconfirmed"))?;
    let observed = e
        .managed(w, &cid, &w.op.id)
        .map_err(|_| err("OutcomeUnknown", "start receipt requires reconciliation"))?;
    verify_isolation(w, &observed, s, &user)
        .map_err(|error| err("OutcomeUnknown", error.0.message))?;
    let state = &observed["State"];
    let running = state["Running"]
        .as_bool()
        .ok_or_else(|| err("OutcomeUnknown", "running state unavailable"))?;
    let limits = if running {
        engine
            .observe_limits(&cid, state, s)
            .map_err(|e| err("OutcomeUnknown", e.0.message))?
    } else {
        Value::Null
    };
    require(
        e.identity().is_ok_and(|id| id == engine.id),
        "OutcomeUnknown",
        "engine changed before the start receipt",
    )?;
    let mut report = json!({
        "container_id": cid,
        "engine_id":engine.id,
        "process_running": running,
        "effective_limits": limits,
        "observed_at": now()
    });
    e.observe_mount(w, &engine, &cid, &w.op.id, state, &mut report)
        .map_err(|error| err("OutcomeUnknown", error.0.message))?;
    health::status(&mut report, s.health_check.as_ref(), state);
    Ok(report)
}
fn tmpfs_options(bytes: u64) -> String {
    format!("rw,noexec,nosuid,nodev,size={bytes}")
}

fn no_entries(value: &Value) -> bool {
    value.is_null() || value.as_array().is_some_and(Vec::is_empty)
}

fn approved_mounts(w: &Work, v: &Value) -> bool {
    let h = &v["HostConfig"];
    let mounts = &v["Mounts"];
    let declarations = &h["Mounts"];
    let Some(data) = &w.data else {
        return no_entries(mounts) && no_entries(declarations);
    };
    let Some(source) = data.to_str() else {
        return false;
    };
    match (
        mounts.as_array().map(Vec::as_slice),
        declarations.as_array().map(Vec::as_slice),
    ) {
        (Some([mounted]), Some([declared])) => {
            mounted["Type"] == "bind"
                && mounted["Destination"] == "/data"
                && mounted["Source"] == source
                && mounted["RW"] == true
                && mounted["Propagation"] == "rprivate"
                && declared["Type"] == "bind"
                && declared["Target"] == "/data"
                && declared["Source"] == source
                && declared["BindOptions"]["Propagation"] == "rprivate"
                && declared["BindOptions"]["NonRecursive"] == true
        }
        _ => false,
    }
}

fn verify_isolation(w: &Work, v: &Value, s: &Install, user: &str) -> Result<()> {
    let h = &v["HostConfig"];
    let no_extra_access = [
        "Binds",
        "VolumesFrom",
        "CapAdd",
        "GroupAdd",
        "Devices",
        "DeviceRequests",
        "DeviceCgroupRules",
    ]
    .into_iter()
    .all(|name| no_entries(&h[name]));
    let no_new_privileges = h["SecurityOpt"].as_array().is_some_and(|options| {
        matches!(options.as_slice(), [option]
            if matches!(option.as_str(), Some("no-new-privileges" | "no-new-privileges:true")))
    });
    require(
        approved_mounts(w, v)
            && h["Tmpfs"] == json!({"/tmp":tmpfs_options(s.ephemeral_max_bytes)})
            && v["Config"]["User"] == user
            && h["NetworkMode"] == "none"
            && h["IpcMode"] == "none"
            && matches!(h["PidMode"].as_str(), Some("" | "private"))
            && h["ReadonlyRootfs"] == true
            && h["Privileged"] == false
            && h["Memory"] == s.memory_max_bytes
            && h["MemorySwap"] == s.memory_max_bytes
            && h["PidsLimit"] == s.pids_max
            && h["NanoCpus"] == u64::from(s.cpu_millicores) * 1_000_000
            && no_extra_access
            && h["CapDrop"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "ALL" || v == "CAP_ALL"))
            && no_new_privileges
            && h["RestartPolicy"]["Name"] == "no"
            && h["LogConfig"]["Type"] == "local"
            && h["LogConfig"]["Config"]["max-size"] == "1m"
            && h["LogConfig"]["Config"]["max-file"] == "1",
        "OutcomeUnknown",
        "OCI instance configuration differs from the approved isolation profile",
    )?;
    health::verify_runtime(s.health_check.as_ref(), v)
}

fn observe_start(w: &Work, e: &Engine<'_>, checkpoint: &Doc) -> Result<Value> {
    let c = &checkpoint.body;
    let engine = e.check_engine()?;
    require(
        c["engine_id"] == engine.id,
        "PreconditionFailed",
        "OCI checkpoint belongs to a different engine",
    )?;
    let cid = c["container_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpoint container ID missing"))?;
    let op = c["start_operation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpoint operation ID missing"))?;
    let v = e.managed(w, cid, op)?;
    let user = format!(
        "{}:{}",
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw()
    );
    let launch = w
        .launch
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "checkpoint release unavailable"))?;
    verify_isolation(w, &v, launch, &user).map_err(|_| {
        err(
            "PreconditionFailed",
            "checkpointed runtime configuration differs from its release",
        )
    })?;
    let state = &v["State"];
    let running = state["Running"] == true && state["Status"] == "running";
    let limits = if running {
        engine
            .observe_limits(cid, state, launch)
            .map_err(|e| err("PreconditionFailed", e.0.message))?
    } else {
        Value::Null
    };
    require(
        e.identity()? == engine.id,
        "PreconditionFailed",
        "OCI engine changed during observation",
    )?;
    let mut report = json!({"container_id":cid, "engine_id":engine.id, "start_operation_id":op,
        "checkpoint_ref":checkpoint.reference(), "process_running":running,
        "runtime_state":state["Status"], "effective_limits":limits,"observed_at":now()});
    e.observe_mount(w, &engine, cid, op, state, &mut report)?;
    health::status(&mut report, launch.health_check.as_ref(), state);
    Ok(report)
}

pub fn perform(w: &Work) -> Result<Value> {
    let e = Engine(&w.cfg);
    match &w.action {
        Action::InspectOciInstance(_) => restart::observe(w),
        Action::Install(s) | Action::Stage { release: s, .. } => {
            e.inspect_image(&s.image_id, s.health_check.as_ref())
        }
        Action::InstallArtifact(_) | Action::StageArtifactRelease(_) => {
            let s = artifact_spec(w)?;
            e.inspect_image(&s.image_id, s.health_check.as_ref())
        }
        Action::Select { release_ref, .. } => Ok(json!({"release_ref":release_ref})),
        Action::RetireRelease(_) => Ok(json!({})),
        Action::Health(_) if w.start_checkpoint.is_some() => {
            observe_start(w, &e, w.start_checkpoint.as_ref().unwrap())
        }
        Action::Uninstall(_) => Ok(json!({"data_retained": true})),
        Action::Logs(_) => log_tail::capture(w, &e),
        Action::Refresh(_) | Action::Health(_) if w.target.body["lifecycle"] == "Stopped" => {
            Ok(json!({
                "process_running": false,
                "runtime_state": "Absent",
                "health": "Unknown",
                "readiness": "NotReady",
                "observed_at": now()
            }))
        }
        Action::Refresh(_) | Action::Health(_) => {
            let cid = w.target.body["container_id"]
                .as_str()
                .ok_or_else(|| err("RecoveryRequired", "instance ID missing"))?;
            let start_op = w.target.body["start_operation_id"]
                .as_str()
                .ok_or_else(|| err("RecoveryRequired", "instance operation ID missing"))?;
            let v = match e.managed(w, cid, start_op) {
                Ok(v) => v,
                Err(_) if e.removed(w, cid)? => {
                    return Ok(
                        json!({"container_id":cid,"engine_id":w.target.body["engine_id"],
                        "start_operation_id":w.target.body["start_operation_id"],"removal_confirmed":true,
                        "process_running":false,"runtime_state":"Absent","readiness":"NotReady","health":"Unknown",
                        "observed_at":now()}),
                    );
                }
                Err(e) => return Err(e),
            };
            require(
                e.identity()? == w.target.body["engine_id"],
                "PreconditionFailed",
                "OCI engine changed during health observation",
            )?;
            health::verify_runtime(w.oci_health_check.as_ref(), &v)?;
            let mut observation = stop::health(&w.target.body, &v);
            health::status(&mut observation, w.oci_health_check.as_ref(), &v["State"]);
            Ok(observation)
        }
        _ => Err(err("UnsupportedCapability", "unknown OCI operation")),
    }
}
fn record_release(
    db: &Db<'_>,
    work: &Work,
    app: &str,
    has_volume: bool,
    s: &Install,
    r: &Value,
) -> Result<Doc> {
    let owner = if work.target.kind == "Application" {
        &work.target.owner
    } else {
        &work.task.owner
    };
    let release = id("release");
    let specid = id("release-spec");
    let profile = db.get("oci-local")?.reference();
    let archive = artifact_install::work_archive(db, work)?;
    let mut image = json!({"config_digest": s.image_id, "inspection": r, "source": "local-engine",
        "health_admission":health::record(db,work,s)?});
    if let Some(archive) = &archive {
        archive.check_image(r)?;
        image["image_archive_ref"] = archive.artifact.reference();
        image["image_capture_ref"] = archive.proof.reference();
    }
    let image = db.record("OCIImage", owner, image)?;
    let mut sources = vec![image.reference()];
    let (publisher, source_trust, admission) =
        match artifact_install::publisher_verification(db, work)? {
            Some(signer) => {
                sources.push(signer.artifact_ref);
                (
                    signer.publisher_ref,
                    "PublisherVerified",
                    signer.evidence_ref,
                )
            }
            None => (
                db.get(owner)?.reference(),
                "UserSuppliedUnverified",
                db.record(
                    "AdmissionRecord",
                    owner,
                    json!({"operation":work.op.id,
                "source_trust":"UserSuppliedUnverified","scope":"local-image-id"}),
                )?
                .reference(),
            ),
        };
    if let Some(archive) = archive {
        sources.push(archive.artifact.reference());
        sources.push(archive.proof.reference());
    }
    let license=db.record("LicenseMaterial",owner,json!({"declared_by": owner, "expression": s.license_expression, "verification": "Unverified"}))?.reference();
    let build = db
        .record(
            "BuildRecord",
            owner,
            json!({
                "source": "local OCI image metadata",
                "config_digest": s.image_id,
                "created": r["created"],
                "upstream_build_attestation": "Unavailable",
                "arch": r["architecture"]
            }),
        )?
        .reference();
    let sbom = db
        .record(
            "ComponentInventory",
            owner,
            json!({
                "scope": "OCI image and content layers",
                "image_config": s.image_id,
                "layers": r["layers"],
                "package_inventory": "Unavailable"
            }),
        )?
        .reference();
    let bindings = if has_volume {
        json!([{"manifest_volume_name":"data","mount_path":"/data","requested_access":"ReadWrite"}])
    } else {
        json!([])
    };
    let spec = json!({
        "schema_version": VERSION,
        "release_spec_id": specid,
        "execution": {"argv": s.argv, "working_directory": "/", "environment": s.environment},
        "lifecycle": {
            "stop_signal": "SIGTERM",
            "stop_grace_seconds": "5",
            "start_deadline_seconds": "20",
            "restart_policy": "Never",
            "max_restarts": "0"
        },
        "identity": {
            "execution_identity_profile_ref": db.get("oci-user")?.reference(),
            "namespace_mapping_profile_ref": db.get("oci-namespaces")?.reference()
        },
        "permission_requests": {
            "capability_refs": [],
            "secret_refs": [],
            "device_profile_refs": [],
            "volume_bindings": bindings
        },
        "resources": {
            "cpu_millicores": s.cpu_millicores.to_string(),
            "memory_max_bytes": s.memory_max_bytes.to_string(),
            "pids_max": s.pids_max.to_string(),
            "ephemeral_max_bytes": s.ephemeral_max_bytes.to_string(),
            "io_profile_ref": db.get("oci-io")?.reference()
        },
        "provenance": {
            "source_refs": sources,
            "license_expression": s.license_expression,
            "license_material_ref": license,
            "source_trust": source_trust,
            "trust_evidence_or_admission_policy_ref": admission,
            "build_record_ref": build,
            "sbom_ref": sbom
        },
        "dependency_release_refs": [],
        "backup_scope_ref": db.get("local-backup")?.reference()
    });
    let spec = db.create("ApplicationReleaseSpec", &specid, owner, json!({}), spec)?;
    let health_check_ref = health::pin(db, &spec, &image, s.health_check.as_ref())?;
    let volumes = if has_volume {
        json!([{"name":"data","minimum_bytes":"1","access_mode":"single_node_rw","schema_ref":db.get("local-files")?.reference()}])
    } else {
        json!([])
    };
    let manifest = json!({
        "schema_version": VERSION,
        "application_id": app,
        "release_id": release,
        "release_version": s.release_version,
        "artifact_ref": image.reference(),
        "publisher_identity_ref": publisher,
        "runtime_profile_ref": profile,
        "release_spec_ref": spec.reference(),
        "package_format": "OCI",
        "required_capability_refs": [],
        "volume_requirements": volumes,
        "network_policy_ref": db.get("oci-network")?.reference(),
        "health_check_ref": health_check_ref,
        "migration_plan_ref": db.get("local-migration")?.reference(),
        "rollback_policy_ref": db.get("local-rollback")?.reference(),
        "compatibility_claims": [{"capability":"Recognized","result":"Passed","profile_ref":profile,"evidence_refs":[image.reference()],"limitations":["Local image inspection only"]}]
    });
    db.create("ApplicationRelease", &release, owner, json!({}), manifest)
}
fn install(db: &Db<'_>, w: &Work, s: &Install, r: &Value) -> Result<Value> {
    let owner = &w.task.owner;
    let app = format!("app-{}", w.op.id);
    let release = record_release(db, w, &app, w.volume.is_some(), s, r)?;
    let deploy_spec = json!({
        "application_id": app,
        "release_ref": release.reference(),
        "environment": s.environment,
        "volume_id": w.volume.as_ref().map(|v|&v.id),
        "data_incarnation": w.volume.as_ref().map(|v|&v.body["data_incarnation"])
    });
    let deploy = db.create(
        "Deployment",
        &id("deployment"),
        owner,
        deploy_spec.clone(),
        json!({"lifecycle": "Prepared", "desired_state": "Stopped"}),
    )?;
    let spec = json!({
        "name": s.name,
        "release_ref": release.reference(),
        "image_id": s.image_id,
        "deployment_ref": deploy.reference(),
        "volume_id": deploy_spec["volume_id"],
        "data_incarnation": deploy_spec["data_incarnation"]
    });
    let application = db.create(
        "Application",
        &app,
        owner,
        spec,
        json!({
            "lifecycle": "Stopped",
            "desired_state": "Stopped",
            "runtime_state": "Absent",
            "health": "Unknown",
            "readiness": "NotReady",
            "observed_at": now()
        }),
    )?;
    Ok(
        artifact_install::retain(db, w, &application, &release, Some(&deploy))?
            .unwrap_or_else(|| application.reference()),
    )
}
fn attachment(db: &Db<'_>, app: &Doc, op_id: &str, v: &Doc, start: bool) -> Result<Value> {
    let mut b = v.body.clone();
    if start {
        let aid = id("attachment");
        let body = json!({
            "schema_version": VERSION,
            "attachment_id": aid,
            "volume_id": v.id,
            "data_incarnation": v.body["data_incarnation"],
            "node_id": db.meta("authority")?,
            "workload_id": app.id,
            "requested_access": "ReadWrite",
            "effective_access": "ReadWrite",
            "lifecycle": "Attached"
        });
        let a = db.create("VolumeAttachment", &aid, &app.owner, json!({}), body)?;
        b["attachment_refs"] = json!([a.reference()]);
        require(
            db.c.execute(
                "UPDATE holds SET holder=?1 WHERE volume=?2 AND holder=?3",
                params![app.id, v.id, op_id],
            )? == 1,
            "RecoveryRequired",
            "volume hold lost",
        )?;
    } else {
        let a = db.resolve(&b["attachment_refs"][0])?;
        require(
            a.body["workload_id"] == app.id
                && a.body["data_incarnation"] == v.body["data_incarnation"],
            "RecoveryRequired",
            "volume attachment identity changed",
        )?;
        let mut ab = a.body.clone();
        ab["effective_access"] = json!("Blocked");
        ab["lifecycle"] = json!("Detached");
        db.status(&a, ab)?;
        b["attachment_refs"] = json!([]);
        require(
            db.c.execute(
                "DELETE FROM holds WHERE volume=?1 AND holder=?2",
                params![v.id, app.id],
            )? == 1,
            "RecoveryRequired",
            "application volume hold is absent",
        )?;
    }
    Ok(db.status(v, b)?.reference())
}
pub fn stopped(
    db: &Db<'_>,
    app: &Doc,
    op_id: &str,
    volume: Option<&Doc>,
    tail: Option<&Value>,
) -> Result<Value> {
    if let Some(tail) = tail {
        log_tail::retain(db, app, op_id, tail)?;
    }
    if let Some(volume) = volume {
        attachment(db, app, op_id, volume, false)?;
    }
    let deploy = db.resolve(&app.spec["deployment_ref"])?;
    let deploy = db.status(
        &deploy,
        json!({"lifecycle":"Active","desired_state":"Stopped"}),
    )?;
    let instance = db.resolve(&app.body["instance_ref"])?;
    db.status(
        &instance,
        json!({"lifecycle":"Absent","observed_at":now(),"stop_operation_id":op_id}),
    )?;
    let mut spec = app.spec.clone();
    spec["deployment_ref"] = deploy.reference();
    Ok(db.update(app,spec,json!({"lifecycle":"Stopped","desired_state":"Stopped","runtime_state":"Absent","last_stop_operation_id":op_id,
        "health":"Unknown","readiness":"NotReady","observed_at":now()}))?.reference())
}
fn artifact_spec(work: &Work) -> Result<&Install> {
    work.launch
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "artifact release specification missing"))
}

fn stage(db: &Db<'_>, work: &Work, release: &Install, inspection: &Value) -> Result<Value> {
    let staged = record_release(
        db,
        work,
        &work.target.id,
        work.target.spec["volume_id"].is_string(),
        release,
        inspection,
    )?;
    Ok(
        artifact_install::retain(db, work, &work.target, &staged, None)?
            .unwrap_or_else(|| staged.reference()),
    )
}

pub fn apply(db: &Db<'_>, w: &Work, r: &Value) -> Result<Value> {
    match &w.action {
        Action::Install(s) => install(db, w, s, r),
        Action::InstallArtifact(_) => install(db, w, artifact_spec(w)?, r),
        Action::Stage { release, .. } => stage(db, w, release, r),
        Action::StageArtifactRelease(_) => stage(db, w, artifact_spec(w)?, r),
        Action::Select { release_ref, .. } => {
            let release = release_retirement::associated(db, &w.target, release_ref)?;
            let spec = db.resolve(&release.body["release_spec_ref"])?;
            let image = db.resolve(&release.body["artifact_ref"])?;
            select_deployment(
                db,
                &w.target,
                &release,
                &image.body["config_digest"],
                spec.body["execution"]["environment"].clone(),
            )
        }
        Action::RetireRelease(input) => release_retirement::retire(db, w, input),
        Action::Refresh(_) => {
            let mut b = w.target.body.clone();
            b["last_observation"] = r.clone();
            for field in ["runtime_state", "readiness", "health", "observed_at"] {
                b[field] = r[field].clone();
            }
            if let Some(reference) = w.target.body.get("instance_ref") {
                let instance = db.resolve(reference)?;
                let instance = db.status(
                    &instance,
                    json!({"lifecycle":r["runtime_state"],"observation":r}),
                )?;
                b["instance_ref"] = instance.reference();
            }
            Ok(db.status(&w.target, b)?.reference())
        }
        Action::Start(_) => started(db, &w.target, &w.op.id, w.volume.as_ref(), r, false),
        Action::RestartApp(_) => restart::settle(db, &w.op, &w.target, r),
        Action::Stop(_) => stopped(
            db,
            &w.target,
            &w.op.id,
            w.volume.as_ref(),
            r.get("retained_tail").filter(|tail| !tail.is_null()),
        ),
        Action::Uninstall(_) => {
            let deployment = db.resolve(&w.target.spec["deployment_ref"])?;
            let deployment = db.status(
                &deployment,
                json!({"lifecycle":"Removed","desired_state":"Removed"}),
            )?;
            let mut spec = w.target.spec.clone();
            spec["deployment_ref"] = deployment.reference();
            let body = json!({"lifecycle":"Uninstalled","data_retained":true,"desired_state":"Removed",
                "runtime_state":"Absent","health":"Unknown","observed_at":now(),"readiness":"NotReady"});
            Ok(db.update(&w.target, spec, body)?.reference())
        }
        _ => Ok(r.clone()),
    }
}
pub fn started(
    db: &Db<'_>,
    app: &Doc,
    op_id: &str,
    volume: Option<&Doc>,
    r: &Value,
    recovered: bool,
) -> Result<Value> {
    if let Some(v) = volume {
        let issued = db.find(&format!("oci-start-{op_id}"))?;
        require(
            !(db.cfg.oci_effective_volume_observation
                || issued.as_ref().is_some_and(mount_observation::required))
                || mount_observation::matches(v, r),
            "OutcomeUnknown",
            "OCI start has no verified effective volume mount",
        )?;
        attachment(db, app, op_id, v, true)?;
    }
    let deployment = db.resolve(&app.spec["deployment_ref"])?;
    let deployment = db.status(
        &deployment,
        json!({"lifecycle":"Active","desired_state":"Running"}),
    )?;
    let running = r["process_running"] == true;
    let runtime = if running { "Running" } else { "Exited" };
    let mut instance_body = json!({"lifecycle": runtime, "observed_at": r["observed_at"]});
    if r.get(mount_observation::FIELD).is_some() {
        instance_body["observation"] = r.clone();
    }
    let instance = db.create(
        "RuntimeInstance", &id("instance"), &app.owner,
        json!({
            "application_id": app.id, "deployment_ref": deployment.reference(),
            "container_id": r["container_id"], "engine_id": r["engine_id"], "start_operation_id": op_id
        }),
        instance_body,
    )?;
    let mut spec = app.spec.clone();
    spec["deployment_ref"] = deployment.reference();
    let mut b = app.body.clone();
    b["lifecycle"] = json!("Running");
    b["desired_state"] = json!("Running");
    b["runtime_state"] = json!(runtime);
    let (health, readiness) = health::projection(r, recovered);
    b["health"] = json!(health);
    b["readiness"] = json!(readiness);
    b["observed_at"] = r["observed_at"].clone();
    b["start_operation_id"] = json!(op_id);
    b["engine_id"] = r["engine_id"].clone();
    b["container_id"] = r["container_id"].clone();
    b["instance_ref"] = instance.reference();
    b["last_observation"] = r.clone();
    Ok(db.update(app, spec, b)?.reference())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    id: String,
    expected_resource_version: String,
    environment: BTreeMap<String, String>,
}
pub fn configure(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Configure = decode(p)?;
    policy::can_target(db, actor, "application.install", &p.id)?;
    environment(&p.environment)?;
    let d = db.of(&p.id, "Application")?;
    policy::owned(actor, &d)?;
    d.cas(&p.expected_resource_version)?;
    require(
        d.body["lifecycle"] == "Stopped"
            && !db.c.query_row(
                "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1)",
                [&d.id],
                |r| r.get::<_, bool>(0),
            )?,
        "Conflict",
        "configuration requires an idle stopped application",
    )?;
    let release = db.resolve(&d.spec["release_ref"])?;
    select_deployment(db, &d, &release, &d.spec["image_id"], json!(p.environment))
}
fn select_deployment(
    db: &Db<'_>,
    d: &Doc,
    release: &Doc,
    image: &Value,
    environment: Value,
) -> Result<Value> {
    release_retirement::available(db, release)?;
    let mut spec = d.spec.clone();
    let previous = db.resolve(&d.spec["deployment_ref"])?;
    db.status(
        &previous,
        json!({"lifecycle": "Superseded", "desired_state": "Stopped"}),
    )?;
    let mut config = previous.spec.clone();
    config.as_object_mut().unwrap().remove("restore_rebind");
    config["environment"] = environment;
    config["release_ref"] = release.reference();
    spec["release_ref"] = release.reference();
    spec["image_id"] = image.clone();
    let selected = db.create(
        "Deployment",
        &id("deployment"),
        &d.owner,
        config,
        json!({"lifecycle": "Prepared", "desired_state": "Stopped"}),
    )?;
    spec["deployment_ref"] = selected.reference();
    Ok(db.update(d, spec, d.body.clone())?.reference())
}
