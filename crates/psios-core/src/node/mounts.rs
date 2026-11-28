//! Observe the Core's own Linux mount namespace without exposing mount credentials.
use crate::{Result, err, now, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};

const INPUT_LIMIT: u64 = 512 * 1024;
const MOUNT_LIMIT: usize = 4096;
const MOUNT_NAMESPACE: &str = "/proc/self/ns/mnt";

#[derive(Serialize)]
#[serde(untagged)]
enum LinuxPath {
    Text(String),
    Bytes { base64: String },
}

#[derive(Serialize)]
struct Device {
    major: u32,
    minor: u32,
}

#[derive(Default, Serialize)]
struct Propagation {
    shared_group: Option<u32>,
    master_group: Option<u32>,
    propagate_from_group: Option<u32>,
    unbindable: bool,
}

#[derive(Serialize)]
struct Mount {
    mount_id: u32,
    parent_mount_id: u32,
    device: Device,
    filesystem_root: LinuxPath,
    mount_point: LinuxPath,
    filesystem_type: String,
    read_only: bool,
    superblock_read_only: bool,
    flags: Vec<&'static str>,
    propagation: Propagation,
}

fn malformed() -> crate::Error {
    err("HostFailure", "invalid Linux mountinfo record")
}

fn number(value: &[u8]) -> Result<u32> {
    std::str::from_utf8(value)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(malformed)
}

fn path(mut value: &[u8]) -> Result<LinuxPath> {
    let mut decoded = Vec::with_capacity(value.len());
    while let Some((&head, tail)) = value.split_first() {
        value = tail;
        if head != b'\\' {
            decoded.push(head);
            continue;
        }
        let (escape, rest) = tail.split_at_checked(3).ok_or_else(malformed)?;
        decoded.push(match escape {
            b"040" => b' ',
            b"011" => b'\t',
            b"012" => b'\n',
            b"134" => b'\\',
            _ => return Err(malformed()),
        });
        value = rest;
    }
    Ok(match String::from_utf8(decoded) {
        Ok(text) => LinuxPath::Text(text),
        Err(error) => LinuxPath::Bytes {
            base64: STANDARD.encode(error.into_bytes()),
        },
    })
}

fn read_only(options: &[u8]) -> Result<bool> {
    match options.split(|b| *b == b',').next() {
        Some(b"ro") => Ok(true),
        Some(b"rw") => Ok(false),
        _ => Err(malformed()),
    }
}

fn mount(line: &[u8]) -> Result<Mount> {
    let mut parts = line.split(|b| *b == b' ');
    let mut field = || {
        parts
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(malformed)
    };
    let mount_id = number(field()?)?;
    let parent_mount_id = number(field()?)?;
    let device = field()?;
    let split = device
        .iter()
        .position(|b| *b == b':')
        .ok_or_else(malformed)?;
    let device = Device {
        major: number(&device[..split])?,
        minor: number(&device[split + 1..])?,
    };
    let filesystem_root = path(field()?)?;
    let point = field()?;
    require(
        point.starts_with(b"/"),
        "HostFailure",
        "Linux mount point is not absolute",
    )?;
    let mount_point = path(point)?;
    let options = field()?;
    let mut propagation = Propagation::default();
    loop {
        let tag = field()?;
        if tag == b"-" {
            break;
        }
        if let Some(value) = tag.strip_prefix(b"shared:") {
            propagation.shared_group = Some(number(value)?);
        } else if let Some(value) = tag.strip_prefix(b"master:") {
            propagation.master_group = Some(number(value)?);
        } else if let Some(value) = tag.strip_prefix(b"propagate_from:") {
            propagation.propagate_from_group = Some(number(value)?);
        } else if tag == b"unbindable" {
            propagation.unbindable = true;
        }
    }
    let filesystem_type = std::str::from_utf8(field()?)
        .map_err(|_| malformed())?
        .to_owned();
    let _source = field()?;
    let super_options = field()?;
    require(
        parts.next().is_none(),
        "HostFailure",
        "unexpected Linux mountinfo columns",
    )?;

    // Only kernel VFS flags are relevant here. Source and filesystem-specific
    // options can contain credentials and should never enter the artifact.
    let flags = [
        "nosuid",
        "nodev",
        "noexec",
        "nosymfollow",
        "noatime",
        "nodiratime",
        "relatime",
        "strictatime",
    ]
    .into_iter()
    .filter(|flag| {
        options
            .split(|b| *b == b',')
            .any(|value| value == flag.as_bytes())
    })
    .collect();
    Ok(Mount {
        mount_id,
        parent_mount_id,
        device,
        filesystem_root,
        mount_point,
        filesystem_type,
        read_only: read_only(options)?,
        superblock_read_only: read_only(super_options)?,
        flags,
        propagation,
    })
}

