use crate::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) fn valid_scope(boot: &str, ns: &str) -> bool {
    uuid::Uuid::parse_str(boot).is_ok_and(|id| !id.is_nil() && id.to_string() == boot)
        && (ns == "unsupported"
            || ns
                .parse::<u64>()
                .is_ok_and(|n| n != 0 && n.to_string() == ns))
}

/// The original elapsed scope stays authoritative even when the Linux boot has changed.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Anchor {
    boot_id: String,
    time_ns: String,
    deadline_boot_ms: i64,
}

impl Anchor {
    pub(super) fn capture(conn: &Connection, resource: &str) -> Result<Option<Self>> {
        let found = conn.query_row(
            "SELECT boot_id,time_ns,deadline_boot_ms FROM authorization_deadlines WHERE resource=?1",
            [resource],
            |r| Ok(Self { boot_id: r.get(0)?, time_ns: r.get(1)?, deadline_boot_ms: r.get(2)? }),
        ).optional()?;
        Ok(found.filter(|anchor| {
            valid_scope(&anchor.boot_id, &anchor.time_ns) && anchor.deadline_boot_ms > 0
        }))
    }

    pub(super) fn matches(
        conn: &Connection,
        resource: &str,
        binding: &Value,
        shift: i64,
    ) -> Result<bool> {
        let Ok(original) = Self::deserialize(binding) else {
            return Ok(false);
        };
        let actual = Self::capture(conn, resource)?;
        Ok(original.deadline_boot_ms > 0
            && shift >= 0
            && actual.is_some_and(|actual| {
                original.boot_id == actual.boot_id
                    && original.time_ns == actual.time_ns
                    && original.deadline_boot_ms.checked_add(shift) == Some(actual.deadline_boot_ms)
            }))
    }
}
