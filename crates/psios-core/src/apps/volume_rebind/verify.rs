use super::{Input, TOOL, activation, lineage};
use crate::{
    Result,
    db::{Doc, find, resolve, row},
    err, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

fn record_missing() -> crate::Error {
    err(
        "RecoveryRequired",
        "application volume rebind lost an exact historical record",
    )
}

fn check_binding(
    conn: &Connection,
    deployment: &Doc,
    previous_app: &Doc,
    app: &Doc,
    format: u32,
) -> Result<()> {
    let proof = &deployment.spec["restore_rebind"];
    let prior = resolve(conn, &proof["previous_deployment_ref"])?;
    let source = resolve(conn, &proof["source_volume_ref"])?;
    let restored = resolve(conn, &proof["restored_volume_ref"])?;
    let lineage = lineage(conn, &source, &restored)?;
    let evidence = activation(conn, &restored)?;
    let issued = resolve(conn, &proof["operation_ref"])?;
    let operation = find(conn, &issued.id)?.ok_or_else(record_missing)?;
    let plan = resolve(conn, &issued.body["plan_ref"])?;
    let task_id = operation.spec["task_id"]
        .as_str()
        .ok_or_else(record_missing)?;
    let task = find(conn, task_id)?.ok_or_else(record_missing)?;
    let input = resolve(conn, &task.spec["input_ref"])?;
    let request: Input = crate::decode(crate::broker::dependencies::tool_input(&input)?)?;
    let mut config = prior.spec.clone();
    config["volume_id"] = json!(restored.id);
    config["data_incarnation"] = restored.body["data_incarnation"].clone();
    config["restore_rebind"] = proof.clone();
    let mut app_spec = previous_app.spec.clone();
    app_spec["volume_id"] = config["volume_id"].clone();
    app_spec["data_incarnation"] = config["data_incarnation"].clone();
    app_spec["deployment_ref"] = deployment.reference();
    require(
        deployment.kind == "Deployment"
            && deployment.rev == 1
            && deployment.owner == app.owner
            && deployment.spec == config
            && deployment.body == json!({"lifecycle":"Prepared","desired_state":"Stopped"})
            && prior.kind == "Deployment"
            && prior.reference() == previous_app.spec["deployment_ref"]
            && prior.spec["application_id"] == app.id
            && prior.spec["volume_id"] == source.id
            && prior.spec["data_incarnation"] == source.body["data_incarnation"]
            && source.kind == "Volume"
            && restored.kind == "Volume"
            && source.id != restored.id
            && app.kind == "Application"
            && app.id == previous_app.id
            && app.rev == previous_app.rev + 1
            && app.spec == app_spec
            && app.body == previous_app.body
            && app.body["lifecycle"] == "Stopped"
            && app.body["runtime_state"] == "Absent"
            && proof["previous_application_ref"] == previous_app.reference()
            && proof["activation_ref"] == evidence.reference()
            && lineage.matches(proof, format),
        "RecoveryRequired",
        "application volume rebind history has inconsistent deployment or restore lineage",
    )?;
    let audience = plan.body["preconditions"]
        .as_array()
        .ok_or_else(record_missing)?;
    require(
        issued.kind == "Operation"
            && issued.body["outcome_knowledge"] == "InFlight"
            && operation.kind == "Operation"
            && operation.body["plan_ref"] == issued.body["plan_ref"]
            && operation.body["outcome_knowledge"] == "Known"
            && operation.body["lifecycle"] == "Succeeded"
            && task.kind == "Task"
            && task.owner == operation.owner
            && task.spec["task_type"] == TOOL
            && task.body["lifecycle"] == "Succeeded"
            && task.body["outcome_knowledge"] == "Known"
            && task.body["operation_refs"] == json!([operation.reference()])
            && request.target.id == previous_app.id
            && request.target.expected_resource_version == previous_app.version()
            && json!(request.source_volume_ref) == source.reference()
            && json!(request.restored_volume_ref) == restored.reference()
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == TOOL
            && plan.body["allowed_effects"][0]["resource_ref"] == previous_app.reference()
            && audience
                .iter()
                .map(|entry| entry["resource_ref"].clone())
                .eq([
                    previous_app.reference(),
                    source.reference(),
                    restored.reference(),
                ]),
        "RecoveryRequired",
        "volume rebind history does not match a settled approved operation",
    )
}

pub(crate) fn check(conn: &Connection, format: u32) -> Result<()> {
    let result = check_history(conn, format);
    result.map_err(|reason| {
        if reason.0.code == "RecoveryRequired" {
            reason
        } else {
            err(
                "RecoveryRequired",
                format!("volume rebind history: {}", reason.0.message),
            )
        }
    })
}

fn check_history(conn: &Connection, format: u32) -> Result<()> {
    let mut transitions = conn.prepare(
        "SELECT v.* FROM versions v JOIN versions p ON v.id=p.id AND p.rev=v.rev-1
         WHERE v.kind='Application' AND (v.spec->>'volume_id' IS NOT p.spec->>'volume_id'
           OR v.spec->>'data_incarnation' IS NOT p.spec->>'data_incarnation')",
    )?;
    let mut rows = transitions.query([])?;
    while let Some(line) = rows.next()? {
        let app = row(line)?;
        let seq: i64 = line.get("seq")?;
        let correlation: String = line.get("correlation")?;
        let prior = conn
            .query_row(
                "SELECT * FROM versions WHERE id=?1 AND rev=?2",
                params![app.id, app.rev - 1],
                row,
            )
            .optional()?
            .ok_or_else(record_missing)?;
        let deployment = conn
            .query_row(
                "SELECT * FROM versions WHERE seq=?1 AND correlation=?2",
                params![seq - 1, correlation],
                row,
            )
            .optional()?
            .ok_or_else(record_missing)?;
        let superseded = conn
            .query_row(
                "SELECT * FROM versions WHERE seq=?1 AND correlation=?2",
                params![seq - 2, correlation],
                row,
            )
            .optional()?
            .ok_or_else(record_missing)?;
        let previous = resolve(conn, &prior.spec["deployment_ref"])?;
        require(
            superseded.id == previous.id
                && superseded.rev == previous.rev + 1
                && superseded.spec == previous.spec
                && superseded.body == json!({"lifecycle":"Superseded","desired_state":"Stopped"}),
            "RecoveryRequired",
            "previous volume binding was not superseded in the same transaction",
        )?;
        check_binding(conn, &deployment, &prior, &app, format)?;
    }
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions d WHERE d.kind='Deployment' AND d.rev=1
          AND d.spec->>'restore_rebind' IS NOT NULL AND NOT EXISTS (
              SELECT 1 FROM versions a JOIN versions prior ON a.id=prior.id AND prior.rev=a.rev-1
               WHERE a.kind='Application' AND a.seq=d.seq+1 AND a.correlation=d.correlation
                 AND a.spec->'deployment_ref'->>'resource_id'=d.id
                 AND a.spec->>'volume_id' IS NOT prior.spec->>'volume_id'))",
        [],
        |row| row.get(0),
    )?;
    require(
        !orphan,
        "RecoveryRequired",
        "orphan restored-volume deployment",
    )
}