fn table(file: fs::File) -> Result<Vec<Mount>> {
    let mut input = Vec::new();
    file.take(INPUT_LIMIT + 1).read_to_end(&mut input)?;
    require(
        input.len() as u64 <= INPUT_LIMIT,
        "QuotaExceeded",
        "mount table exceeds the observation byte limit",
    )?;
    let input = input.strip_suffix(b"\n").ok_or_else(malformed)?;
    let mut mounts = Vec::new();
    for line in input.split(|b| *b == b'\n') {
        require(
            mounts.len() < MOUNT_LIMIT,
            "QuotaExceeded",
            "mount table exceeds the observation row limit",
        )?;
        mounts.push(mount(line)?);
    }
    mounts.sort_unstable_by_key(|mount| mount.mount_id);
    require(
        mounts
            .windows(2)
            .all(|pair| pair[0].mount_id < pair[1].mount_id),
        "Conflict",
        "mount identity changed during observation",
    )?;
    Ok(mounts)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootIdMount {
    NativeProc,
    LocalLxc,
}

pub(crate) fn boot_id_mount(
    file: fs::File,
    mount_id: u64,
    device: (u32, u32),
    proc_device: (u32, u32),
) -> Result<BootIdMount> {
    // The kernel provides the unique mount ID of the held boot ID descriptor.
    // Decode only that row and bound the search even in a large namespace.
    let mut stream = BufReader::new(file.take(INPUT_LIMIT + 1));
    let mut bytes = 0u64;
    let mut line = Vec::new();
    for _ in 0..MOUNT_LIMIT {
        line.clear();
        let length = stream.read_until(b'\n', &mut line)?;
        if length == 0 {
            break;
        }
        bytes += length as u64;
        require(
            bytes <= INPUT_LIMIT,
            "QuotaExceeded",
            "mount table exceeds the clock profile",
        )?;
        let raw = line.strip_suffix(b"\n").ok_or_else(malformed)?;
        let first = raw
            .split(|byte| *byte == b' ')
            .next()
            .ok_or_else(malformed)?;
        if u64::from(number(first)?) == mount_id {
            return classify_boot_mount(mount(raw)?, device, proc_device);
        }
    }
    Err(err(
        "UnsupportedCapability",
        "Linux boot ID mount is outside the clock profile",
    ))
}

fn classify_boot_mount(
    observed: Mount,
    device: (u32, u32),
    proc_device: (u32, u32),
) -> Result<BootIdMount> {
    const BOOT_ID: &str = "/proc/sys/kernel/random/boot_id";
    let outside = || {
        err(
            "UnsupportedCapability",
            "Linux boot ID mount is outside the clock profile",
        )
    };
    let (LinuxPath::Text(point), LinuxPath::Text(root)) =
        (observed.mount_point, observed.filesystem_root)
    else {
        return Err(outside());
    };
    require(
        observed.read_only && device == (observed.device.major, observed.device.minor),
        "UnsupportedCapability",
        "Linux boot ID mount lacks read-only custody",
    )?;
    if observed.filesystem_type == "proc"
        && device == proc_device
        && Path::new(BOOT_ID).starts_with(&point)
        && point.strip_prefix("/proc") == Some(root.as_str())
    {
        return Ok(BootIdMount::NativeProc);
    }
    if matches!(observed.filesystem_type.as_str(), "devtmpfs" | "tmpfs")
        && point == BOOT_ID
        && root == "/.lxc-boot-id"
        && ["nosuid", "nodev", "noexec"]
            .iter()
            .all(|flag| observed.flags.contains(flag))
    {
        return Ok(BootIdMount::LocalLxc);
    }
    Err(outside())
}

pub(crate) fn private_data_mount(
    file: fs::File,
    observed_mount_id: u64,
    read_only: bool,
) -> Result<()> {
    let mut matched = false;
    for entry in table(file)? {
        let point = match entry.mount_point {
            LinuxPath::Text(name) => name.into_bytes(),
            LinuxPath::Bytes { base64 } => STANDARD.decode(base64).map_err(|_| malformed())?,
        };
        if point == b"/data" {
            require(
                !matched
                    && u64::from(entry.mount_id) == observed_mount_id
                    && entry.read_only == read_only
                    && (read_only || !entry.superblock_read_only)
                    && entry.propagation.shared_group.is_none()
                    && entry.propagation.master_group.is_none()
                    && entry.propagation.propagate_from_group.is_none()
                    && !entry.propagation.unbindable,
                "PreconditionFailed",
                "effective OCI /data mount access or propagation differs from its approval",
            )?;
            matched = true;
        }
        require(
            !point.starts_with(b"/data/"),
            "PreconditionFailed",
            "effective OCI data mount includes an unapproved nested mount",
        )?;
    }
    require(
        matched,
        "PreconditionFailed",
        "effective OCI /data mount is missing",
    )
}

pub(super) fn observe() -> Result<Value> {
    let namespace = fs::metadata(MOUNT_NAMESPACE)?.ino();
    let mounts = table(fs::File::open("/proc/self/mountinfo")?)?;
    require(
        fs::metadata(MOUNT_NAMESPACE)?.ino() == namespace,
        "Conflict",
        "mount namespace changed during observation",
    )?;
    Ok(json!({
        "mount_namespace_inode": namespace.to_string(),
        "boot_id": crate::linux_clock::boot_id()?,
        "observed_at": now(),
        "mounts": mounts,
    }))
}

pub(crate) fn native_storage(root: &rustix::fd::OwnedFd) -> Result<Value> {
    use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

    let thread = crate::linux_proc::thread(root)?;
    let mounts = table(crate::linux_proc::file(&thread, "mountinfo")?)?;
    let mut storage = serde_json::Map::new();
    for (key, path) in [
        ("state", "/var/lib"),
        ("home", "/mnt/psios-homes"),
        ("operator_home", "/home/psios"),
    ] {
        let mut matches = mounts
            .iter()
            .filter(|mount| matches!(&mount.mount_point, LinuxPath::Text(point) if point == path));
        let mount = matches.next();
        require(
            matches.next().is_none(),
            "Conflict",
            "native storage mount is covered by another mount",
        )?;
        if let Some(mount) = mount {
            let effective = statx(
                CWD,
                path,
                AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT,
                StatxFlags::MNT_ID,
            )
            .map_err(|_| err("Conflict", "native storage mount is no longer visible"))?;
            require(
                effective.stx_mask & StatxFlags::MNT_ID.bits() != 0,
                "UnsupportedCapability",
                "kernel mount identity is unavailable",
            )?;
            require(
                effective.stx_mnt_id == u64::from(mount.mount_id),
                "Conflict",
                "native storage mount is no longer visible",
            )?;
        }
        storage.insert(
            key.into(),
            mount.map_or(Value::Null, |mount| {
                json!({
                    "mount_id": mount.mount_id,
                    "device": mount.device,
                    "filesystem": mount.filesystem_type,
                    "read_only": mount.read_only || mount.superblock_read_only,
                    "flags": mount.flags,
                })
            }),
        );
    }
    Ok(Value::Object(storage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn checks_effective_private_data_mount_access() {
        let check = |line: &str, id, read_only| {
            let mut table = tempfile::tempfile().unwrap();
            table.write_all(line.as_bytes()).unwrap();
            table.rewind().unwrap();
            private_data_mount(table, id, read_only)
        };
        let data = "77 51 8:5 /owned /data rw,relatime - ext4 local rw\n";
        check(data, 77, false).unwrap();
        let read_only = data.replace("rw,relatime", "ro,relatime");
        check(&read_only, 77, true).unwrap();
        assert_eq!(
            check(data, 77, true).unwrap_err().0.code,
            "PreconditionFailed"
        );
        let nested = "78 77 8:5 /other /data/extra rw - ext4 local rw\n";
        for (bad, id) in [
            (data.to_owned(), 78),
            (read_only, 77),
            (data.replace("local rw", "local ro"), 77),
            (data.replace(" - ", " shared:7 - "), 77),
            (format!("{data}{nested}"), 77),
        ] {
            assert_eq!(
                check(&bad, id, false).unwrap_err().0.code,
                "PreconditionFailed"
            );
        }
    }

    #[test]
    fn parses_kernel_escapes_and_keeps_mount_and_superblock_flags_separate() {
        let raw = br"51 33 8:5 /a\040b /mnt\134a\011b\012c ro,nodev,nosuid,relatime shared:5 master:1 future:2 - ext4 source-placeholder rw,private=not-returned";
        let report = serde_json::to_value(mount(raw).unwrap()).unwrap();
        assert_eq!(report["filesystem_root"], "/a b");
        assert_eq!(report["mount_point"], "/mnt\\a\tb\nc");
        assert_eq!(report["flags"], json!(["nosuid", "nodev", "relatime"]));
        assert_eq!(report["read_only"], true);
        assert_eq!(report["superblock_read_only"], false);
        assert_eq!(report["propagation"]["shared_group"], 5);
        assert_eq!(report["propagation"]["master_group"], 1);
        assert!(!report.to_string().contains("source-placeholder"));
        assert!(!report.to_string().contains("not-returned"));
    }

    #[test]
    fn preserves_non_utf8_paths_without_repairing_malformed_records() {
        let raw = b"44 5 0:8 / /bad\xff ro unbindable - tmpfs none rw";
        let report = serde_json::to_value(mount(raw).unwrap()).unwrap();
        assert_eq!(
            report["mount_point"]["base64"],
            STANDARD.encode(b"/bad\xff")
        );
        assert_eq!(report["propagation"]["unbindable"], true);
        assert!(mount(br"44 5 0:8 / /bad\077 ro - tmpfs none rw").is_err());
        assert!(mount(b"44 5 0:8 / /bad ro - tmpfs none").is_err());
    }

    #[test]
    fn namespace_filesystem_roots_are_kernel_names() {
        let raw = b"129 54 0:4 net:[4026531992] /run/netns/managed rw - nsfs nsfs rw";
        let report = serde_json::to_value(mount(raw).unwrap()).unwrap();
        assert_eq!(report["filesystem_root"], "net:[4026531992]");
        assert_eq!(report["mount_point"], "/run/netns/managed");
    }

    #[test]
    fn boot_id_mount_requires_the_original_proc_path_or_exact_lxc_file() {
        let proc_dev = (0, 50);
        let lxc_dev = (0, 5);
        let read_only_proc = b"66 1 0:50 /sys /proc/sys ro,nosuid,nodev,noexec - proc proc rw";
        let lxc = b"66 1 0:5 /.lxc-boot-id /proc/sys/kernel/random/boot_id ro,nosuid,nodev,noexec - devtmpfs udev rw";
        assert!(matches!(
            classify_boot_mount(mount(read_only_proc).unwrap(), proc_dev, proc_dev),
            Ok(BootIdMount::NativeProc)
        ));
        assert!(matches!(
            classify_boot_mount(mount(lxc).unwrap(), lxc_dev, proc_dev),
            Ok(BootIdMount::LocalLxc)
        ));
        for (line, device) in [
            ("66 1 0:4 /sys /proc/sys ro - proc proc rw", (0, 4)),
            (
                "66 1 0:50 /sys/kernel/random /proc/sys ro - proc proc rw",
                proc_dev,
            ),
            (
                "66 1 0:5 /other /proc/sys/kernel/random/boot_id ro,nosuid,nodev,noexec - tmpfs tmpfs rw",
                lxc_dev,
            ),
            (
                "66 1 0:5 /.lxc-boot-id /proc/sys/kernel/random/boot_id ro,nosuid,nodev - tmpfs tmpfs rw",
                lxc_dev,
            ),
        ] {
            let error = classify_boot_mount(mount(line.as_bytes()).unwrap(), device, proc_dev)
                .err()
                .unwrap();
            assert_eq!(error.0.code, "UnsupportedCapability");
        }
    }
}
