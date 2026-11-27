//! Observe the actual OCI bind mount from a pinned Linux process.
use super::cgroup::Process;
use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::VolumeAccess,
    now, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, Mode, OFlags, StatxFlags, fstat, open, openat, statx},
    io::Errno,
};
use serde_json::{Value, json};
use std::{
    fs::File,
    io,
    time::{Duration, Instant},
};

pub(crate) const FORMAT: u32 = crate::schedule::admission_delay::FORMAT + 1;
pub(crate) const READ_ONLY_FORMAT: u32 = crate::node::block::PAGED_INVENTORY_FORMAT + 1;
const PROFILE: &str = "LinuxLocalOciVolumeV1";
const EPOCH: &str = "oci-effective-mount-epoch";
pub(super) const REQUIRED: &str = "mount_observation_required";
pub(super) const FIELD: &str = "mount_observation";

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn read(fd: &OwnedFd) -> Result<Self> {
        let meta = fstat(fd).map_err(io::Error::from)?;
        Ok(Self {
            device: meta.st_dev,
            inode: meta.st_ino,
        })
    }
}

fn process_file(
    process: &Process,
    name: &str,
    flags: OFlags,
    deadline: Instant,
) -> Result<OwnedFd> {
    loop {
        match openat(&process.directory, name, flags, Mode::empty()) {
            Ok(fd) => return Ok(fd),
            // Docker can report Running while its init is still crossing the
            // approved UID and exec boundary, when procfs access is denied.
            // Only re-read the same pinned process, never repeat a host command.
            Err(Errno::ACCESS | Errno::PERM) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(Errno::ACCESS | Errno::PERM) => {
                return Err(err(
                    "DependencyUnavailable",
                    "Linux denied access to the required OCI mount observation",
                ));
            }
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
}

fn directory(process: &Process, deadline: Instant) -> Result<OwnedFd> {
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    // The fixed /proc/PID/root entry is a kernel magic link. Dereference only
    // that entry and forbid symlinks when opening the approved mount point.
    let root = process_file(process, "root", flags, deadline)?;
    openat(root, "data", flags | OFlags::NOFOLLOW, Mode::empty())
        .map_err(io::Error::from)
        .map_err(Into::into)
}

fn mount_id(fd: &OwnedFd) -> Result<u64> {
    let meta = statx(fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID).map_err(io::Error::from)?;
    require(
        StatxFlags::from_bits_retain(meta.stx_mask).contains(StatxFlags::MNT_ID)
            && meta.stx_mnt_id > 0,
        "UnsupportedCapability",
        "effective mount identity is unavailable from Linux",
    )?;
    Ok(meta.stx_mnt_id)
}

fn namespace(process: &Process, deadline: Instant) -> Result<OwnedFd> {
    process_file(
        process,
        "ns/mnt",
        OFlags::RDONLY | OFlags::CLOEXEC,
        deadline,
    )
}

fn inspect(process: &Process, mount: u64, access: VolumeAccess, deadline: Instant) -> Result<()> {
    let table = process_file(
        process,
        "mountinfo",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        deadline,
    )?;
    crate::node::mounts::private_data_mount(File::from(table), mount, access.read_only())
}

pub(super) fn observe(
    volume: &Doc,
    access: VolumeAccess,
    engine: &str,
    container: &str,
    state: &Value,
    confirm: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    require(
        state["Running"] == true && state["Status"] == "running" && state["Paused"] == false,
        "PreconditionFailed",
        "the OCI invocation has no observable running data mount",
    )?;
    let started = state["StartedAt"]
        .as_str()
        .filter(|at| valid_time(at))
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "OCI invocation start identity is unavailable",
            )
        })?;
    let boot = super::restart::boot_id()?;
    let process = Process::pin(container, state)?;
    let access_deadline = Instant::now() + Duration::from_millis(250);
    let mount_namespace = namespace(&process, access_deadline)?;
    let held_namespace = Identity::read(&mount_namespace)?;
    let service_namespace = open(
        "/proc/self/ns/mnt",
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    require(
        held_namespace != Identity::read(&service_namespace)?,
        "PreconditionFailed",
        "OCI data mount is visible in the service's own mount namespace",
    )?;
    let data = directory(&process, access_deadline)?;
    let held_data = Identity::read(&data)?;
    let device = held_data.device.to_string();
    let inode = held_data.inode.to_string();
    require(
        volume.spec["device"].as_str() == Some(device.as_str())
            && volume.spec["inode"].as_str() == Some(inode.as_str()),
        "PreconditionFailed",
        "effective OCI /data root differs from the approved volume inode",
    )?;
    let mount = mount_id(&data)?;
    inspect(&process, mount, access, access_deadline)?;
    let latest = confirm()?;
    process.confirm(container)?;
    let current = directory(&process, access_deadline)?;
    require(
        latest["Running"] == true
            && latest["Status"] == "running"
            && latest["Paused"] == false
            && latest["Pid"] == process.pid
            && latest["StartedAt"] == started
            && Identity::read(&current)? == held_data
            && mount_id(&current)? == mount
            && Identity::read(&namespace(&process, access_deadline)?)? == held_namespace
            && super::restart::boot_id()? == boot,
        "PreconditionFailed",
        "OCI mount or invocation identity changed during observation",
    )?;
    inspect(&process, mount, access, access_deadline)?;
    process.confirm(container)?;
    Ok(json!({
        "profile":PROFILE,"volume_ref":volume.reference(),"data_incarnation":volume.body["data_incarnation"],
        "boot_id":boot,"engine_id":engine,"container_id":container,"host_pid":process.pid,"started_at":started,
        "mount_namespace_device":held_namespace.device.to_string(),"mount_namespace_inode":held_namespace.inode.to_string(),
        "mount_id":mount.to_string(),"device":device,"inode":inode,
        "effective_access":access,"observed_at":now()
    }))
}

