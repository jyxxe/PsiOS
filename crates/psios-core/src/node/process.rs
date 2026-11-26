//! Bounded procfs observations tied to a process in the local node's PID view.
pub(super) mod custody;

use crate::{Result, err, linux_proc, now, require};
use rustix::{
    fd::OwnedFd,
    fs::{Dir, fstat},
    io::Errno,
    param::{clock_ticks_per_second, page_size},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Read};

pub(crate) const FORMAT: u32 = crate::events::query::SNAPSHOT_FORMAT + 1;
pub const TOOL: &str = "node.process_inspect";
const STAT_LIMIT: u64 = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    after_pid: Option<u32>,
    limit: Option<usize>,
}

impl Page {
    pub fn validate(&self) -> Result<()> {
        require(
            (1..=128).contains(&self.limit.unwrap_or(32)),
            "InvalidArgument",
            "process page limit must be 1 to 128",
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    node_id: String,
    boot_id: String,
    proc_view_id: String,
    pid: u32,
    start_ticks: String,
}

impl Identity {
    pub fn validate(&self) -> Result<()> {
        crate::label(&self.node_id)?;
        let decimal = |s: &str| s.parse::<u64>().is_ok_and(|n| n.to_string() == s);
        require(
            self.pid > 0
                && uuid::Uuid::parse_str(&self.boot_id)
                    .is_ok_and(|id| id.hyphenated().to_string() == self.boot_id)
                && decimal(&self.proc_view_id)
                && decimal(&self.start_ticks),
            "InvalidArgument",
            "invalid Linux process reference",
        )
    }
}

struct Proc {
    root: OwnedFd,
    node_id: String,
    boot_id: String,
    view_id: String,
}

impl Proc {
    fn open(node_id: &str) -> Result<Self> {
        let root = linux_proc::root().map_err(|error| {
            if error.kind() == io::ErrorKind::Unsupported {
                err("UnsupportedCapability", "local procfs is unavailable")
            } else {
                error.into()
            }
        })?;
        Ok(Self {
            node_id: node_id.into(),
            boot_id: crate::linux_clock::boot_id_in(&root)?,
            view_id: fstat(&root).map_err(io::Error::from)?.st_dev.to_string(),
            root,
        })
    }

    fn stat(&self, pid: u32) -> Result<String> {
        read_at(&self.root, &format!("{pid}/stat"), STAT_LIMIT)
    }

    fn identity(&self, pid: u32, start_ticks: u64) -> Identity {
        Identity {
            node_id: self.node_id.clone(),
            boot_id: self.boot_id.clone(),
            proc_view_id: self.view_id.clone(),
            pid,
            start_ticks: start_ticks.to_string(),
        }
    }
}

fn read_at(root: &OwnedFd, path: &str, limit: u64) -> Result<String> {
    let mut bytes = Vec::new();
    linux_proc::file(root, path)
        .map_err(stat_error)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(stat_error)?;
    require(
        bytes.len() as u64 <= limit,
        "QuotaExceeded",
        "procfs observation exceeds the profile",
    )?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn stat_error(error: io::Error) -> crate::Error {
    match error.raw_os_error() {
        Some(code) if code == Errno::NOENT.raw_os_error() || code == Errno::SRCH.raw_os_error() => {
            err("NotFound", "process is no longer visible in this PID view")
        }
        _ if error.kind() == io::ErrorKind::PermissionDenied => {
            err("PermissionDenied", "kernel denied process observation")
        }
        _ => error.into(),
    }
}

struct Sample<'a> {
    name: &'a str,
    state: &'a str,
    parent_pid: u32,
    user_ticks: u64,
    system_ticks: u64,
    threads: u64,
    start_ticks: u64,
    virtual_bytes: u64,
    resident_pages: i64,
}

impl<'a> Sample<'a> {
    fn parse(raw: &'a str, pid: u32) -> Option<Self> {
        // comm can contain spaces, newlines and parentheses. All following
        // fields are separated by spaces; the closing delimiter is the last.
        let (head, tail) = raw.rsplit_once(") ")?;
        let (observed_pid, name) = head.split_once(" (")?;
        if observed_pid.parse::<u32>().ok()? != pid {
            return None;
        }
        let fields: Vec<_> = tail.split_ascii_whitespace().take(22).collect();
        if fields.len() != 22 || fields[0].len() != 1 {
            return None;
        }
        Some(Self {
            name,
            state: fields[0],
            parent_pid: fields[1].parse().ok()?,
            user_ticks: fields[11].parse().ok()?,
            system_ticks: fields[12].parse().ok()?,
            threads: fields[17].parse().ok()?,
            start_ticks: fields[19].parse().ok()?,
            virtual_bytes: fields[20].parse().ok()?,
            resident_pages: fields[21].parse().ok()?,
        })
    }
}

pub(super) fn page(node_id: &str, query: &Page) -> Result<Value> {
    query.validate()?;
    let proc = Proc::open(node_id)?;
    let limit = query.limit.unwrap_or(32);
    let mut pids = Vec::new();
    for entry in Dir::read_from(&proc.root).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        if let Ok(name) = entry.file_name().to_str()
            && let Ok(pid) = name.parse::<u32>()
            && pid > query.after_pid.unwrap_or(0)
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    let mut items = Vec::with_capacity(limit);
    let mut next_after = None;
    let mut last = 0;
    for pid in pids {
        if items.len() == limit {
            next_after = Some(last);
            break;
        }
        let raw = match proc.stat(pid) {
            Ok(raw) => raw,
            Err(e) if matches!(e.0.code.as_str(), "NotFound" | "PermissionDenied") => continue,
            Err(e) => return Err(e),
        };
        if let Some(sample) = Sample::parse(&raw, pid) {
            items.push(json!({
                "pid":pid,"name":sample.name,"state":sample.state,
                "parent_pid":sample.parent_pid.to_string(),"start_ticks":sample.start_ticks.to_string(),
                "resident_pages":sample.resident_pages.to_string(),
                "process_ref":proc.identity(pid, sample.start_ticks),
            }));
            last = pid;
        }
    }
    Ok(json!({
        "boot_id":proc.boot_id,"proc_view_id":proc.view_id,"items":items,
        "next_after_pid":next_after,"observed_at":now(),
    }))
}

pub(super) fn inspect(node_id: &str, expected: &Identity) -> Result<Value> {
    require(
        node_id == expected.node_id,
        "PreconditionFailed",
        "process reference belongs to a different node",
    )?;
    let proc = Proc::open(node_id)?;
    require(
        proc.boot_id == expected.boot_id && proc.view_id == expected.proc_view_id,
        "PreconditionFailed",
        "process reference belongs to a different boot or procfs view",
    )?;
    let raw = proc.stat(expected.pid)?;
    let sample = Sample::parse(&raw, expected.pid)
        .ok_or_else(|| err("StorageUnavailable", "invalid process stat record"))?;
    let identity = proc.identity(expected.pid, sample.start_ticks);
    require(
        &identity == expected,
        "PreconditionFailed",
        "PID refers to a different process instance",
    )?;
    Ok(json!({
        "process_ref":identity,"name":sample.name,"state":sample.state,
        "parent_pid":sample.parent_pid,"threads":sample.threads,
        "cpu":{"user_ticks":sample.user_ticks.to_string(),"system_ticks":sample.system_ticks.to_string(),
               "ticks_per_second":clock_ticks_per_second()},
        "memory":{"virtual_bytes":sample.virtual_bytes.to_string(),
                  "resident_pages":sample.resident_pages.to_string(),"page_bytes":page_size()},
        "observed_at":now(),
    }))
}
