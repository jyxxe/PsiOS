//! Durable, normalized health for operator-registered probes.
use super::{Doc, Result, Work, definition, err, json, namespace, require};
use crate::{
    artifact,
    db::{Db, find, resolve},
    policy,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{canonical, sha256};
use ring::digest::{SHA256, digest};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

pub(crate) const TOOL: &str = "node.observe_network_probe";
pub(crate) const FORMAT: u32 = crate::apps::restart::CUSTODY_FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE TABLE network_probe_positions (
    probe TEXT PRIMARY KEY, context TEXT NOT NULL,
    dispatched_seq INTEGER NOT NULL CHECK(dispatched_seq>=0),
    artifact_ref TEXT, state_ref TEXT
) STRICT;";
const KIND: &str = "NetworkProbeState";

fn state_id(probe: &Doc) -> String {
    // Exact Watch cursors carry the ID and must fit the v1 256-character limit.
    let hashed = digest(&SHA256, probe.id.as_bytes());
    format!("state-{}", URL_SAFE_NO_PAD.encode(&hashed.as_ref()[..16]))
}

fn context(probe: &Doc, boot: &Value, namespace_inode: &Value) -> String {
    sha256(&canonical(&json!([
        probe.reference(),
        boot,
        namespace_inode
    ])))
}

pub(super) fn sync(db: &Db<'_>, probe: &Doc, host: &super::super::Namespace) -> Result<()> {
    let id = state_id(probe);
    let spec = json!({"probe_ref":probe.reference(),"node_id":probe.spec["node_id"]});
    let retired = probe.body["lifecycle"] == "Unregistered";
    let current_context = context(
        probe,
        &json!(host.boot_id),
        &json!(host.network_namespace_inode),
    );
    let previous_context: Option<String> =
        db.c.query_row(
            "SELECT context FROM network_probe_positions WHERE probe=?1",
            [&probe.id],
            |row| row.get(0),
        )
        .optional()?;
    let initial = json!({"lifecycle": if retired { "Retired" } else { "Unknown" },
        "reason":null,"observed_at":null});
    let state = db.find(&id)?;
    if let Some(state) = &state {
        require(
            state.kind == KIND
                && state.owner == policy::SYSTEM
                && state.spec["probe_ref"]["resource_id"] == probe.id,
            "RecoveryRequired",
            "network probe state identity is already used",
        )?;
        if !retired && state.spec == spec && state.body["lifecycle"] != "Retired" {
            require(
                previous_context.is_some(),
                "RecoveryRequired",
                "registered network probe state has no durable position",
            )?;
            if previous_context.as_deref() == Some(current_context.as_str()) {
                return Ok(());
            }
        } else if retired
            && state.spec == spec
            && state.body == initial
            && previous_context.is_none()
        {
            return Ok(());
        }
        db.update(state, spec, initial)?;
    } else {
        require(
            previous_context.is_none(),
            "RecoveryRequired",
            "network probe position has no state resource",
        )?;
        db.create(KIND, &id, policy::SYSTEM, spec, initial)?;
    }
    db.c.execute(
        "DELETE FROM network_probe_positions WHERE probe=?1",
        [&probe.id],
    )?;
    if !retired {
        db.c.execute(
            "INSERT INTO network_probe_positions(probe,context,dispatched_seq) VALUES(?1,?2,0)",
            params![probe.id, current_context],
        )?;
    }
    Ok(())
}

fn normalized(observation: &Value) -> (&'static str, &'static str) {
    let dns = &observation["dns"];
    let dns_status = dns["result"]["status"].as_str();
    let tcp = observation["tcp"]["status"].as_str();
    if dns.is_null() || dns_status == Some("Verified") {
        return match tcp {
            Some("Connected") => ("Healthy", "TcpOpen"),
            Some("Refused") => ("Unhealthy", "TcpRefused"),
            Some("Unreachable") => ("Unhealthy", "TcpUnreachable"),
            Some("TimedOut") => ("Unknown", "TcpTimedOut"),
            Some("Denied") => ("Unknown", "TcpDenied"),
            _ => ("Unknown", "TcpUnverified"),
        };
    }
    if tcp != Some("Skipped") {
        return ("Unknown", "InvalidMeasurement");
    }
    match dns_status {
        Some("AddressMismatch") => ("Unhealthy", "DnsAddressMismatch"),
        Some("NoAddress") => ("Unhealthy", "DnsNoAddress"),
        Some("ResponseError") if dns["result"]["response_code"] == 3 => {
            ("Unhealthy", "DnsNameMissing")
        }
        Some("TimedOut") => ("Unknown", "DnsTimedOut"),
        Some("InvalidResponse") => ("Unknown", "DnsInvalidResponse"),
        _ => ("Unknown", "DnsUnverified"),
    }
}

fn dispatch_position(conn: &Connection, work: &Work) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![work.op.id, work.op.rev],
        |row| row.get(0),
    )?)
}

