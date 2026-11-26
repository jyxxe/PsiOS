//! Owner retirement and the live enrollment projection used by the watcher.
use super::{Identity, KIND, identity, state, view, watched};
use crate::{
    Result,
    db::{Db, Doc},
    decode,
    host::Target,
    now, policy, require,
};
use rusqlite::Connection;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::project::review_signal::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("retirement.sql");

fn retired_state(prior: &Doc, expected: &Identity, retired_at: &str) -> Value {
    let receipt = json!({"lifecycle": "Retired", "observed_at": prior.body["observed_at"]});
    let mut body = state(expected, &receipt, prior.rev + 1);
    body["retired_at"] = json!(retired_at);
    body["retired_from_ref"] = prior.reference();
    body
}

pub(crate) fn retire(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    policy::active(db, actor)?;
    let request: Target = decode(params)?;
    crate::label(&request.id)?;
    let current = db.of(&request.id, KIND)?;
    current.cas(&request.expected_resource_version)?;
    let node = db.of(&db.meta("authority")?, "Node")?;
    let expected = identity(&current, &node.id)?;
    require(
        watched(&current.body),
        "PreconditionFailed",
        "only an active block enrollment can be retired",
    )?;
    require(
        current.body["observed_at"]
            .as_str()
            .is_some_and(valid_timestamp),
        "RecoveryRequired",
        "tracked block device lacks an observation time",
    )?;
    let retired = db.status(&current, retired_state(&current, &expected, &now()))?;
    Ok(view(&retired, &expected, &node))
}

fn valid_timestamp(value: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(value).is_ok()
}

pub(super) fn verify_transition(prior: &Doc, current: &Doc, expected: &Identity) -> Result<()> {
    require(
        current.body["retired_at"].as_str().is_some_and(|at| {
            valid_timestamp(at) && current.body == retired_state(prior, expected, at)
        }),
        "RecoveryRequired",
        "block retirement does not match its prior active revision",
    )
}

pub(super) fn verify_projection(conn: &Connection) -> Result<()> {
    let consistent: bool = conn.query_row(
        "WITH eligible AS (
             SELECT origin.seq AS origin_seq,origin.id AS device_id
             FROM versions AS origin INDEXED BY versions_origin_kind
             WHERE origin.kind='BlockDevice' AND origin.rev=1
               AND (SELECT latest.body->>'lifecycle' FROM versions AS latest
                    WHERE latest.id=origin.id ORDER BY latest.rev DESC LIMIT 1) IN ('Present','Absent')
         )
         SELECT NOT EXISTS (SELECT * FROM eligible EXCEPT SELECT * FROM block_device_watch)
            AND NOT EXISTS (SELECT * FROM block_device_watch EXCEPT SELECT * FROM eligible)",
        [],
        |row| row.get(0),
    )?;
    require(
        consistent,
        "RecoveryRequired",
        "active block enrollment projection differs from history",
    )
}