fn valid_time(value: &str) -> bool {
    value.len() <= 40 && chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

fn positive(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        text.parse::<u64>()
            .is_ok_and(|value| value > 0 && value.to_string() == text)
    })
}

pub(super) fn matches(volume: &Doc, report: &Value, access: VolumeAccess) -> bool {
    let proof = &report[FIELD];
    proof.as_object().is_some_and(|map| map.len() == 15)
        && proof["profile"] == PROFILE
        && proof["volume_ref"] == volume.reference()
        && proof["data_incarnation"] == volume.body["data_incarnation"]
        && proof["engine_id"] == report["engine_id"]
        && proof["container_id"] == report["container_id"]
        && proof["device"] == volume.spec["device"]
        && proof["inode"] == volume.spec["inode"]
        && proof["effective_access"] == json!(access)
        && proof["host_pid"] == report["effective_limits"]["host_pid"]
        && report["effective_limits"]["source"] == "LinuxCgroupV2"
        && proof["started_at"].as_str().is_some_and(valid_time)
        && report
            .get("started_at")
            .is_none_or(|at| *at == proof["started_at"])
        && proof["observed_at"].as_str().is_some_and(valid_time)
        && proof["boot_id"].as_str().is_some_and(|value| {
            uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
        })
        && report
            .get("boot_id")
            .is_none_or(|boot| *boot == proof["boot_id"])
        && positive(&proof["mount_namespace_device"])
        && positive(&proof["mount_namespace_inode"])
        && positive(&proof["mount_id"])
        && positive(&proof["device"])
        && positive(&proof["inode"])
}

pub(crate) fn required(checkpoint: &Doc) -> bool {
    checkpoint.body[REQUIRED] == true
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    let boundary: i64 =
        db.c.query_row("SELECT coalesce(max(seq),0) FROM versions", [], |row| {
            row.get(0)
        })?;
    require(
        boundary > 0,
        "RecoveryRequired",
        "effective OCI mount observation requires a bootstrapped authority",
    )?;
    db.c.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2)",
        params![EPOCH, boundary.to_string()],
    )?;
    Ok(())
}

fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [EPOCH], |row| {
        row.get::<_, String>(0)
    })
    .optional()?
    .and_then(|value| value.parse::<i64>().ok())
    .filter(|value| *value > 0)
    .ok_or_else(|| {
        err(
            "RecoveryRequired",
            "effective OCI mount observation epoch is missing",
        )
    })
}

