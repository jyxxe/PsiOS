//! Bounded observations of Linux pressure and configured controls.
mod proc_source;

use crate::{Result, err, host::Work, linux_cgroup, now, require};
use proc_source::Source;
use rustix::{
    fd::OwnedFd,
    fs::fstat,
    time::{ClockId, clock_gettime},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

pub const TOOL: &str = "node.pressure";
pub(super) const CONTROL_LIMIT: usize = 8192;
const VMSTAT_LIMIT: usize = 64 * 1024;
const MEMBERSHIP_LIMIT: usize = 128 * 1024;

#[derive(Serialize, PartialEq, Eq)]
#[serde(tag = "status", content = "value")]
pub(super) enum Reading<T> {
    Observed(T),
    Unsupported,
    Denied,
    Unavailable,
    ChangedDuringObservation,
}

impl<T> From<io::Result<T>> for Reading<T> {
    fn from(value: io::Result<T>) -> Self {
        match value {
            Ok(value) => Self::Observed(value),
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::Unsupported => Self::Unsupported,
                io::ErrorKind::PermissionDenied => Self::Denied,
                io::ErrorKind::Interrupted => Self::ChangedDuringObservation,
                _ => Self::Unavailable,
            },
        }
    }
}

pub(super) fn text(file: File, limit: usize) -> io::Result<String> {
    let mut value = String::new();
    file.take(limit as u64 + 1).read_to_string(&mut value)?;
    if value.len() > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(value)
}

pub(super) fn read<T>(
    file: io::Result<File>,
    limit: usize,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Reading<T> {
    file.and_then(|file| text(file, limit))
        .and_then(|value| parse(&value).ok_or_else(|| io::ErrorKind::InvalidData.into()))
        .into()
}

#[derive(Serialize)]
struct Stalls {
    avg10_percent: f64,
    avg60_percent: f64,
    avg300_percent: f64,
    total_us: u64,
}

fn ratio(value: &str) -> Option<f64> {
    let value: f64 = value.parse().ok()?;
    (0.0..=100.0).contains(&value).then_some(value)
}

fn stalls<'a>(fields: impl Iterator<Item = &'a str>) -> Option<Stalls> {
    let (mut avg10, mut avg60, mut avg300, mut total) = (None, None, None, None);
    for field in fields {
        let (key, value) = field.split_once('=')?;
        match key {
            "avg10" if avg10.replace(ratio(value)?).is_none() => (),
            "avg60" if avg60.replace(ratio(value)?).is_none() => (),
            "avg300" if avg300.replace(ratio(value)?).is_none() => (),
            "total" if total.replace(value.parse().ok()?).is_none() => (),
            "avg10" | "avg60" | "avg300" | "total" => return None,
            _ => (),
        }
    }
    Some(Stalls {
        avg10_percent: avg10?,
        avg60_percent: avg60?,
        avg300_percent: avg300?,
        total_us: total?,
    })
}

#[derive(Serialize)]
struct Pressure {
    some: Stalls,
    #[serde(skip_serializing_if = "Option::is_none")]
    full: Option<Stalls>,
}

fn pressure(value: &str, report_full: bool) -> Option<Pressure> {
    let mut some = None;
    let mut full = None;
    for line in value.lines() {
        let mut fields = line.split_ascii_whitespace();
        let row = match fields.next()? {
            "some" => &mut some,
            "full" => &mut full,
            _ => continue,
        };
        if row.replace(stalls(fields)?).is_some() {
            return None;
        }
    }
    // Linux always reports system-wide CPU full as zero, including when CPUs are contended.
    Some(Pressure {
        some: some?,
        full: if report_full { full } else { None },
    })
}

fn kernel_pressure(source: &Source, path: &str, report_full: bool) -> Reading<Pressure> {
    read(source.file(path), CONTROL_LIMIT, |text| {
        pressure(text, report_full)
    })
}

type Counters = BTreeMap<&'static str, u64>;

