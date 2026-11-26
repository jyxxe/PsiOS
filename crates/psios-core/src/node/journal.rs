use super::configured_service;
use crate::{Result, config::Manager, err, host::Work, now, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeSet, process::Command};

pub const TOOL: &str = "service.logs";
pub const SINCE_TOOL: &str = "service.logs_since";
pub(crate) const SINCE_FORMAT: u32 = crate::artifact::retention::delayed::FORMAT + 1;
const MAX_ROWS: u32 = 32;
const MAX_MESSAGE_BYTES: usize = 4096;
const MAX_CURSOR_BYTES: usize = 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub id: String,
    pub expected_resource_version: String,
    pub limit: Option<u32>,
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        validate_limit(self.limit)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinceQuery {
    pub id: String,
    pub expected_resource_version: String,
    pub boot_id: String,
    pub after_cursor: String,
    pub limit: Option<u32>,
}

impl SinceQuery {
    pub fn validate(&self) -> Result<()> {
        validate_limit(self.limit)?;
        require(
            uuid::Uuid::parse_str(&self.boot_id)
                .is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == self.boot_id),
            "InvalidArgument",
            "service journal continuation requires a canonical Linux boot ID",
        )?;
        // systemd cursors are opaque; do not infer an entry or seek order from the text.
        require(
            valid_cursor(&self.after_cursor),
            "InvalidArgument",
            "service journal cursor must be bounded printable ASCII",
        )
    }
}

fn valid_cursor(cursor: &str) -> bool {
    (1..=MAX_CURSOR_BYTES).contains(&cursor.len())
        && cursor.bytes().all(|byte| byte.is_ascii_graphic())
}

fn validate_limit(limit: Option<u32>) -> Result<()> {
    require(
        (1..=MAX_ROWS).contains(&limit.unwrap_or(16)),
        "InvalidArgument",
        "service log limit must be 1 to 32 entries",
    )
}

fn message(value: &Value) -> Value {
    if let Some(text) = value
        .as_str()
        .filter(|text| text.len() <= MAX_MESSAGE_BYTES)
    {
        return json!({"encoding":"Utf8", "data":text});
    }
    if let Some(array) = value
        .as_array()
        .filter(|array| array.len() <= MAX_MESSAGE_BYTES)
    {
        let bytes: Option<Vec<_>> = array
            .iter()
            .map(|byte| byte.as_u64().and_then(|n| u8::try_from(n).ok()))
            .collect();
        if let Some(bytes) = bytes {
            return json!({"encoding":"Base64", "data":STANDARD.encode(bytes)});
        }
    }
    json!({"encoding":"Omitted", "data":Value::Null})
}

pub fn read(work: &Work, query: &Query) -> Result<Value> {
    read_window(work, query.limit.unwrap_or(16), None)
}

pub fn read_since(work: &Work, query: &SinceQuery) -> Result<Value> {
    query.validate()?;
    read_window(work, query.limit.unwrap_or(16), Some(query))
}

