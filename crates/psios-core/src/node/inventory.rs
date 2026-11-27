//! Sample bounded kernel inventory from one verified procfs view.
use crate::{Result, linux_clock, linux_proc, now};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io};

const MEMORY_LIMIT: usize = 16 * 1024;
const LOAD_LIMIT: usize = 256;

fn memory(input: &str) -> Option<BTreeMap<String, u64>> {
    let mut values = BTreeMap::new();
    for line in input.lines() {
        let Some((name, field)) = line.split_once(':') else {
            continue;
        };
        if !matches!(name, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") {
            continue;
        }
        let mut fields = field.split_ascii_whitespace();
        let bytes = fields.next()?.parse::<u64>().ok()?.checked_mul(1024)?;
        if fields.next() != Some("kB")
            || fields.next().is_some()
            || values.insert(name.to_owned(), bytes).is_some()
        {
            return None;
        }
    }
    values
        .get("MemTotal")
        .is_some_and(|total| *total > 0)
        .then_some(values)
}

fn load_average(input: &str) -> Option<Vec<String>> {
    let values: Vec<_> = input.split_ascii_whitespace().take(3).collect();
    if values.len() != 3
        || !values.iter().all(|value| {
            value.split_once('.').is_some_and(|(integer, fraction)| {
                !integer.is_empty()
                    && fraction.len() == 2
                    && integer
                        .bytes()
                        .chain(fraction.bytes())
                        .all(|b| b.is_ascii_digit())
            })
        })
    {
        return None;
    }
    Some(values.into_iter().map(str::to_owned).collect())
}

fn projection<T: Default>(
    text: io::Result<String>,
    parse: impl FnOnce(&str) -> Option<T>,
) -> (T, &'static str) {
    match text.and_then(|text| parse(&text).ok_or_else(|| io::ErrorKind::InvalidData.into())) {
        Ok(value) => (value, "Observed"),
        Err(error) => (
            T::default(),
            match error.kind() {
                io::ErrorKind::NotFound | io::ErrorKind::Unsupported => "Unsupported",
                io::ErrorKind::PermissionDenied => "Denied",
                _ => "Unavailable",
            },
        ),
    }
}

pub(super) fn observe() -> Result<Value> {
    let root = linux_proc::root()?;
    let (memory_bytes, memory_status) =
        projection(linux_proc::text(&root, "meminfo", MEMORY_LIMIT), memory);
    let (load, load_status) =
        projection(linux_proc::text(&root, "loadavg", LOAD_LIMIT), load_average);
    Ok(json!({
        "boot_id": linux_clock::boot_id_in(&root)?,
        "kernel_release": linux_proc::kernel_release(&root)?,
        "architecture": std::env::consts::ARCH,
        "available_cpus": std::thread::available_parallelism()?.get(),
        "memory_bytes": memory_bytes,
        "load_average": load,
        "kernel_observation": {"memory": memory_status, "load_average": load_status},
        "observed_at": now(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{load_average, memory};

    #[test]
    fn parse_kernel_memory_values_in_kib_without_overflow_or_duplicate_keys() {
        let normal = "MemTotal:      4096 kB\nMemAvailable:  2048 kB\nOther: 99\nSwapFree: 0 kB\n";
        let values = memory(normal).expect("valid kernel input");
        assert_eq!(values["MemTotal"], 4096 * 1024);
        assert_eq!(values["MemAvailable"], 2048 * 1024);
        assert_eq!(values["SwapFree"], 0);
        assert!(!values.contains_key("Other"));
        for malformed in [
            "MemFree: 0 kB",
            "MemTotal: 0 kB",
            "MemTotal: 2 MB",
            "MemTotal: 2 kB extra",
            "MemTotal: 18446744073709551615 kB",
            "MemTotal: 4 kB\nMemTotal: 5 kB",
        ] {
            assert!(memory(malformed).is_none(), "{malformed}");
        }
    }

    #[test]
    fn require_all_three_fixed_decimal_load_averages() {
        assert_eq!(
            load_average("1.25 23.00 4.99 1/123 456\n").unwrap(),
            ["1.25", "23.00", "4.99"],
        );
        for malformed in [
            "1.20 2.34",
            "NaN 2.34 0.00",
            "0.1 0.22 3.00",
            "0.00 0.22 2.",
        ] {
            assert!(load_average(malformed).is_none(), "{malformed}");
        }
    }
}
