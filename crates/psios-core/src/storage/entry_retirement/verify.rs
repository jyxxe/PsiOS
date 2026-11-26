//! Validate witnessed ownership and the bounded cleanup index without reading volume paths.
use super::{KIND, Location, MAX_BYTES, MAX_ENTRIES, TOOL, fs::Entry, id};
use crate::{Result, broker::dependencies, db, decode, err, require, storage::files};
use rusqlite::{Connection, OptionalExtension, params};

fn found(conn: &Connection, id: &str, kind: &str) -> Result<db::Doc> {
    let doc = db::find(conn, id)?
        .ok_or_else(|| err("RecoveryRequired", "entry retirement reference is missing"))?;
    require(
        doc.kind == kind,
        "RecoveryRequired",
        "entry retirement reference has the wrong kind",
    )?;
    Ok(doc)
}

fn text<'a>(value: &'a serde_json::Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "invalid entry retirement reference"))
}

pub(super) fn binding(conn: &Connection, doc: &db::Doc) -> Result<(db::Doc, db::Doc)> {
    let operation_id = text(&doc.spec, "operation_id")?;
    let op = found(conn, operation_id, "Operation")?;
    let task = found(conn, text(&op.spec, "task_id")?, "Task")?;
    let volume = db::resolve(conn, &doc.spec["volume_ref"])?;
    let current = found(conn, &volume.id, "Volume")?;
    let plan = db::resolve(conn, &doc.spec["plan_ref"])?;
    let input = db::resolve(conn, &task.spec["input_ref"])?;
    let input = dependencies::tool_input(&input)?;
    let effect_ref = op.body["effect_refs"]
        .as_array()
        .and_then(|effects| effects.last())
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "entry retirement has no original effect",
            )
        })?;
    let effect = db::resolve(conn, effect_ref)?;
    let path = text(&doc.spec, "path")?;
    files::path(path, false)?;
    require(
        doc.id == id(operation_id)
            && doc.kind == KIND
            && doc.owner == task.owner
            && task.owner == op.owner
            && task.spec["task_type"] == TOOL
            && volume.kind == "Volume"
            && volume.body["data_incarnation"] == doc.spec["data_incarnation"]
            && current.body["data_incarnation"] == doc.spec["data_incarnation"]
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && op.body["plan_ref"] == plan.reference()
            && plan.body["allowed_effects"][0]["resource_ref"] == volume.reference()
            && plan.body["allowed_effects"][0]["action"] == TOOL
            && input["target"]["id"] == volume.id
            && input["target"]["expected_resource_version"] == volume.version()
            && input["path"] == path
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && effect.id == doc.spec["effect_id"]
            && effect.body["operation_id"] == op.id
            && effect.body["resource_ref"] == volume.reference()
            && effect.body["action"] == TOOL,
        "RecoveryRequired",
        "entry retirement is not bound to the approved path and Volume effect",
    )?;
    if doc.body.get("location").is_some() {
        let location: Location = decode(&doc.body["location"])?;
        location.check()?;
        location.check_volume(&volume)?;
        let started = db::resolve(conn, &doc.body["started_effect_ref"])?;
        require(
            started.kind == "Effect"
                && started.id == effect.id
                && started.owner == effect.owner
                && started.spec == effect.spec
                && started.body["phase"] == "EffectStarted"
                && started.rev <= effect.rev
                && started.body["resource_ref"] == volume.reference()
                && started.body["operation_id"] == op.id
                && started.body["action"] == TOOL,
            "RecoveryRequired",
            "entry retirement has no witnessed rename command boundary",
        )?;
    } else {
        require(
            doc.body.get("started_effect_ref").is_none()
                && matches!(
                    doc.body["lifecycle"].as_str(),
                    Some("Preparing" | "NotApplied")
                ),
            "RecoveryRequired",
            "entry retirement lost its prepared location",
        )?;
    }
    if matches!(
        doc.body["lifecycle"].as_str(),
        Some("PendingCleanup" | "Reclaimed")
    ) {
        let updated = db::resolve(conn, &doc.body["result_volume_ref"])?;
        let known = effect.body["evidence_refs"].as_array().is_some_and(|refs| {
            refs.iter().any(|reference| {
                // The exact logical retirement revision precedes any physical cleanup revision.
                reference["resource_id"] == doc.id && reference["version"] == "r3"
            })
        });
        let confirmed = conn
            .query_row(
                "SELECT * FROM versions WHERE id=?1 AND rev=3",
                [&doc.id],
                db::row,
            )
            .optional()?
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "entry retirement confirmation disappeared",
                )
            })?;
        let location: Location = decode(&doc.body["location"])?;
        let retired: Entry = decode(&doc.body["retired_entry"])?;
        location.check_retired(&retired)?;
        let mut expected_body = volume.body.clone();
        expected_body["resource_version"] = serde_json::json!(updated.version());
        require(
            updated.kind == "Volume"
                && updated.id == volume.id
                && updated.rev == volume.rev + 1
                && updated.owner == volume.owner
                && updated.spec == volume.spec
                && volume.body["resource_version"] == volume.version()
                && updated.body == expected_body
                && op.body["outcome_knowledge"] == "Known"
                && known
                && effect.body["evidence_refs"]
                    .as_array()
                    .is_some_and(|refs| refs.contains(&confirmed.reference()))
                && confirmed.body["lifecycle"] == "PendingCleanup"
                && confirmed.spec == doc.spec
                && confirmed.body["location"] == doc.body["location"]
                && confirmed.body["retired_entry"] == doc.body["retired_entry"]
                && confirmed.body["result_volume_ref"] == updated.reference(),
            "RecoveryRequired",
            "entry reclamation has no exact known logical retirement",
        )?;
    }
    Ok((op, current))
}

