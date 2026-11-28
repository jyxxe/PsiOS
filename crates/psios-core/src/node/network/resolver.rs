//! Observe the resolver file visible in the Core's mount namespace.
use crate::{Result, err, now, require};
use rustix::fs::{Mode, OFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::{fs, io::Read, net::IpAddr, os::fd::AsRawFd, os::unix::fs::MetadataExt};

pub(crate) const TOOL: &str = "node.resolver";
pub(crate) const FORMAT: u32 = crate::local_read_tools::VOLUME_DIRECTORY_REVISION_FORMAT + 1;
const PATH: &str = "/etc/resolv.conf";
const MAX_BYTES: u64 = 64 * 1024;
const MAX_SERVERS: usize = 32;

#[derive(Debug, PartialEq, Eq, Serialize)]
struct Server<'a> {
    address: IpAddr,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone: Option<&'a str>,
}

fn server(value: &str) -> Option<Server<'_>> {
    let (literal, zone) = value
        .split_once('%')
        .map_or((value, None), |(address, zone)| (address, Some(zone)));
    let address = literal.parse::<IpAddr>().ok()?;
    if let Some(zone) = zone
        && (!address.is_ipv6()
            || !(1..=15).contains(&zone.len())
            || !zone
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte)))
    {
        return None;
    }
    Some(Server { address, zone })
}

fn nameservers(contents: &str) -> Result<(Vec<Server<'_>>, usize)> {
    let mut servers = Vec::new();
    let mut invalid = 0;
    for line in contents.lines() {
        let directive = line.split(['#', ';']).next().unwrap_or_default();
        let mut fields = directive.split_ascii_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        if let Some(server) = fields.next().and_then(server) {
            require(
                servers.len() < MAX_SERVERS,
                "QuotaExceeded",
                "resolver file contains more than 32 nameservers",
            )?;
            servers.push(server);
        } else {
            invalid += 1;
        }
    }
    Ok((servers, invalid))
}

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

pub(crate) fn observe() -> Result<Value> {
    // Follow the system's resolver symlink without opening a FIFO or device
    // for I/O. Reopen only the pinned, validated regular file.
    let unavailable = |_| {
        err(
            "DependencyUnavailable",
            "the system resolver file is unavailable",
        )
    };
    let entry = fs::File::from(
        rustix::fs::open(PATH, OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
            .map_err(unavailable)?,
    );
    let before = entry.metadata()?;
    require(
        before.is_file() && before.mode() & 0o022 == 0,
        "PermissionDenied",
        "the system resolver file must be regular and protected from group and other writes",
    )?;
    require(
        before.len() <= MAX_BYTES,
        "QuotaExceeded",
        "resolver file exceeds 64 KiB",
    )?;
    let mut file = fs::File::from(
        rustix::fs::open(
            format!("/proc/self/fd/{}", entry.as_raw_fd()),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(unavailable)?,
    );
    let mut bytes = Vec::new();
    (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= MAX_BYTES,
        "QuotaExceeded",
        "resolver file exceeds 64 KiB",
    )?;
    require(
        before.len() == bytes.len() as u64
            && same_file(&before, &file.metadata()?)
            && fs::metadata(PATH).is_ok_and(|current| same_file(&before, &current)),
        "Conflict",
        "resolver file changed while reading its configuration",
    )?;
    let contents = String::from_utf8_lossy(&bytes);
    let (servers, invalid) = nameservers(&contents)?;
    let namespace = super::namespace()?;
    Ok(json!({
        "path":PATH,
        "boot_id":namespace.boot_id,
        "network_namespace_inode":namespace.network_namespace_inode,
        "mount_namespace_inode":fs::metadata("/proc/self/ns/mnt")?.ino().to_string(),
        "observed_at":now(),
        "nameservers":servers,
        "invalid_nameserver_directives":invalid,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_only_nameservers_in_file_order() {
        let input = "# comment\nsearch internal.invalid\n nameserver 192.0.2.1 # IPv4\n\
                     nameserver fe80::53%eth0\n nameserver 192.0.2.1; repeated\n\
                     nameserver 192.0.2.1%lo\n nameserver example.invalid\n nameserver\n";
        let (servers, invalid) = nameservers(input).unwrap();
        assert_eq!(
            json!(servers),
            json!([
                {"address":"192.0.2.1"},{"address":"fe80::53","zone":"eth0"},{"address":"192.0.2.1"},
            ])
        );
        assert_eq!(invalid, 3);
        assert!(nameservers("\n# none\n").unwrap().0.is_empty());
        assert_eq!(
            nameservers(&"nameserver ::1\n".repeat(MAX_SERVERS + 1))
                .unwrap_err()
                .0
                .code,
            "QuotaExceeded"
        );
    }
}