fn receipt(state: &Doc, measurement: &Value, projection: &str) -> Value {
    json!({"network_probe_state_ref":state.reference(),"measurement":measurement,"projection":projection})
}

pub(crate) fn publish(db: &Db<'_>, work: &Work, measurement: &Value) -> Result<Value> {
    let probe = db.of(&work.target.id, super::KIND)?;
    let state = db.of(&state_id(&probe), KIND)?;
    let host = namespace()?;
    if probe.reference() != work.target.reference()
        || state.spec["probe_ref"] != probe.reference()
        || state.body["lifecycle"] == "Retired"
        || measurement["boot_id"] != host.boot_id
        || measurement["network_namespace_inode"] != host.network_namespace_inode
    {
        return Ok(receipt(&state, measurement, "Superseded"));
    }
    let config = definition(db.cfg, &probe)?;
    let raw = &measurement["observation"];
    let dns_bound = config.dns.as_ref().map_or_else(
        || raw["dns"].is_null(),
        |dns| raw["dns"]["resolver"] == json!(dns.resolver) && raw["dns"]["name"] == dns.name,
    );
    let stamp = measurement["observed_at"]
        .as_str()
        .filter(|value| value.len() <= 40 && chrono::DateTime::parse_from_rfc3339(value).is_ok())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "network probe measurement timestamp is invalid",
            )
        })?;
    require(
        measurement["target_ref"] == probe.reference()
            && raw["endpoint"] == json!(config.endpoint)
            && raw["tcp"]["status"].is_string()
            && dns_bound,
        "RecoveryRequired",
        "network probe measurement does not describe the configured target",
    )?;
    let scope = context(
        &probe,
        &measurement["boot_id"],
        &measurement["network_namespace_inode"],
    );
    let position = dispatch_position(db.c, work)?;
    let accepted = db.c.execute(
        "UPDATE network_probe_positions SET dispatched_seq=?3,artifact_ref=NULL,state_ref=NULL
         WHERE probe=?1 AND context=?2 AND dispatched_seq<?3",
        params![probe.id, scope, position],
    )?;
    if accepted == 0 {
        let expected: Option<String> =
            db.c.query_row(
                "SELECT context FROM network_probe_positions WHERE probe=?1",
                [&probe.id],
                |row| row.get(0),
            )
            .optional()?;
        require(
            expected.as_deref() == Some(scope.as_str()),
            "RecoveryRequired",
            "network probe projection context is missing",
        )?;
        return Ok(receipt(&state, measurement, "Superseded"));
    }
    let (lifecycle, reason) = normalized(raw);
    if state.body["lifecycle"] == lifecycle && state.body["reason"] == reason {
        return Ok(receipt(&state, measurement, "Unchanged"));
    }
    let body = json!({"lifecycle":lifecycle,"reason":reason,"observed_at":stamp});
    Ok(receipt(&db.status(&state, body)?, measurement, "Published"))
}

/// Called by the broker in the same transaction as state, result and Task settlement.
pub(crate) fn bind_result(db: &Db<'_>, work: &Work, result: &Value, artifact: &Doc) -> Result<()> {
    if result["projection"] == "Superseded" {
        return Ok(());
    }
    require(
        db.c.execute(
            "UPDATE network_probe_positions SET artifact_ref=?3,state_ref=?4
             WHERE probe=?1 AND dispatched_seq=?2 AND artifact_ref IS NULL AND state_ref IS NULL",
            params![
                work.target.id,
                dispatch_position(db.c, work)?,
                artifact.reference(),
                result["network_probe_state_ref"]
            ],
        )? == 1,
        "RecoveryRequired",
        "network probe receipt cannot bind its durable projection",
    )
}

pub(crate) fn retains_artifact(conn: &Connection, artifact: &Doc) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM network_probe_positions WHERE artifact_ref=?1)",
        [artifact.reference()],
        |row| row.get(0),
    )?)
}

