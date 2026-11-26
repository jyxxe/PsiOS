//! Fixed local system D-Bus transport, pinned to a single bus and NM owner.
use crate::{Result, err, require};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    process::Command,
};

const SOCKET: &str = "/run/dbus/system_bus_socket";
const ADDRESS: &str = "unix:path=/run/dbus/system_bus_socket";
const DBUS: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
pub(super) const NM: &str = "org.freedesktop.NetworkManager";
pub(super) const NM_PATH: &str = "/org/freedesktop/NetworkManager";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub boot_id: String,
    pub network_namespace_device: u64,
    pub network_namespace_inode: u64,
    pub time_namespace_device: u64,
    pub time_namespace_inode: u64,
    pub bus_id: String,
    pub manager_owner: String,
    pub manager_pid: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    #[serde(rename = "type")]
    signature: String,
    data: Value,
}

fn run(address: &str, args: &[&str], effect: bool) -> Result<Vec<u8>> {
    let mut command = Command::new("/usr/bin/busctl");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/usr/bin:/bin")
        .arg(format!("--address={address}"))
        .args([
            "--json=short",
            "--no-pager",
            "--auto-start=no",
            "--allow-interactive-authorization=no",
            "--timeout=2s",
        ])
        .args(args);
    let reply = crate::command::capture(command, effect, "NetworkManager D-Bus", 3)?;
    require(
        !reply.truncated,
        if effect {
            "OutcomeUnknown"
        } else {
            "DependencyUnavailable"
        },
        "NetworkManager D-Bus reply exceeds limit",
    )?;
    Ok(reply.stdout)
}

fn payload(bytes: &[u8], signature: &str, arity: usize, effect: bool) -> Result<Vec<Value>> {
    let invalid = || {
        err(
            if effect {
                "OutcomeUnknown"
            } else {
                "DependencyUnavailable"
            },
            "invalid typed NetworkManager D-Bus reply",
        )
    };
    if signature.is_empty() && arity == 0 && bytes.is_empty() {
        return Ok(Vec::new());
    }
    let reply: Reply = psios_api::parse(bytes)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .ok_or_else(invalid)?;
    match reply.data {
        Value::Array(data) if data.len() == arity && reply.signature == signature => Ok(data),
        _ => Err(invalid()),
    }
}

fn dbus(address: &str, method: &str, args: &[&str], signature: &str) -> Result<Value> {
    let mut command = vec!["call", DBUS, DBUS_PATH, DBUS, method];
    command.extend_from_slice(args);
    let bytes = run(address, &command, false)?;
    Ok(payload(&bytes, signature, 1, false)?
        .pop()
        .expect("one D-Bus value"))
}

pub(super) fn field<'a>(map: &'a Value, name: &str, signature: &str) -> Result<&'a Value> {
    let value = &map[name];
    require(
        value["type"] == signature
            && value.as_object().is_some_and(|obj| obj.len() == 2)
            && value.get("data").is_some(),
        "DependencyUnavailable",
        "NetworkManager property has an unexpected type",
    )?;
    Ok(&value["data"])
}

pub(super) fn text(value: &Value) -> Result<&str> {
    value.as_str().ok_or_else(|| {
        err(
            "DependencyUnavailable",
            "NetworkManager value is not a string",
        )
    })
}

pub(super) fn uint(value: &Value) -> Result<u32> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| {
            err(
                "DependencyUnavailable",
                "NetworkManager value is not a uint32",
            )
        })
}

pub(super) fn numbered_path<'a>(value: &'a Value, prefix: &str) -> Result<&'a str> {
    let value = text(value)?;
    require(
        value
            .strip_prefix(prefix)
            .is_some_and(|suffix| canonical_uint(suffix).is_some_and(|number| number > 0)),
        "DependencyUnavailable",
        "NetworkManager object path is outside its expected kind",
    )?;
    Ok(value)
}

fn canonical_uint(value: &str) -> Option<u32> {
    value
        .parse::<u32>()
        .ok()
        .filter(|number| number.to_string() == value)
}

fn local_socket() -> Result<()> {
    for directory in ["/run", "/run/dbus"] {
        let metadata = fs::symlink_metadata(directory).map_err(|_| {
            err(
                "DependencyUnavailable",
                "system D-Bus runtime directory is unavailable",
            )
        })?;
        require(
            metadata.is_dir() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0,
            "DependencyUnavailable",
            "system D-Bus runtime directory is not root-controlled",
        )?;
    }
    let socket = fs::symlink_metadata(SOCKET).map_err(|_| {
        err(
            "DependencyUnavailable",
            "system D-Bus socket is unavailable",
        )
    })?;
    require(
        socket.file_type().is_socket() && socket.uid() == 0,
        "DependencyUnavailable",
        "system D-Bus socket is not root-controlled",
    )
}