fn validate_selected(
    conn: &Connection,
    doc: &db::Doc,
    volume_id: &str,
    bytes: u64,
    phase: &str,
) -> Result<()> {
    let (op, volume) = binding(conn, doc)?;
    let lifecycle = text(&doc.body, "lifecycle")?;
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |row| row.get(0),
    )?;
    let observed_bytes = if lifecycle == "Preparing" {
        0
    } else {
        decode::<Location>(&doc.body["location"])?.bytes()
    };
    require(
        volume.id == volume_id
            && volume.body["lifecycle"] == "Available"
            && bytes == observed_bytes
            && match (phase, lifecycle) {
                ("Active", "Preparing" | "Prepared") => {
                    held && volume.reference() == doc.spec["volume_ref"]
                        && (op.body["outcome_knowledge"] == "InFlight"
                            || lifecycle == "Prepared" && op.body["outcome_knowledge"] == "Unknown")
                }
                ("PendingCleanup", "PendingCleanup") => {
                    !held && op.body["outcome_knowledge"] == "Known"
                }
                _ => false,
            },
        "RecoveryRequired",
        "entry retirement capacity index or hold disagrees with the witnessed state",
    )
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    let budget: (i64, i64) = conn
        .query_row(
            "SELECT entries,bytes FROM volume_entry_retirement_budget WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| err("RecoveryRequired", "entry retirement budget is missing"))?;
    let mut query = conn.prepare(
        "SELECT operation,volume,bytes,phase,eligible_at FROM volume_entry_retirements ORDER BY operation LIMIT ?1",
    )?;
    let mut rows = query.query([MAX_ENTRIES + 1])?;
    let mut used = 0;
    let mut bytes_used = 0u64;
    while let Some(row) = rows.next()? {
        let op: String = row.get(0)?;
        let bytes: i64 = row.get(2)?;
        let bytes = u64::try_from(bytes)
            .map_err(|_| err("RecoveryRequired", "invalid entry retirement byte count"))?;
        let eligible_at: String = row.get(4)?;
        used += 1;
        bytes_used = bytes_used.saturating_add(bytes);
        require(
            used <= MAX_ENTRIES
                && bytes_used <= MAX_BYTES
                && chrono::DateTime::parse_from_rfc3339(&eligible_at).is_ok(),
            "RecoveryRequired",
            "invalid entry retirement quota or cleanup time",
        )?;
        let volume: String = row.get(1)?;
        let phase: String = row.get(3)?;
        validate_selected(conn, &found(conn, &id(&op), KIND)?, &volume, bytes, &phase)?;
    }
    require(
        budget == (used, bytes_used as i64),
        "RecoveryRequired",
        "entry retirement custody index disagrees with its witnessed budget",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_active(conn)?;
    let mut query = conn.prepare("SELECT * FROM current WHERE kind=?1 ORDER BY id")?;
    let mut rows = query.query([KIND])?;
    while let Some(row) = rows.next()? {
        let doc = db::row(row)?;
        let (op, _) = binding(conn, &doc)?;
        let reserved: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM volume_entry_retirements WHERE operation=?1)",
            [&op.id],
            |row| row.get(0),
        )?;
        let lifecycle = text(&doc.body, "lifecycle")?;
        require(
            match lifecycle {
                "Preparing" | "Prepared" | "PendingCleanup" => reserved,
                "Reclaimed" | "NotApplied" => {
                    !reserved && op.body["outcome_knowledge"] != "Unknown"
                }
                _ => false,
            },
            "RecoveryRequired",
            "entry retirement journal is missing its indexed reservation",
        )?;
    }
    Ok(())
}
