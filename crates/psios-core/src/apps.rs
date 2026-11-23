//! OCI adapter for a trusted local Docker Engine. No registry pulls, shell or client-provided host paths.
use crate::{
    db::{Db, Doc},
    host::{Action, Install, Work},
    policy, *,
};
use psios_api::{VERSION, id};
use rusqlite::params;
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Command};

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
pub fn validate(s: &Install) -> Result<()> {
    label(&s.name)?;
    label(&s.release_version)?;
    label(&s.license_expression)?;
    require(
        s.image_id.len() == 71
            && s.image_id.starts_with("sha256:")
            && s.image_id[7..]
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
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
pub fn launch_spec(db: &Db<'_>, app: &Doc) -> Result<Install> {
    let deployment = db.resolve(&app.spec["deployment_ref"])?;
    require(
        deployment.kind == "Deployment"
            && deployment.spec["application_id"] == app.id
            && deployment.spec["release_ref"] == app.spec["release_ref"],
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
        "ephemeral_max_bytes": count("ephemeral_max_bytes")?
    }))?;
    validate(&launch)?;
    Ok(launch)
}
pub fn preflight(db: &Db<'_>, w: &Work) -> Result<()> {
    match w.action {
        Action::Stage { ref release, .. } => require(
            w.target.body["lifecycle"] != "Uninstalled" && w.target.spec["name"] == release.name,
            "PreconditionFailed",
            "staged release must belong to an installed application with the same name",
        )?,
        Action::Select {
            ref release_ref, ..
        } => {
            let release = db.resolve(release_ref)?;
            require(
                w.target.body["lifecycle"] == "Stopped"
                    && release.kind == "ApplicationRelease"
                    && release.body["application_id"] == w.target.id
                    && release.owner == w.target.owner,
                "PreconditionFailed",
                "release selection requires a stopped application and its own immutable release",
            )?;
        }
        Action::Start(_) | Action::Uninstall(_) => require(
            w.target.body["lifecycle"] == "Stopped",
            "PreconditionFailed",
            "application must be stopped",
        )?,
        Action::Stop(_) => require(
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
    if matches!(w.action, Action::Start(_))
        && let Some(v) = &w.volume
    {
        require(
            w.target.spec["data_incarnation"] == v.body["data_incarnation"],
            "PreconditionFailed",
            "volume incarnation changed",
        )?;
    }
    Ok(())
}
struct Engine<'a>(&'a Config);
impl Engine<'_> {
    fn command(&self, args: &[String], effect: bool) -> Result<crate::command::Output> {
        let mut cmd = Command::new("/usr/bin/docker");
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("--host")
            .arg(format!("unix://{}", self.0.docker_socket.display()))
            .args(args);
        crate::command::capture(cmd, effect, "Docker", 20)
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
        Ok(json!({
            "config_digest": image,
            "os": v["Os"],
            "architecture": v["Architecture"],
            "created": v["Created"],
            "layers": v["RootFS"]["Layers"],
            "inspected_at": now()
        }))
    }
    fn check_engine(&self) -> Result<()> {
        let out = self.command(&["info", "--format", "{{json .}}"].map(String::from), false)?;
        let v: Value = serde_json::from_slice(&out.stdout)
            .map_err(|_| err("DependencyUnavailable", "cannot inspect engine limits"))?;
        require(
            v["MemoryLimit"] == true
                && v["CpuCfsQuota"] == true
                && v["PidsLimit"] == true
                && v["SecurityOptions"].as_array().is_some_and(|a| {
                    a.iter()
                        .any(|x| x.as_str().is_some_and(|x| x.contains("seccomp")))
                }),
            "UnsupportedCapability",
            "engine lacks required cgroup or seccomp enforcement",
        )
    }
    fn managed(&self, w: &Work, id: &str) -> Result<Value> {
        let v = self.inspect("container", id)?;
        require(
            v["Id"] == id
                && v["Config"]["Labels"]["io.psios.application"] == w.target.id
                && v["Image"] == w.target.spec["image_id"],
            "PreconditionFailed",
            "container identity mismatch",
        )?;
        Ok(v)
    }
}
fn start(w: &Work, e: &Engine<'_>) -> Result<Value> {
    let s = w
        .launch
        .as_ref()
        .ok_or_else(|| err("PreconditionFailed", "missing validated release"))?;
    e.image(&s.image_id)?;
    e.check_engine()?;
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
        "--no-healthcheck",
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
        format!(
            "--tmpfs=/tmp:rw,noexec,nosuid,nodev,size={}",
            s.ephemeral_max_bytes
        ),
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
        cid.len() == 64 && cid.bytes().all(|b| b.is_ascii_hexdigit()),
        "OutcomeUnknown",
        "invalid OCI create receipt",
    )?;
    let v = e.managed(w, &cid).map_err(|_| {
        err(
            "OutcomeUnknown",
            "created instance identity requires reconciliation",
        )
    })?;
    let h = &v["HostConfig"];
    let valid_mount = match &w.data {
        Some(data) => v["Mounts"].as_array().is_some_and(|a| {
            a.iter().any(|m| {
                m["Destination"] == "/data"
                    && m["Source"] == data.to_string_lossy().as_ref()
                    && m["RW"] == true
            })
        }),
        None => true,
    };
    require(
        valid_mount
            && v["Config"]["User"] == user
            && h["NetworkMode"] == "none"
            && h["ReadonlyRootfs"] == true
            && h["Privileged"] == false
            && h["Memory"] == s.memory_max_bytes
            && h["PidsLimit"] == s.pids_max
            && h["NanoCpus"] == u64::from(s.cpu_millicores) * 1_000_000
            && h["CapDrop"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "ALL" || v == "CAP_ALL"))
            && h["SecurityOpt"].as_array().is_some_and(|a| {
                a.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|s| s.starts_with("no-new-privileges"))
                })
            }),
        "OutcomeUnknown",
        "created instance failed isolation verification; start withheld",
    )?;
    e.command(&["start".into(), cid.clone()], true)?;
    let observed = e
        .managed(w, &cid)
        .map_err(|_| err("OutcomeUnknown", "start receipt requires reconciliation"))?;
    Ok(json!({
        "container_id": cid,
        "process_running": observed["State"]["Running"],
        "observed_at": now()
    }))
}
fn logs(e: &Engine<'_>, id: &str) -> Result<Value> {
    let out = e.command(&["logs", "--tail", "64", id].map(String::from), false)?;
    // Keep result + transport encoding inside the frame even for strings consisting of control bytes.
    let clip = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(8192)]).into_owned();
    Ok(json!({
        "stdout": clip(&out.stdout),
        "stderr": clip(&out.stderr),
        "truncated": out.truncated||out.stdout.len()>8192||out.stderr.len()>8192
    }))
}
pub fn perform(w: &Work) -> Result<Value> {
    let e = Engine(&w.cfg);
    match &w.action {
        Action::Install(s) | Action::Stage { release: s, .. } => e.image(&s.image_id),
        Action::Select { release_ref, .. } => Ok(json!({"release_ref":release_ref})),
        Action::Start(_) => start(w, &e),
        Action::Uninstall(_) => Ok(json!({"data_retained": true})),
        Action::Refresh(_) | Action::Logs(_) | Action::Health(_)
            if w.target.body["lifecycle"] == "Stopped" =>
        {
            Ok(if matches!(w.action, Action::Logs(_)) {
                w.target.body["last_logs"].clone()
            } else {
                json!({
                    "process_running": false,
                    "runtime_state": "Absent",
                    "health": "Unknown",
                    "readiness": "NotReady",
                    "observed_at": now()
                })
            })
        }
        Action::Refresh(_) | Action::Logs(_) | Action::Health(_) | Action::Stop(_) => {
            let cid = w.target.body["container_id"]
                .as_str()
                .ok_or_else(|| err("RecoveryRequired", "instance ID missing"))?;
            let v = e.managed(w, cid)?;
            if matches!(w.action, Action::Logs(_)) {
                return logs(&e, cid);
            }
            if matches!(w.action, Action::Refresh(_) | Action::Health(_)) {
                return Ok(json!({
                    "container_id": cid,
                    "process_running": v["State"]["Running"],
                    "exit_code": v["State"]["ExitCode"],
                    "runtime_state": if v["State"]["Running"]==true{"Running"}else{"Exited"},
                    "health": "Unknown",
                    "readiness": if v["State"]["Running"]==true{"Unknown"}else{"NotReady"},
                    "observed_at": now()
                }));
            }
            e.command(&["stop", "--time=5", cid].map(String::from), true)?;
            let v = e
                .managed(w, cid)
                .map_err(|_| err("OutcomeUnknown", "unable to verify stop"))?;
            require(
                v["State"]["Running"] == false,
                "OutcomeUnknown",
                "instance stop unconfirmed",
            )?;
            let tail = logs(&e, cid).unwrap_or_else(|_| json!({"unavailable": true}));
            e.command(&["rm", cid].map(String::from), true)?;
            Ok(json!({"last_logs": tail}))
        }
        _ => Err(err("UnsupportedCapability", "unknown OCI operation")),
    }
}
fn record_release(
    db: &Db<'_>,
    operation: &str,
    app: &str,
    owner: &str,
    has_volume: bool,
    s: &Install,
    r: &Value,
) -> Result<Doc> {
    let release = id("release");
    let specid = id("release-spec");
    let profile = db.get("oci-local")?.reference();
    let publisher = db.get(owner)?.reference();
    let image = db.record(
        "OCIImage",
        owner,
        json!({"config_digest": s.image_id, "inspection": r, "source": "local-engine"}),
    )?;
    let admission=db.record("AdmissionRecord",owner,json!({"operation": operation, "source_trust": "UserSuppliedUnverified", "scope": "local-image-id"}))?.reference();
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
            "source_refs": [image.reference()],
            "license_expression": s.license_expression,
            "license_material_ref": license,
            "source_trust": "UserSuppliedUnverified",
            "trust_evidence_or_admission_policy_ref": admission,
            "build_record_ref": build,
            "sbom_ref": sbom
        },
        "dependency_release_refs": [],
        "backup_scope_ref": db.get("local-backup")?.reference()
    });
    let spec = db.create("ApplicationReleaseSpec", &specid, owner, json!({}), spec)?;
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
        "health_check_ref": db.get("oci-health")?.reference(),
        "migration_plan_ref": db.get("local-migration")?.reference(),
        "rollback_policy_ref": db.get("local-rollback")?.reference(),
        "compatibility_claims": [{"capability":"Recognized","result":"Passed","profile_ref":profile,"evidence_refs":[image.reference()],"limitations":["Local image inspection only"]}]
    });
    db.create("ApplicationRelease", &release, owner, json!({}), manifest)
}
fn install(db: &Db<'_>, w: &Work, s: &Install, r: &Value) -> Result<Value> {
    let owner = &w.task.owner;
    let app = format!("app-{}", w.op.id);
    let release = record_release(db, &w.op.id, &app, owner, w.volume.is_some(), s, r)?;
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
    Ok(db
        .create(
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
                "last_logs": null,
                "observed_at": now()
            }),
        )?
        .reference())
}
fn attachment(db: &Db<'_>, w: &Work, v: &Doc, start: bool) -> Result<Value> {
    let mut b = v.body.clone();
    if start {
        let aid = id("attachment");
        let body = json!({
            "schema_version": VERSION,
            "attachment_id": aid,
            "volume_id": v.id,
            "data_incarnation": v.body["data_incarnation"],
            "node_id": db.meta("authority")?,
            "workload_id": w.target.id,
            "requested_access": "ReadWrite",
            "effective_access": "ReadWrite",
            "lifecycle": "Attached"
        });
        let a = db.create("VolumeAttachment", &aid, &w.target.owner, json!({}), body)?;
        b["attachment_refs"] = json!([a.reference()]);
        require(
            db.c.execute(
                "UPDATE holds SET holder=?1 WHERE volume=?2 AND holder=?3",
                params![w.target.id, v.id, w.op.id],
            )? == 1,
            "RecoveryRequired",
            "volume hold lost",
        )?;
    } else {
        let a = db.resolve(&b["attachment_refs"][0])?;
        let mut ab = a.body.clone();
        ab["effective_access"] = json!("Blocked");
        ab["lifecycle"] = json!("Detached");
        db.status(&a, ab)?;
        b["attachment_refs"] = json!([]);
        db.c.execute(
            "DELETE FROM holds WHERE volume=?1 AND holder=?2",
            params![v.id, w.target.id],
        )?;
    }
    Ok(db.status(v, b)?.reference())
}
pub fn apply(db: &Db<'_>, w: &Work, r: &Value) -> Result<Value> {
    let mut b = w.target.body.clone();
    match &w.action {
        Action::Install(s) => return install(db, w, s, r),
        Action::Stage { release, .. } => {
            let staged = record_release(
                db,
                &w.op.id,
                &w.target.id,
                &w.target.owner,
                w.target.spec["volume_id"].is_string(),
                release,
                r,
            )?;
            return Ok(staged.reference());
        }
        Action::Select { release_ref, .. } => {
            let release = db.resolve(release_ref)?;
            let spec = db.resolve(&release.body["release_spec_ref"])?;
            let image = db.resolve(&release.body["artifact_ref"])?;
            return select_deployment(
                db,
                &w.target,
                &release,
                &image.body["config_digest"],
                spec.body["execution"]["environment"].clone(),
            );
        }
        Action::Refresh(_) => {
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
            return Ok(db.status(&w.target, b)?.reference());
        }
        Action::Start(_) => {
            b["lifecycle"] = json!("Running");
            b["container_id"] = r["container_id"].clone();
            b["start_operation_id"] = json!(w.op.id);
            b["last_observation"] = r.clone();
        }
        Action::Stop(_) => {
            b = json!({"lifecycle": "Stopped", "last_logs": r["last_logs"]});
        }
        Action::Uninstall(_) => {
            b = json!({"lifecycle": "Uninstalled", "data_retained": true});
        }
        _ => return Ok(r.clone()),
    }
    if matches!(w.action, Action::Start(_) | Action::Stop(_))
        && let Some(v) = &w.volume
    {
        attachment(db, w, v, matches!(w.action, Action::Start(_)))?;
    }
    let deployment = db.resolve(&w.target.spec["deployment_ref"])?;
    let (desired, phase, runtime) = match &w.action {
        Action::Start(_) => (
            "Running",
            "Active",
            if r["process_running"] == true {
                "Running"
            } else {
                "Exited"
            },
        ),
        Action::Stop(_) => ("Stopped", "Active", "Absent"),
        _ => ("Removed", "Removed", "Absent"),
    };
    let deployment = db.status(
        &deployment,
        json!({"lifecycle": phase, "desired_state": desired}),
    )?;
    let mut spec = w.target.spec.clone();
    spec["deployment_ref"] = deployment.reference();
    if matches!(w.action, Action::Start(_)) {
        let instance = db.create(
            "RuntimeInstance",
            &id("instance"),
            &w.target.owner,
            json!({
                "application_id": w.target.id,
                "deployment_ref": deployment.reference(),
                "container_id": r["container_id"],
                "start_operation_id": w.op.id
            }),
            json!({"lifecycle": runtime, "observed_at": r["observed_at"]}),
        )?;
        b["instance_ref"] = instance.reference();
    } else if matches!(w.action, Action::Stop(_)) {
        let instance = db.resolve(&w.target.body["instance_ref"])?;
        db.status(
            &instance,
            json!({"lifecycle": "Absent", "observed_at": now(), "stop_operation_id": w.op.id}),
        )?;
    }
    b["desired_state"] = json!(desired);
    b["runtime_state"] = json!(runtime);
    b["health"] = json!("Unknown");
    b["observed_at"] = json!(now());
    b["readiness"] = json!(if runtime == "Running" {
        "Unknown"
    } else {
        "NotReady"
    });
    Ok(db.update(&w.target, spec, b)?.reference())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    id: String,
    expected_resource_version: String,
    environment: BTreeMap<String, String>,
}
pub fn configure(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "application.install")?;
    let p: Configure = decode(p)?;
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
    let mut spec = d.spec.clone();
    let previous = db.resolve(&d.spec["deployment_ref"])?;
    db.status(
        &previous,
        json!({"lifecycle": "Superseded", "desired_state": "Stopped"}),
    )?;
    let mut config = previous.spec.clone();
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