fn same_namespace(pid: u32, kind: &str) -> Result<(u64, u64)> {
    let unavailable = || {
        err(
            "DependencyUnavailable",
            "unable to check NetworkManager Linux namespaces",
        )
    };
    let local = fs::metadata(format!("/proc/self/ns/{kind}")).map_err(|_| unavailable())?;
    let remote = fs::metadata(format!("/proc/{pid}/ns/{kind}")).map_err(|_| unavailable())?;
    require(
        local.ino() == remote.ino() && local.dev() == remote.dev(),
        "PreconditionFailed",
        "NetworkManager belongs to another Linux namespace",
    )?;
    Ok((local.dev(), local.ino()))
}

pub(super) struct Bus {
    pub(super) identity: Identity,
    address: String,
}

impl Bus {
    pub(super) fn open() -> Result<Self> {
        require(
            rustix::process::geteuid().is_root(),
            "DependencyUnavailable",
            "the local NetworkManager adapter requires root service custody",
        )?;
        local_socket()?;
        let boot_id = crate::node::service_control::boot_id()?;
        let bus_id = dbus(ADDRESS, "GetId", &[], "s")?;
        let bus_id = text(&bus_id)?;
        require(
            bus_id.len() == 32
                && bus_id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                && bus_id.as_bytes().iter().any(|byte| *byte != b'0'),
            "DependencyUnavailable",
            "invalid system D-Bus ID",
        )?;
        let address = format!("{ADDRESS},guid={bus_id}");
        let owner = dbus(&address, "GetNameOwner", &["s", NM], "s")?;
        let owner = text(&owner)?;
        require(
            owner
                .strip_prefix(':')
                .and_then(|name| name.split_once('.'))
                .is_some_and(|(major, minor)| {
                    canonical_uint(major).is_some_and(|number| number > 0)
                        && canonical_uint(minor).is_some()
                }),
            "DependencyUnavailable",
            "invalid NetworkManager D-Bus owner",
        )?;
        let uid = uint(&dbus(
            &address,
            "GetConnectionUnixUser",
            &["s", owner],
            "u",
        )?)?;
        let pid = uint(&dbus(
            &address,
            "GetConnectionUnixProcessID",
            &["s", owner],
            "u",
        )?)?;
        require(
            uid == 0 && pid > 0,
            "DependencyUnavailable",
            "NetworkManager D-Bus owner is not a root process",
        )?;
        let (network_namespace_device, network_namespace_inode) = same_namespace(pid, "net")?;
        let (time_namespace_device, time_namespace_inode) = same_namespace(pid, "time")?;
        require(
            dbus(&address, "GetNameOwner", &["s", NM], "s")? == owner,
            "PreconditionFailed",
            "NetworkManager D-Bus owner changed",
        )?;
        Ok(Self {
            identity: Identity {
                boot_id,
                network_namespace_device,
                network_namespace_inode,
                time_namespace_device,
                time_namespace_inode,
                bus_id: bus_id.to_owned(),
                manager_owner: owner.to_owned(),
                manager_pid: pid,
            },
            address,
        })
    }

    pub(super) fn verify_owner(&self) -> Result<()> {
        require(
            dbus(&self.address, "GetNameOwner", &["s", NM], "s")? == self.identity.manager_owner,
            "PreconditionFailed",
            "NetworkManager D-Bus owner changed",
        )
    }

    pub(super) fn get_all(&self, path: &str, interface: &str) -> Result<Value> {
        self.read(
            path,
            "org.freedesktop.DBus.Properties",
            "GetAll",
            &["s", interface],
            "a{sv}",
        )
    }

    pub(super) fn read(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        args: &[&str],
        signature: &str,
    ) -> Result<Value> {
        Ok(self
            .call(path, interface, method, args, false, (signature, 1))?
            .pop()
            .expect("one D-Bus value"))
    }

    pub(super) fn applied(&self, device: &str) -> Result<Value> {
        let mut values = self.call(
            device,
            "org.freedesktop.NetworkManager.Device",
            "GetAppliedConnection",
            &["u", "0"],
            false,
            ("a{sa{sv}}t", 2),
        )?;
        require(
            values
                .pop()
                .is_some_and(|version| version.as_u64().is_some_and(|version| version > 0)),
            "DependencyUnavailable",
            "invalid NetworkManager applied connection version",
        )?;
        Ok(values.pop().expect("applied settings"))
    }

    pub(super) fn mutate(
        &self,
        method: &str,
        args: &[&str],
        signature: &str,
    ) -> Result<Option<Value>> {
        Ok(self
            .call(
                NM_PATH,
                NM,
                method,
                args,
                true,
                (signature, usize::from(!signature.is_empty())),
            )?
            .pop())
    }

    fn call(
        &self,
        path: &str,
        interface: &str,
        method: &str,
        args: &[&str],
        effect: bool,
        (signature, arity): (&str, usize),
    ) -> Result<Vec<Value>> {
        let mut command = vec![
            "call",
            self.identity.manager_owner.as_str(),
            path,
            interface,
            method,
        ];
        command.extend_from_slice(args);
        payload(
            &run(&self.address, &command, effect)?,
            signature,
            arity,
            effect,
        )
    }
}
