//! Bounded, read-only interface observations in the daemon's network namespace.
mod kernel;
use crate::{Result, err, now, require};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, net::IpAddr, os::unix::fs::MetadataExt, process::Command};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub after_ifindex: Option<u32>,
    pub limit: Option<usize>,
}
impl Query {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=128).contains(&self.limit.unwrap_or(32)),
            "InvalidArgument",
            "interface page limit must be 1 to 128",
        )
    }
}

#[derive(Deserialize, Serialize)]
struct Interface {
    ifindex: u32,
    ifname: String,
    mtu: u32,
    operstate: String,
    flags: Vec<String>,
    link_type: String,
    addr_info: Vec<Address>,
}
#[derive(Deserialize, Serialize)]
struct Address {
    family: String,
    local: IpAddr,
    prefixlen: u8,
    scope: String,
    #[serde(
        rename(deserialize = "address"),
        skip_serializing_if = "Option::is_none"
    )]
    peer: Option<IpAddr>,
}

fn iproute_interfaces() -> Result<Vec<Interface>> {
    let mut cmd = Command::new("/usr/sbin/ip");
    cmd.env_clear()
        .env("LC_ALL", "C")
        .args(["-j", "address", "show"]);
    let result = crate::command::capture(cmd, false, "iproute2", 5)?;
    require(
        !result.truncated,
        "QuotaExceeded",
        "interface inventory exceeds the backend output limit",
    )?;
    serde_json::from_slice(&result.stdout).map_err(|_| {
        err(
            "DependencyUnavailable",
            "invalid interface observation from iproute2",
        )
    })
}

pub fn interfaces(q: &Query) -> Result<Value> {
    let mut interfaces = match fs::canonicalize("/usr/sbin/ip") {
        Ok(path) if path.file_name().is_some_and(|s| s == "busybox") => kernel::observe()?,
        Ok(_) => iproute_interfaces()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => kernel::observe()?,
        Err(e) => return Err(e.into()),
    };
    interfaces.sort_unstable_by_key(|i| i.ifindex);
    require(
        !interfaces.windows(2).any(|p| p[0].ifindex == p[1].ifindex),
        "Conflict",
        "interface index changed during observation",
    )?;
    interfaces.retain(|i| i.ifindex > q.after_ifindex.unwrap_or(0));
    let limit = q.limit.unwrap_or(32);
    let more = interfaces.len() > limit;
    interfaces.truncate(limit);
    for i in &interfaces {
        require(
            i.addr_info.iter().all(|a| match a.local {
                IpAddr::V4(_) => a.family == "inet" && a.prefixlen <= 32,
                IpAddr::V6(_) => a.family == "inet6" && a.prefixlen <= 128,
            }),
            "DependencyUnavailable",
            "invalid interface address or prefix length",
        )?;
    }
    let next = more.then(|| interfaces.last().unwrap().ifindex);
    Ok(json!({
        "interfaces": interfaces, "next_after_ifindex": next, "observed_at": now(),
        "boot_id": fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
        "network_namespace_inode": fs::metadata("/proc/self/ns/net")?.ino().to_string(),
    }))
}
