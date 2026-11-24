//! Observe the Core's own Linux mount namespace without exposing mount credentials.
use crate::{Result, err, now, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::{fs, io::Read, os::unix::fs::MetadataExt};

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

pub(super) fn observe() -> Result<Value> {
    let namespace = fs::metadata(MOUNT_NAMESPACE)?.ino();
    let mut input = Vec::new();
    fs::File::open("/proc/self/mountinfo")?
        .take(INPUT_LIMIT + 1)
        .read_to_end(&mut input)?;
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
            .all(|pair| pair[0].mount_id < pair[1].mount_id)
            && fs::metadata(MOUNT_NAMESPACE)?.ino() == namespace,
        "Conflict",
        "mount identity changed during observation",
    )?;
    Ok(json!({
        "mount_namespace_inode": namespace.to_string(),
        "boot_id": super::scalar("/proc/sys/kernel/random/boot_id")?,
        "observed_at": now(),
        "mounts": mounts,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