fn read_window(work: &Work, limit: u32, continuation: Option<&SinceQuery>) -> Result<Value> {
    let config = configured_service(work)?;
    let boot_id = crate::linux_clock::boot_id()?;
    if let Some(query) = continuation {
        require(
            query.boot_id == boot_id,
            "CursorExpired",
            "service journal cursor belongs to a different Linux boot",
        )?;
    }
    let uid = rustix::process::geteuid().as_raw().to_string();
    let user = config.manager == Manager::User;
    let field = if user {
        "_SYSTEMD_USER_UNIT"
    } else {
        "_SYSTEMD_UNIT"
    };
    let boot_match = format!("--boot={}", boot_id.replace('-', ""));
    let unit_match = format!("{field}={}", config.unit);
    let output_fields = format!("--output-fields=MESSAGE,PRIORITY,_PID,{field},_SYSTEMD_OWNER_UID");
    let mut command = Command::new("/usr/bin/journalctl");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C");
    command.args([
        if user { "--user" } else { "--system" },
        "--no-pager",
        "--quiet",
        "--output=json",
    ]);
    command.arg(&boot_match).arg(output_fields);
    let max_rows = if let Some(query) = continuation {
        let max_rows = limit + 2;
        command
            .arg(format!("--cursor={}", query.after_cursor))
            .arg(format!("--lines=+{max_rows}"));
        max_rows
    } else {
        command.arg(format!("--lines={limit}"));
        limit
    };
    command.arg("--").arg(unit_match);
    if user {
        command.arg(format!("_SYSTEMD_OWNER_UID={uid}"));
    }
    let output = crate::command::capture(command, false, "systemd journal", 5)?;
    require(
        !output.truncated,
        "QuotaExceeded",
        "service journal result exceeds output limit",
    )?;

    let boot_hex = boot.simple().to_string();
    let mut entries = Vec::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        require(
            entries.len() < max_rows as usize,
            "DependencyUnavailable",
            "journal exceeded the row limit",
        )?;
        let row = serde_json::from_slice::<Value>(line)
            .map_err(|_| err("DependencyUnavailable", "invalid journal entry"))?;
        require(
            row[field] == config.unit
                && row["_BOOT_ID"] == boot_hex
                && (!user || row["_SYSTEMD_OWNER_UID"] == uid),
            "DependencyUnavailable",
            "journal entry is outside the configured service and boot",
        )?;
        let timestamp = row["__REALTIME_TIMESTAMP"]
            .as_str()
            .and_then(|text| text.parse::<u64>().ok())
            .ok_or_else(|| err("DependencyUnavailable", "invalid journal timestamp"))?;
        let cursor = row["__CURSOR"]
            .as_str()
            .filter(|text| (1..=MAX_CURSOR_BYTES).contains(&text.len()))
            .filter(|text| continuation.is_none() || valid_cursor(text))
            .ok_or_else(|| err("DependencyUnavailable", "invalid journal entry identity"))?;
        let priority = row["PRIORITY"]
            .as_str()
            .and_then(|n| n.parse::<u8>().ok())
            .filter(|n| *n <= 7);
        let pid = row["_PID"].as_str().and_then(|n| n.parse::<u32>().ok());
        entries.push(
            json!({"journal_cursor":cursor,"realtime_usec":timestamp.to_string(),
            "priority":priority,"pid":pid,"message":message(&row["MESSAGE"])}),
        );
    }
    let mut result = json!({
        "service_ref":work.target.reference(), "unit":config.unit, "manager":config.manager,
        "boot_id":boot_id, "limit":limit, "observed_at":now(),
    });
    if let Some(query) = continuation {
        // journalctl may seek to a nearby entry if the exact one was rotated out or is out of scope.
        // Accept only a cursor value returned by this scoped journal for the first row.
        require(
            entries
                .first()
                .is_some_and(|entry| entry["journal_cursor"] == query.after_cursor),
            "CursorExpired",
            "service journal cursor is no longer available in the configured scope",
        )?;
        let unique = {
            let mut seen = BTreeSet::new();
            entries.iter().all(|entry| {
                entry["journal_cursor"]
                    .as_str()
                    .is_some_and(|cursor| seen.insert(cursor))
            })
        };
        require(
            unique,
            "DependencyUnavailable",
            "service journal returned duplicate entry identities",
        )?;
        entries.remove(0);
        let has_more = entries.len() > limit as usize;
        entries.truncate(limit as usize);
        let next = entries
            .last()
            .and_then(|entry| entry["journal_cursor"].as_str())
            .unwrap_or(&query.after_cursor);
        result["after_cursor"] = json!(query.after_cursor);
        result["next_cursor"] = json!(next);
        result["has_more"] = json!(has_more);
    }
    result["entries"] = json!(entries);
    Ok(result)
}