fn counters(value: &str, keys: &[&'static str], required: &str) -> Option<Counters> {
    let mut result = BTreeMap::new();
    for line in value.lines() {
        let mut fields = line.split_ascii_whitespace();
        let key = fields.next()?;
        if let Some(&key) = keys.iter().find(|&&item| item == key) {
            let number = fields.next()?.parse().ok()?;
            if fields.next().is_some() || result.insert(key, number).is_some() {
                return None;
            }
        }
    }
    result.contains_key(required).then_some(result)
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub(super) enum Limit {
    Bounded(u64),
    Unbounded,
}

pub(super) fn limit(text: &str) -> Option<Limit> {
    match text.trim() {
        "max" => Some(Limit::Unbounded),
        value => value.parse().ok().map(Limit::Bounded),
    }
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
pub(super) struct CpuLimit {
    pub(super) quota_us: Limit,
    pub(super) period_us: u64,
}

pub(super) fn cpu_limit(text: &str) -> Option<CpuLimit> {
    let mut parts = text.split_ascii_whitespace();
    let quota_us = limit(parts.next()?)?;
    let period_us = parts.next()?.parse().ok()?;
    (parts.next().is_none() && period_us > 0 && quota_us != Limit::Bounded(0)).then_some(CpuLimit {
        quota_us,
        period_us,
    })
}

fn membership(source: &Source) -> io::Result<PathBuf> {
    let contents = text(source.membership()?, CONTROL_LIMIT)?;
    let mut unified = contents
        .lines()
        .filter_map(|line| line.strip_prefix("0::/"));
    let path = Path::new(unified.next().ok_or(io::ErrorKind::Unsupported)?);
    if unified.next().is_some()
        || path
            .components()
            .any(|segment| !matches!(segment, Component::Normal(_)))
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(path.to_owned())
}

pub(super) fn contains_process(group: &OwnedFd, pid: u32) -> io::Result<bool> {
    // A namespace path alone can match the wrong directory on a bind mount.
    let ids = text(
        linux_cgroup::control(group, "cgroup.procs")?,
        MEMBERSHIP_LIMIT,
    )?;
    let pid = pid.to_string();
    Ok(ids.lines().any(|line| line == pid))
}

fn group_snapshot(source: &Source) -> io::Result<Value> {
    let pid = std::process::id();
    let path = membership(source)?;
    let group = linux_cgroup::group(linux_cgroup::root()?, &path)?;
    if !contains_process(&group, pid)? {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let result = cgroup_snapshot(&group)?;
    if membership(source)? != path || !contains_process(&group, pid)? {
        return Err(io::ErrorKind::Interrupted.into());
    }
    Ok(result)
}

pub(super) fn cgroup_snapshot(group: &OwnedFd) -> io::Result<Value> {
    let id = fstat(group).map_err(io::Error::from)?;
    let control = |name| linux_cgroup::control(group, name);
    let group_pressure = |name| read(control(name), CONTROL_LIMIT, |text| pressure(text, true));
    let preferred_limit = |name| read(control(name), CONTROL_LIMIT, limit);
    let usage = |name| {
        read(control(name), CONTROL_LIMIT, |text| {
            text.trim().parse::<u64>().ok()
        })
    };
    let cpu_stat = read(control("cpu.stat"), CONTROL_LIMIT, |text| {
        counters(
            text,
            &[
                "usage_usec",
                "user_usec",
                "system_usec",
                "nr_periods",
                "nr_throttled",
                "throttled_usec",
            ],
            "usage_usec",
        )
    });
    let memory_events = read(control("memory.events.local"), CONTROL_LIMIT, |text| {
        counters(
            text,
            &["low", "high", "max", "oom", "oom_kill", "oom_group_kill"],
            "oom_kill",
        )
    });
    let pids_events = read(control("pids.events"), CONTROL_LIMIT, |text| {
        counters(text, &["max"], "max")
    });
    Ok(json!({
        "source":"LinuxCgroupV2", "device":id.st_dev.to_string(), "inode":id.st_ino.to_string(),
        "cpu": {
            "pressure":group_pressure("cpu.pressure"), "stat":cpu_stat,
            "configured_limit":read(control("cpu.max"), CONTROL_LIMIT, cpu_limit),
            "configured_burst_us":usage("cpu.max.burst")
        },
        "memory": {
            "pressure":group_pressure("memory.pressure"), "current_bytes":usage("memory.current"),
            "configured_high_bytes":preferred_limit("memory.high"), "configured_max_bytes":preferred_limit("memory.max"),
            "swap_current_bytes":usage("memory.swap.current"), "configured_swap_max_bytes":preferred_limit("memory.swap.max"),
            "local_events":memory_events
        },
        "io": {"pressure":group_pressure("io.pressure")},
        "pids": {"current":usage("pids.current"), "configured_max":preferred_limit("pids.max"), "events":pids_events}
    }))
}

pub(super) fn boot_us() -> u128 {
    let time = clock_gettime(ClockId::Boottime);
    time.tv_sec as u128 * 1_000_000 + time.tv_nsec as u128 / 1_000
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let start = boot_us();
    let source = Source::open().map_err(identity_error)?;
    let identity = source.identity().map_err(identity_error)?;
    // Use the effective boot identity of the exact Node revision. A container
    // can provide its boot ID on a separate mount from the kernel counters.
    let boot_id = work.target.body["boot_id"]
        .as_str()
        .filter(|id| super::block::valid_boot(id))
        .ok_or_else(|| err("RecoveryRequired", "Node lacks a valid boot identity"))?;
    let kernel = json!({"source":"LinuxProcfs",
        "cpu":kernel_pressure(&source, "pressure/cpu", false),
        "memory":kernel_pressure(&source, "pressure/memory", true),
        "io":kernel_pressure(&source, "pressure/io", true), "oom_kills":read(source.file("vmstat"), VMSTAT_LIMIT,
            |text| counters(text, &["oom_kill"], "oom_kill")?.get("oom_kill").copied())
    });
    let group = Reading::from(group_snapshot(&source));
    require(
        source.identity().map_err(identity_error)? == identity,
        "PreconditionFailed",
        "kernel pressure namespace changed during observation",
    )?;
    Ok(
        json!({"node_ref":work.target.reference(), "boot_id":boot_id,
            "cgroup_namespace_inode":identity.cgroup_namespace, "mount_namespace_inode":identity.mount_namespace,
            "observed_at":now(), "from_boot_us":start.to_string(), "through_boot_us":boot_us().to_string(),
            "kernel":kernel, "core_cgroup":group
        }),
    )
}

fn identity_error(error: io::Error) -> crate::Error {
    let code = match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::Unsupported => "UnsupportedCapability",
        io::ErrorKind::PermissionDenied => "PermissionDenied",
        _ => "DependencyUnavailable",
    };
    err(code, "kernel pressure namespace identity is unavailable")
}

pub(crate) fn check_receipt(work: &Work, receipt: &Value) -> Result<()> {
    require(
        receipt["node_ref"] == work.target.reference()
            && receipt["boot_id"] == work.target.body["boot_id"],
        "Fenced",
        "kernel pressure sample differs from its authorized Node revision",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_psi_by_key_omitting_undefined_system_cpu_full() {
        let sample = "full avg10=0.00 avg60=0.04 avg300=0.12 total=63\nsome total=809734 avg300=1.68 avg60=0.77 avg10=2.99";
        let io = pressure(sample, true).unwrap();
        assert_eq!(io.some.total_us, 809734);
        assert_eq!(io.some.avg10_percent, 2.99);
        assert_eq!(io.full.unwrap().total_us, 63);
        assert!(pressure(sample, false).unwrap().full.is_none());
        for line in [
            "some avg10=NaN avg60=0 avg300=0 total=0",
            "some avg10=101 avg60=0 avg300=0 total=0",
            "some avg10=0 avg60=0 avg300=0 total=0 avg10=3",
            "some avg10=0 avg60=0 avg300=0",
            "full avg10=0 avg60=0 avg300=0 total=0",
        ] {
            assert!(pressure(line, true).is_none(), "{line}");
        }
    }

    #[test]
    fn distinguishes_bounds_and_refuses_invalid_counters() {
        assert!(matches!(
            cpu_limit("max 100000\n").unwrap().quota_us,
            Limit::Unbounded
        ));
        assert!(matches!(
            cpu_limit("25000 100000").unwrap().quota_us,
            Limit::Bounded(25000)
        ));
        assert!(cpu_limit("25000 0").is_none());
        assert!(cpu_limit("25000 100000 unexpected").is_none());
        assert_eq!(
            counters("future arbitrary\noom_kill 3", &["oom_kill"], "oom_kill").unwrap()["oom_kill"],
            3
        );
        assert!(counters("oom_kill 3\noom_kill 8", &["oom_kill"], "oom_kill").is_none());
        assert!(counters("oom_kill -1", &["oom_kill"], "oom_kill").is_none());
    }
}
