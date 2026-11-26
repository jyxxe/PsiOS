//! Report kernel resource accounting for one allowlisted, running systemd invocation.
mod limits;

use super::{
    block, configured_service, pressure, process::custody, service_control, service_manager,
};
use crate::{Result, config::ServiceConfig, err, host::Work, linux_cgroup, now, require};
use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, StatxFlags, fstat, statx},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs, io,
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

pub(crate) const TOOL: &str = "service.resources";
pub(crate) const FORMAT: u32 = crate::agent::default_context::FORMAT + 1;
const MAX_PATH_BYTES: usize = 2048;
const MAX_DEPTH: usize = 24;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub(crate) id: String,
    pub(crate) expected_resource_version: String,
    expected_boot_id: String,
    expected_invocation_id: String,
}

impl Query {
    pub(crate) fn validate(&self) -> Result<()> {
        crate::label(&self.id)?;
        require(
            block::valid_boot(&self.expected_boot_id)
                && super::valid_invocation(&self.expected_invocation_id),
            "InvalidArgument",
            "service resources requires exact boot and invocation identities",
        )
    }
}

#[derive(PartialEq, Eq)]
struct Invocation {
    pid: u32,
    group_path: String,
    group_id: u64,
}

impl Invocation {
    fn get(config: &ServiceConfig, query: &Query) -> Result<Self> {
        let mut command = service_manager::command(&config.manager, "show");
        command.arg("--property=Id,LoadState,ActiveState,InvocationID,MainPID,ControlGroup,ControlGroupId")
            .arg("--").arg(&config.unit);
        let output = crate::command::capture(command, false, "systemd service resources", 5)?;
        require(
            !output.truncated,
            "DependencyUnavailable",
            "systemd resource response exceeds limit",
        )?;
        let body = std::str::from_utf8(&output.stdout)
            .map_err(|_| err("DependencyUnavailable", "invalid systemd resource response"))?;
        let mut fields = BTreeMap::new();
        for line in body.lines() {
            let Some((key, value)) = line.split_once('=') else {
                return Err(err(
                    "DependencyUnavailable",
                    "invalid systemd resource property",
                ));
            };
            require(
                fields.insert(key, value).is_none(),
                "DependencyUnavailable",
                "duplicate systemd resource property",
            )?;
        }
        require(
            fields.get("Id") == Some(&config.unit.as_str())
                && fields.get("LoadState") == Some(&"loaded")
                && fields.get("ActiveState") == Some(&"active")
                && fields.get("InvocationID") == Some(&query.expected_invocation_id.as_str()),
            "PreconditionFailed",
            "configured service invocation is no longer active",
        )?;
        let pid = fields
            .get("MainPID")
            .and_then(|pid| canonical_nonzero(pid))
            .filter(|pid: &u32| *pid <= i32::MAX as u32)
            .ok_or_else(|| {
                err(
                    "PreconditionFailed",
                    "systemd service has no verified main process",
                )
            })?;
        let group_path = fields.get("ControlGroup").copied().unwrap_or("");
        group_relative(group_path)?;
        let group_id = fields
            .get("ControlGroupId")
            .and_then(|id| canonical_nonzero(id))
            .ok_or_else(|| {
                err(
                    "UnsupportedCapability",
                    "systemd did not provide a cgroup v2 identity",
                )
            })?;
        Ok(Self {
            pid,
            group_path: group_path.into(),
            group_id,
        })
    }
}

fn canonical_nonzero<T: std::str::FromStr + ToString + Default + PartialEq>(
    value: &str,
) -> Option<T> {
    let n: T = value.parse().ok()?;
    (n != T::default() && n.to_string() == value).then_some(n)
}

fn group_relative(path: &str) -> Result<&Path> {
    let relative = path
        .strip_prefix('/')
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "service has no child cgroup in the observed mount",
            )
        })?;
    require(
        path.len() <= MAX_PATH_BYTES && relative.split('/').count() <= MAX_DEPTH,
        "QuotaExceeded",
        "service cgroup ancestry exceeds profile",
    )?;
    require(
        !relative.chars().any(char::is_control)
            && relative
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".."))
            && Path::new(relative)
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "PreconditionFailed",
        "systemd cgroup path is outside the observed hierarchy",
    )?;
    Ok(Path::new(relative))
}

fn kernel_error(error: io::Error) -> crate::Error {
    match error.kind() {
        io::ErrorKind::Unsupported => err(
            "UnsupportedCapability",
            "local cgroup v2 filesystem is unavailable",
        ),
        io::ErrorKind::PermissionDenied => err(
            "PermissionDenied",
            "kernel denied service cgroup observation",
        ),
        io::ErrorKind::NotFound | io::ErrorKind::Interrupted => err(
            "PreconditionFailed",
            "service cgroup changed or is outside the observed namespace",
        ),
        _ => err(
            "DependencyUnavailable",
            "kernel service cgroup observation is unavailable",
        ),
    }
}

