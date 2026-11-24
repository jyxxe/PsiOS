mod agent;
mod api;
mod apps;
mod broker;
mod chat;
mod command;
mod config;
mod db;
mod events;
mod host;
mod node;
mod policy;
mod schedule;
pub mod server;
mod storage;
mod watch;
mod workflow;

pub use config::{AuthorityReserve, Config};
pub use db::Store;
use psios_api::ApiError;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use std::fmt;
pub use storage::inspect_detached_backup;

#[derive(Debug)]
pub struct Error(pub Box<ApiError>);
pub type Result<T> = std::result::Result<T, Error>;
pub fn err(code: &str, msg: impl Into<String>) -> Error {
    Error(Box::new(ApiError::new(code, msg)))
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Error {}
impl From<Box<ApiError>> for Error {
    fn from(e: Box<ApiError>) -> Self {
        Self(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        eprintln!("psiosd database: {e}");
        err("StorageUnavailable", "state database error")
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        err("StorageUnavailable", format!("local I/O: {}", e.kind()))
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        err("InvalidArgument", "invalid fields or types")
    }
}
pub(crate) fn decode<T: DeserializeOwned>(v: &Value) -> Result<T> {
    Ok(serde_json::from_value(v.clone())?)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ById {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Conditional {
    id: String,
    expected_resource_version: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn after(seconds: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(seconds))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn expired(at: &str) -> Result<bool> {
    let at = chrono::DateTime::parse_from_rfc3339(at)
        .map_err(|_| err("InvalidArgument", "invalid UTC deadline"))?;
    Ok(at <= chrono::Utc::now())
}
fn require(cond: bool, code: &str, msg: &str) -> Result<()> {
    if cond { Ok(()) } else { Err(err(code, msg)) }
}
fn label(s: &str) -> Result<()> {
    require(
        !s.is_empty() && s.len() <= 200 && !s.chars().any(char::is_control),
        "InvalidArgument",
        "invalid identifier",
    )
}

#[cfg(test)]
mod recovery_tests;
