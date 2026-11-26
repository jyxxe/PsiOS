//! Summarize configured ceilings across the visible cgroup mount hierarchy.
use super::kernel_error;
use crate::{
    Result, linux_cgroup,
    node::pressure::{CONTROL_LIMIT, CpuLimit, Limit, Reading, cpu_limit, limit, read},
};
use rustix::{fd::OwnedFd, fs::fstat};
use serde_json::{Value, json};
use std::{io, path::Path};

#[derive(PartialEq, Eq)]
struct Group {
    directory: (u64, u64),
    cpu: Reading<CpuLimit>,
    memory: Reading<Limit>,
    swap: Reading<Limit>,
    pids: Reading<Limit>,
}

fn configured(group: &OwnedFd) -> Result<Group> {
    let info = fstat(group).map_err(|error| kernel_error(io::Error::from(error)))?;
    let read_limit = |name| read(linux_cgroup::control(group, name), CONTROL_LIMIT, limit);
    Ok(Group {
        directory: (info.st_dev, info.st_ino),
        cpu: read(
            linux_cgroup::control(group, "cpu.max"),
            CONTROL_LIMIT,
            cpu_limit,
        ),
        memory: read_limit("memory.max"),
        swap: read_limit("memory.swap.max"),
        pids: read_limit("pids.max"),
    })
}

#[derive(PartialEq, Eq)]
pub(super) struct Hierarchy(Vec<Group>);

impl Hierarchy {
    pub(super) fn sample(root: &OwnedFd, leaf: &OwnedFd, path: &Path) -> Result<Self> {
        let mut levels = vec![configured(leaf)?];
        for ancestor in path.ancestors().skip(1) {
            let group = if ancestor.as_os_str().is_empty() {
                configured(root)?
            } else {
                let group = linux_cgroup::group_at(root, ancestor).map_err(kernel_error)?;
                configured(&group)?
            };
            levels.push(group);
        }
        Ok(Self(levels))
    }

    pub(super) fn summary(&self) -> Value {
        json!({
            "coverage":"VisibleCgroupAncestors", "groups_sampled":self.0.len(),
            "cpu_bandwidth":minimum(self.0.iter().map(|g| &g.cpu), smaller_bandwidth),
            "memory_max_bytes":minimum(self.0.iter().map(|g| &g.memory), smaller_limit),
            "swap_max_bytes":minimum(self.0.iter().map(|g| &g.swap), smaller_limit),
            "pids_max":minimum(self.0.iter().map(|g| &g.pids), smaller_limit),
        })
    }

    pub(super) fn agrees_with_leaf(&self, resources: &Value) -> bool {
        let leaf = &self.0[0];
        resources["cpu"]["configured_limit"] == json!(leaf.cpu)
            && resources["memory"]["configured_max_bytes"] == json!(leaf.memory)
            && resources["memory"]["configured_swap_max_bytes"] == json!(leaf.swap)
            && resources["pids"]["configured_max"] == json!(leaf.pids)
    }
}

fn minimum<'a, T: Copy + 'a>(
    readings: impl Iterator<Item = &'a Reading<T>>,
    tighter: impl Fn(T, T) -> T,
) -> Reading<T> {
    let mut result = None;
    for reading in readings {
        match reading {
            Reading::Observed(value) => {
                result = Some(result.map_or(*value, |old| tighter(old, *value)))
            }
            // A controller need not expose a setting at every hierarchy level.
            Reading::Unsupported => {}
            Reading::Denied => return Reading::Denied,
            Reading::Unavailable => return Reading::Unavailable,
            Reading::ChangedDuringObservation => return Reading::ChangedDuringObservation,
        }
    }
    result.map_or(Reading::Unsupported, Reading::Observed)
}

fn smaller_limit(first: Limit, second: Limit) -> Limit {
    match (first, second) {
        (Limit::Bounded(a), Limit::Bounded(b)) => Limit::Bounded(a.min(b)),
        (Limit::Unbounded, _) => second,
        (_, Limit::Unbounded) => first,
    }
}

fn smaller_bandwidth(first: CpuLimit, second: CpuLimit) -> CpuLimit {
    match (first.quota_us, second.quota_us) {
        (Limit::Unbounded, _) => second,
        (Limit::Bounded(a), Limit::Bounded(b))
            if u128::from(b) * u128::from(first.period_us)
                < u128::from(a) * u128::from(second.period_us) =>
        {
            second
        }
        _ => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_bandwidth_compares_exact_rates_and_preserves_unknown_limits() {
        let readings = [
            Reading::Observed(cpu_limit("10000 20000").unwrap()),
            Reading::Unsupported,
            Reading::Observed(cpu_limit("15000 100000").unwrap()),
        ];
        assert_eq!(
            serde_json::to_value(minimum(readings.iter(), smaller_bandwidth)).unwrap(),
            json!({"status":"Observed","value":{"quota_us":{"kind":"Bounded","value":15000},"period_us":100000}})
        );
        let maximum = [
            Reading::Observed(Limit::Unbounded),
            Reading::Observed(Limit::Bounded(0)),
        ];
        assert!(matches!(
            minimum(maximum.iter(), smaller_limit),
            Reading::Observed(Limit::Bounded(0))
        ));
        let partial = [Reading::Observed(Limit::Bounded(20)), Reading::Denied];
        assert!(matches!(
            minimum(partial.iter(), smaller_limit),
            Reading::Denied
        ));
    }
}
