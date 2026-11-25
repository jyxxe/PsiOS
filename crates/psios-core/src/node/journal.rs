use super::configured_service;
use crate::{Result, config::Manager, err, host::Work, now, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{fs, process::Command};

pub const TOOL: &str = "service.logs";
const MAX_ROWS: u32 = 32;
const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub id: String,
    pub expected_resource_version: String,
    pub limit: Option<u32>,
}

impl Query {
    fn limit(&self) -> u32 {
        self.limit.unwrap_or(16)
    }

    pub fn validate(&self) -> Result<()> {
        require(
            (1..=MAX_ROWS).contains(&self.limit()),
            "InvalidArgument",
            "service log limit must be 1 to 32 entries",
        )
    }
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
    let config = configured_service(work)?;
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let boot = uuid::Uuid::parse_str(boot.trim())
        .map_err(|_| err("DependencyUnavailable", "invalid Linux boot identity"))?;
    let uid = rustix::process::geteuid().as_raw().to_string();
    let user = config.manager == Manager::User;
    let field = if user {
        "_SYSTEMD_USER_UNIT"
    } else {
        "_SYSTEMD_UNIT"
    };
    let boot_match = format!("--boot={}", boot.simple());
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
    command
        .arg(&boot_match)
        .arg(output_fields)
        .arg(format!("--lines={}", query.limit()));
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
            entries.len() < query.limit() as usize,
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
            .filter(|text| (1..=1024).contains(&text.len()))
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
    Ok(
        json!({"service_ref":work.target.reference(), "unit":config.unit, "manager":config.manager,
        "boot_id":boot.to_string(), "limit":query.limit(),"entries":entries,"observed_at":now()}),
    )
}