#[derive(PartialEq, Eq)]
struct GroupIdentity {
    device: u64,
    inode: u64,
    mount_id: u64,
}

fn group_identity(dir: &OwnedFd) -> Result<GroupIdentity> {
    let info = fstat(dir).map_err(|error| kernel_error(io::Error::from(error)))?;
    let mount = statx(dir, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .map_err(|error| kernel_error(io::Error::from(error)))?;
    require(
        mount.stx_mask & StatxFlags::MNT_ID.bits() != 0 && mount.stx_mnt_id != 0,
        "UnsupportedCapability",
        "kernel cgroup mount identity is unavailable",
    )?;
    Ok(GroupIdentity {
        device: info.st_dev,
        inode: info.st_ino,
        mount_id: mount.stx_mnt_id,
    })
}

fn manager_epoch(config: &ServiceConfig) -> Result<String> {
    service_manager::sample(&config.manager)?.ok_or_else(|| {
        err(
            "DependencyUnavailable",
            "systemd manager startup time is unavailable",
        )
    })
}

pub(super) fn observe(work: &Work, query: &Query) -> Result<Value> {
    let config = configured_service(work)?;
    let from = pressure::boot_us();
    let boot_id = service_control::boot_id()?;
    require(
        boot_id == query.expected_boot_id,
        "PreconditionFailed",
        "service invocation belongs to another Linux boot",
    )?;
    let node_id = work.service_resources_node_id.as_deref().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "service resource task lacks a node identity",
        )
    })?;
    let epoch = manager_epoch(config)?;
    let invocation = Invocation::get(config, query)?;
    let relative = group_relative(&invocation.group_path)?;
    let root = linux_cgroup::root().map_err(kernel_error)?;
    let mount = group_identity(&root)?;
    let group = linux_cgroup::group_at(&root, relative).map_err(kernel_error)?;
    let identity = group_identity(&group)?;
    let cgroup_namespace = fs::metadata("/proc/self/ns/cgroup")?.ino();
    let mount_namespace = fs::metadata("/proc/self/ns/mnt")?.ino();
    require(
        identity.mount_id == mount.mount_id && identity.inode == invocation.group_id,
        "PreconditionFailed",
        "systemd and kernel disagree on service cgroup identity",
    )?;
    let process = custody::Pinned::pin(node_id, invocation.pid)?;
    process.confirm(&invocation.group_path)?;
    require(
        pressure::contains_process(&group, invocation.pid).map_err(kernel_error)?,
        "PreconditionFailed",
        "service main process is not a direct member of the verified cgroup",
    )?;

    let hierarchy = limits::Hierarchy::sample(&root, &group, relative)?;
    let resources = pressure::cgroup_snapshot(&group).map_err(kernel_error)?;
    require(
        hierarchy.agrees_with_leaf(&resources),
        "PreconditionFailed",
        "service cgroup limits changed during observation",
    )?;
    let after = Invocation::get(config, query)?;
    let fresh_root = linux_cgroup::root().map_err(kernel_error)?;
    let current = linux_cgroup::group_at(&fresh_root, relative).map_err(kernel_error)?;
    require(
        invocation == after
            && group_identity(&fresh_root)? == mount
            && group_identity(&current)? == identity
            && limits::Hierarchy::sample(&fresh_root, &current, relative)? == hierarchy
            && pressure::contains_process(&current, invocation.pid).map_err(kernel_error)?
            && manager_epoch(config)? == epoch
            && service_control::boot_id()? == boot_id
            && fs::metadata("/proc/self/ns/cgroup")?.ino() == cgroup_namespace
            && fs::metadata("/proc/self/ns/mnt")?.ino() == mount_namespace,
        "PreconditionFailed",
        "service invocation or cgroup changed during observation",
    )?;
    process.confirm(&invocation.group_path)?;

    Ok(json!({
        "service_ref":work.target.reference(),"manager":config.manager,
        "manager_uid":service_control::manager_uid(&config.manager),"boot_id":boot_id,
        "manager_started_monotonic_us":epoch,"invocation_id":query.expected_invocation_id,
        "main_process_ref":process.reference(),"cgroup_mount_id":identity.mount_id.to_string(),
        "cgroup_namespace_inode":cgroup_namespace.to_string(),"mount_namespace_inode":mount_namespace.to_string(),
        "from_boot_us":from.to_string(),"through_boot_us":pressure::boot_us().to_string(),"observed_at":now(),
        "cgroup":resources,"visible_limits":hierarchy.summary(),
    }))
}