fn check_checkpoint(conn: &Connection, checkpoint: &Doc) -> Result<()> {
    require(
        checkpoint.body[REQUIRED].is_boolean(),
        "RecoveryRequired",
        "OCI checkpoint lost its effective mount observation requirement",
    )?;
    let app_ref = if checkpoint.kind == "OCIStartCheckpoint" {
        &checkpoint.body["application_ref"]
    } else {
        &checkpoint.body["before"]["application_ref"]
    };
    let application = crate::db::resolve(conn, app_ref)?;
    let access = super::selected_volume_access(conn, &application)?;
    require(
        !access.read_only() || required(checkpoint),
        "RecoveryRequired",
        "read-only OCI checkpoint lost its effective mount observation requirement",
    )?;
    if required(checkpoint) {
        let volume = crate::db::resolve(conn, &checkpoint.body["volume_ref"])?;
        require(
            volume.kind == "Volume"
                && application.kind == "Application"
                && application.spec["volume_id"] == volume.id
                && application.spec["data_incarnation"] == volume.body["data_incarnation"]
                && (checkpoint.kind != "OCIRestartCheckpoint"
                    || matches(&volume, &checkpoint.body["before"], access)),
            "RecoveryRequired",
            "required OCI checkpoint has no matching effective volume mount",
        )?;
    }
    Ok(())
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    let boundary = epoch(conn)?;
    let mut statement = conn.prepare(
        "SELECT v.* FROM queue q JOIN versions v
         ON v.id='oci-start-'||q.operation OR v.id='oci-restart-'||q.operation
         WHERE q.phase IN ('Dispatched','Unknown') AND v.seq>?1
           AND v.kind IN ('OCIStartCheckpoint','OCIRestartCheckpoint')",
    )?;
    let mut records = statement.query([boundary])?;
    while let Some(row) = records.next()? {
        check_checkpoint(conn, &crate::db::row(row)?)?;
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let boundary = epoch(conn)?;
    let mut statement = conn.prepare(
        "SELECT * FROM versions WHERE seq>?1 AND kind IN ('OCIStartCheckpoint','OCIRestartCheckpoint')",
    )?;
    let mut records = statement.query([boundary])?;
    while let Some(row) = records.next()? {
        let checkpoint = crate::db::row(row)?;
        check_checkpoint(conn, &checkpoint)?;
        if required(&checkpoint) {
            check_settlement(conn, &checkpoint)?;
        }
    }
    Ok(())
}

fn check_settlement(conn: &Connection, checkpoint: &Doc) -> Result<()> {
    let start = checkpoint.kind == "OCIStartCheckpoint";
    let operation_id = if start {
        &checkpoint.body["start_operation_id"]
    } else {
        &checkpoint.spec["operation_id"]
    };
    let operation = crate::db::find(conn, operation_id.as_str().unwrap_or_default())?
        .filter(|record| record.kind == "Operation")
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "required OCI mount checkpoint has no Operation",
            )
        })?;
    if operation.body["outcome_knowledge"] != "Known"
        || !matches!(
            operation.body["lifecycle"].as_str(),
            Some("Succeeded" | "Cancelled")
        )
    {
        return Ok(());
    }
    let application = crate::db::resolve(
        conn,
        if start {
            &checkpoint.body["application_ref"]
        } else {
            &checkpoint.body["before"]["application_ref"]
        },
    )?;
    let reference = checkpoint.reference();
    let app = conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev>?2 AND kind='Application'
          AND ((?3 AND body->>'start_operation_id'=?4) OR (NOT ?3
           AND body->'last_restart_checkpoint_ref'->>'resource_id'=?5))
          ORDER BY rev LIMIT 1",
            params![
                application.id,
                application.rev,
                start,
                operation.id,
                checkpoint.id
            ],
            crate::db::row,
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "settled OCI invocation has no mount observation",
            )
        })?;
    let volume = crate::db::resolve(conn, &checkpoint.body["volume_ref"])?;
    let instance = crate::db::resolve(conn, &app.body["instance_ref"])?;
    let report = &app.body["last_observation"];
    let access = super::selected_volume_access(conn, &application)?;
    require(
        (start || app.body["last_restart_checkpoint_ref"] == reference)
            && matches(&volume, report, access)
            && instance.kind == "RuntimeInstance"
            && instance.spec["application_id"] == app.id
            && instance.spec["container_id"] == report["container_id"]
            && instance.spec["engine_id"] == report["engine_id"]
            && instance.body["observation"][FIELD] == report[FIELD],
        "RecoveryRequired",
        "settled OCI invocation has no verified effective volume mount",
    )
}
