//! Sample the systemd manager's startup time on the same transport as unit control.
use crate::{Result, config::Manager, db::Db, err, policy, require};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::process::Command;

pub(crate) const FORMAT: u32 = crate::storage::restore_publication::DISPOSAL_FORMAT + 1;
const KIND: &str = "HostServiceManagerEpoch";
const EPOCH: &str = "host-service-manager-epoch";
pub(super) const STARTED: &str = "manager_started_monotonic_us";

pub(super) fn command(manager: &Manager, verb: &str) -> Command {
    let mut command = Command::new("/usr/bin/systemctl");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C");
    let scope = match manager {
        Manager::User => {
            command.env(
                "XDG_RUNTIME_DIR",
                format!("/run/user/{}", rustix::process::geteuid().as_raw()),
            );
            "--user"
        }
        Manager::System => "--system",
    };
    command.args([scope, "--no-pager", "--no-ask-password", verb]);
    command
}

pub(super) fn sample(manager: &Manager) -> Result<Option<String>> {
    let mut command = command(manager, "show");
    command.arg("--property=UserspaceTimestampMonotonic");
    let result = crate::command::capture(command, false, "systemd manager", 5)?;
    require(
        !result.truncated,
        "DependencyUnavailable",
        "systemd manager startup response exceeds limit",
    )?;
    parse_sample(&result.stdout)
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.create(
        KIND,
        EPOCH,
        policy::SYSTEM,
        json!({}),
        json!({"format": FORMAT}),
    )?;
    Ok(())
}

pub(super) fn cutover(conn: &Connection) -> Result<Option<i64>> {
    let format: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if format < FORMAT {
        return Ok(None);
    }
    let seq = conn
        .query_row(
            "SELECT seq FROM current WHERE id=?1 AND kind=?2 AND rev=1 AND owner=?3
           AND spec='{}' AND body=?4 AND seq>0",
            params![
                EPOCH,
                KIND,
                policy::SYSTEM,
                json!({"format": FORMAT}).to_string()
            ],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "systemd manager evidence epoch is missing",
            )
        })?;
    Ok(Some(seq))
}

fn canonical_timestamp(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|usec| usec > 0 && usec < u64::MAX && usec.to_string() == value)
}

pub(super) fn timestamp(observation: &Value) -> Option<&str> {
    observation[STARTED]
        .as_str()
        .filter(|value| canonical_timestamp(value))
}

fn parse_sample(output: &[u8]) -> Result<Option<String>> {
    if output.is_empty() || output == b"UserspaceTimestampMonotonic=0\n" {
        return Ok(None);
    }
    let started = std::str::from_utf8(output)
        .ok()
        .and_then(|line| line.strip_prefix("UserspaceTimestampMonotonic="))
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| canonical_timestamp(value))
        .ok_or_else(|| {
            err(
                "DependencyUnavailable",
                "invalid systemd manager startup time",
            )
        })?;
    Ok(Some(started.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_startup_is_an_exact_nonzero_decimal_sample() {
        assert_eq!(parse_sample(b"").unwrap(), None);
        assert_eq!(
            parse_sample(b"UserspaceTimestampMonotonic=0\n").unwrap(),
            None
        );
        assert_eq!(
            parse_sample(b"UserspaceTimestampMonotonic=23830883\n")
                .unwrap()
                .as_deref(),
            Some("23830883")
        );
        for invalid in [
            "UserspaceTimestampMonotonic=\n",
            "UserspaceTimestampMonotonic=023830883\n",
            "UserspaceTimestampMonotonic=1\nUserspaceTimestampMonotonic=2\n",
            "UserspaceTimestampMonotonic=18446744073709551615\n",
            "UserspaceTimestampMonotonic=18446744073709551616\n",
            "UserspaceTimestampMonotonic=+3\n",
            "UserspaceTimestamp=23830883\n",
        ] {
            assert_eq!(
                parse_sample(invalid.as_bytes()).unwrap_err().0.code,
                "DependencyUnavailable"
            );
        }
    }
}
