use crate::{
    Result,
    db::{Db, Doc},
    err,
    host::Work,
    policy, require,
};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

pub const TOOL: &str = "service.observe";
pub const FORMAT: u32 = 36;
pub const SCHEMA: &str = include_str!("service_state.sql");
const KIND: &str = "HostServiceState";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub id: String,
    pub expected_spec_generation: String,
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        require(
            self.expected_spec_generation
                .parse::<i64>()
                .is_ok_and(|n| n > 0 && n.to_string() == self.expected_spec_generation),
            "InvalidArgument",
            "expected a positive decimal service spec generation",
        )
    }

    pub fn target(&self, db: &Db<'_>) -> Result<Doc> {
        let service = db.of(&self.id, "HostService")?;
        super::require_registered(&service)?;
        require(
            service.generation.to_string() == self.expected_spec_generation,
            "Conflict",
            "service specification changed",
        )?;
        Ok(service)
    }
}

fn state_id(service: &Doc) -> String {
    format!("state-{}", service.id)
}

pub(super) fn sync(db: &Db<'_>, service: &Doc, boot: &str) -> Result<()> {
    let id = state_id(service);
    let retired = service.body["lifecycle"] == "Unregistered";
    let spec =
        json!({"service_id":service.id,"service_spec_generation":service.generation.to_string()});
    let lifecycle = if retired { "Retired" } else { "Unknown" };
    let initial =
        json!({"lifecycle":lifecycle,"boot_id":boot,"observation":null,"observed_at":null});
    match db.find(&id)? {
        Some(state) => {
            require(
                state.kind == KIND,
                "RecoveryRequired",
                "service observation identity is already used",
            )?;
            if state.spec != spec
                || state.body["boot_id"] != boot
                || (state.body["lifecycle"] == "Retired") != retired
            {
                db.update(&state, spec, initial)?;
            }
        }
        None => {
            db.create(KIND, &id, policy::SYSTEM, spec, initial)?;
        }
    }
    Ok(())
}

fn lifecycle(observed: &Value) -> &'static str {
    match observed["load_state"].as_str() {
        Some("not-found") => return "Missing",
        Some("masked") => return "Masked",
        Some("bad-setting" | "error") => return "Invalid",
        _ => {}
    }
    match observed["active_state"].as_str() {
        Some("active") => "Active",
        Some("activating") => "Starting",
        Some("deactivating") => "Stopping",
        Some("failed") => "Failed",
        Some("inactive") => "Inactive",
        Some("reloading" | "refreshing") => "Reloading",
        Some("maintenance") => "Maintenance",
        _ => "Unknown",
    }
}

pub fn publish(db: &Db<'_>, work: &Work, observed: &Value) -> Result<Value> {
    let current = db.of(&work.target.id, "HostService")?;
    let state = db.of(&state_id(&current), KIND)?;
    if current.rev != work.target.rev || state.body["boot_id"] != observed["boot_id"] {
        return Ok(
            json!({"service_state_ref":state.reference(),"observation":observed,"projection":"Superseded"}),
        );
    }
    require(
        state.spec["service_spec_generation"]
            .as_str()
            .and_then(|generation| generation.parse().ok())
            == Some(current.generation)
            && state.body["lifecycle"] != "Retired"
            && observed["unit"] == current.spec["unit"],
        "RecoveryRequired",
        "service observation no longer matches its configured subject",
    )?;
    let seq: i64 = db.c.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![work.op.id, work.op.rev],
        |row| row.get(0),
    )?;
    require(
        seq > 0,
        "RecoveryRequired",
        "observation has no durable dispatch position",
    )?;
    // A later dispatch can settle with the same observation. Keep its position
    // without introducing a new resource revision or a false state-change event.
    let accepted = db.c.execute(
        "INSERT INTO service_observation_positions(service,dispatched_seq) VALUES(?1,?2)
         ON CONFLICT(service) DO UPDATE SET dispatched_seq=excluded.dispatched_seq
         WHERE excluded.dispatched_seq > service_observation_positions.dispatched_seq",
        params![current.id, seq],
    )?;
    let value = json!({
        "load_state":observed["load_state"], "active_state":observed["active_state"], "sub_state":observed["sub_state"],
        "invocation_id":observed["invocation_id"], "needs_reload":observed["needs_reload"],
        "definition_digest":observed["definition_digest"]
    });
    let (state, projection) = if accepted == 0 {
        (state, "Superseded")
    } else if state.body["observation"] == value {
        (state, "Unchanged")
    } else {
        let timestamp = observed["observed_at"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "service observation has no timestamp"))?;
        let body = json!({"lifecycle":lifecycle(observed),"boot_id":observed["boot_id"],
            "observation":value,"observed_at":timestamp});
        (db.status(&state, body)?, "Published")
    };
    Ok(
        json!({"service_state_ref":state.reference(),"observation":observed,"projection":projection}),
    )
}