fn current(conn: &Connection, id: &Value) -> Result<Doc> {
    let id = id.as_str().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "network probe provenance identity is missing",
        )
    })?;
    find(conn, id)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "network probe provenance resource is missing",
        )
    })
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut positions = conn.prepare(
        "SELECT probe,context,dispatched_seq,artifact_ref,state_ref FROM network_probe_positions",
    )?;
    for position in positions.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<Value>>(3)?,
            r.get::<_, Option<Value>>(4)?,
        ))
    })? {
        let (id, scope, seq, artifact_ref, state_ref) = position?;
        let probe = current(conn, &json!(id))?;
        let state = current(conn, &json!(state_id(&probe)))?;
        require(
            probe.kind == super::KIND
                && probe.body["lifecycle"] == "Registered"
                && state.kind == KIND
                && state.owner == policy::SYSTEM
                && state.spec["probe_ref"] == probe.reference()
                && state.spec["node_id"] == probe.spec["node_id"]
                && state.body["lifecycle"] != "Retired"
                && scope.starts_with("sha256:")
                && scope.len() == 71,
            "RecoveryRequired",
            "network probe position has no exact registered state",
        )?;
        if seq == 0 {
            require(
                artifact_ref.is_none()
                    && state_ref.is_none()
                    && state.body["lifecycle"] == "Unknown"
                    && state.body["reason"].is_null()
                    && state.body["observed_at"].is_null(),
                "RecoveryRequired",
                "network probe without a measurement has a published result",
            )?;
            continue;
        }
        require(
            state_ref == Some(state.reference()),
            "RecoveryRequired",
            "network probe receipt points at a stale state",
        )?;
        let artifact = resolve(
            conn,
            &artifact_ref
                .ok_or_else(|| err("RecoveryRequired", "network probe receipt is missing"))?,
        )?;
        let task = current(conn, &artifact.spec["task_id"])?;
        let operation = resolve(conn, &task.body["operation_refs"][0])?;
        let run = current(conn, &artifact.spec["run_id"])?;
        let plan = resolve(conn, &operation.body["plan_ref"])?;
        let effect_ref = operation.body["effect_refs"]
            .as_array()
            .and_then(|refs| refs.last())
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "network probe receipt has no producing effect",
                )
            })?;
        let effect = resolve(conn, effect_ref)?;
        let ordered: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE seq=?1 AND kind='Operation' AND id=?2
                 AND body->>'outcome_knowledge'='InFlight'
                 AND json_extract(body,'$.effect_refs[#-1].resource_id')=?3)",
            params![seq, operation.id, effect.id],
            |r| r.get(0),
        )?;
        require(
            ordered
                && artifact::is_json(&artifact)
                && artifact.rev == 1
                && artifact::is_task_output(&artifact, &task)
                && task.kind == "Task"
                && task.spec["task_type"] == TOOL
                && task.body["lifecycle"] == "Succeeded"
                && task.body["outcome_knowledge"] == "Known"
                && task.body["result"]["availability"] == "Available"
                && task.body["result"]["artifact_refs"] == json!([artifact.reference()])
                && run.kind == "Run"
                && run.owner == task.owner
                && run.body["task_id"] == task.id
                && run.body["lifecycle"] == "Succeeded"
                && run.body["output_refs"] == json!([artifact.reference()])
                && run.body["operation_refs"] == task.body["operation_refs"]
                && operation.kind == "Operation"
                && operation.id == artifact.spec["operation_id"]
                && operation.owner == task.owner
                && operation.spec["task_id"] == task.id
                && plan.kind == "OperationPlan"
                && plan.body["allowed_effects"][0]["resource_ref"] == probe.reference()
                && plan.body["allowed_effects"][0]["action"] == TOOL
                && effect.kind == "Effect"
                && effect.id == artifact.spec["effect_id"]
                && effect.owner == task.owner
                && effect.body["operation_id"] == operation.id
                && effect.body["phase"] == "EffectConfirmed"
                && effect.body["action"] == TOOL
                && effect.body["resource_ref"] == probe.reference()
                && effect.body["evidence_refs"] == json!([artifact.reference()]),
            "RecoveryRequired",
            "network probe receipt lineage disagrees with its settled observation",
        )?;
        let bytes = artifact::checkpoint_bytes(conn, &artifact, 64 * 1024)?;
        let result: Value = serde_json::from_slice(&bytes)?;
        let measurement = &result["measurement"];
        let statuses = normalized(&measurement["observation"]);
        require(
            result["network_probe_state_ref"] == state.reference()
                && matches!(
                    result["projection"].as_str(),
                    Some("Published" | "Unchanged")
                )
                && measurement["target_ref"] == probe.reference()
                && context(
                    &probe,
                    &measurement["boot_id"],
                    &measurement["network_namespace_inode"],
                ) == scope
                && state.body["lifecycle"] == statuses.0
                && state.body["reason"] == statuses.1,
            "RecoveryRequired",
            "network probe state does not match its observation",
        )?;
    }
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM current WHERE kind=?1 AND body->>'lifecycle'='Registered'
           AND NOT EXISTS(SELECT 1 FROM network_probe_positions p WHERE p.probe=current.id))",
        [super::KIND],
        |r| r.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "registered network probe position is missing",
    )
}
