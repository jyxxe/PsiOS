//! Offline verification of exact Session inputs and brokered custody.
use super::*;
use crate::db::{find, row};

fn task_blueprint(conn: &Connection, task: &Doc) -> Result<SessionBlueprint> {
    let input = resolve(conn, &task.spec["input_ref"])?;
    require(
        task.kind == "Task" && task.spec["task_type"] == START_SESSION && input.owner == task.owner,
        "RecoveryRequired",
        "brokered session input disagrees with its original Task",
    )?;
    let blueprint: SessionBlueprint = decode(broker::dependencies::tool_input(&input)?)?;
    blueprint.validate()?;
    Ok(blueprint)
}

fn verify_task_plan(conn: &Connection, task: &Doc, blueprint: &SessionBlueprint) -> Result<Doc> {
    let operation = resolve(conn, &task.body["operation_refs"][0])?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let audience = std::iter::once(blueprint.model_ref.clone())
        .chain(std::iter::once(blueprint.skill_ref.clone()))
        .chain(
            blueprint
                .input_artifacts
                .iter()
                .map(|input| json!(input.reference)),
        );
    let preconditions = audience
        .map(|reference| {
            json!({"resource_ref":reference,"kind":"ContentDigest",
            "expected":reference["digest"],"enforcement":"ConditionalApply"})
        })
        .collect::<Vec<_>>();
    require(
        operation.kind == "Operation"
            && operation.owner == task.owner
            && operation.spec["task_id"] == task.id
            && plan.kind == "OperationPlan"
            && plan.owner == task.owner
            && plan.spec["action"] == START_SESSION
            && plan.body["purpose_ref"] == task.spec["input_ref"]
            && plan.body["delegation_ref"] == task.spec["delegation_ref"]
            && plan.body["preconditions"] == json!(preconditions),
        "RecoveryRequired",
        "session launch plan does not bind the exact private inputs",
    )?;
    Ok(operation)
}

fn verify_task_origin(conn: &Connection, original: &Doc) -> Result<()> {
    let reference = &original.spec["origin_task_ref"];
    let task = resolve(conn, reference)?;
    let blueprint = task_blueprint(conn, &task)?;
    let mut expected = blueprint.opening(&task)?;
    expected["tool_context_window"] = json!(blueprint.tool_context_window.unwrap_or(1));
    expected["origin_task_ref"] = reference.clone();
    require(
        task.owner == original.owner
            && task.body["outcome_knowledge"] == "InFlight"
            && task.spec["delegation_ref"] == original.body["delegation_ref"]
            && original.body["origin_task_ref"] == *reference
            && original.spec == expected
            && original.id == registry::stable("session", json!([original.owner, "task", task.id])),
        "RecoveryRequired",
        "brokered session input custody differs from the issued launch",
    )?;
    let operation = verify_task_plan(conn, &task, &blueprint)?;
    let settled = find(conn, &task.id)?
        .ok_or_else(|| err("RecoveryRequired", "session launch receipt is missing"))?;
    let artifacts = &settled.body["result"]["artifact_refs"];
    require(
        settled.body["lifecycle"] == "Succeeded"
            && settled.body["outcome_knowledge"] == "Known"
            && settled.spec == task.spec
            && artifacts.as_array().is_some_and(|items| items.len() == 1),
        "RecoveryRequired",
        "brokered session has no settled original launch",
    )?;
    let receipt = resolve(conn, &artifacts[0])?;
    require(
        receipt.kind == "Artifact"
            && receipt.owner == original.owner
            && artifact::is_json(&receipt)
            && receipt.spec["task_id"] == task.id
            && receipt.spec["operation_id"] == operation.id
            && receipt.spec["run_id"] == task.body["current_run_id"]
            && psios_api::parse(&artifact::checkpoint_bytes(conn, &receipt, 1024)?)?
                == json!({"session_ref":original.reference()}),
        "RecoveryRequired",
        "session launch result does not attest the original session",
    )
}

fn verify_task_holds(conn: &Connection) -> Result<()> {
    let mut pending = conn.prepare(
        "SELECT q.task,q.phase FROM queue q JOIN current t ON t.id=q.task
         WHERE q.phase<>'Done' AND t.spec->>'task_type'=?1",
    )?;
    let mut expected = 0i64;
    for entry in pending.query_map([START_SESSION], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, phase) = entry?;
        let task = find(conn, &id)?
            .ok_or_else(|| err("RecoveryRequired", "input custody Task is missing"))?;
        let blueprint = task_blueprint(conn, &task)?;
        if blueprint.input_artifacts.is_empty() {
            continue;
        }
        require(
            matches!(
                phase.as_str(),
                "Queued" | "Claimed" | "WaitingResource" | "WaitingApproval"
            ),
            "RecoveryRequired",
            "atomic private session launch has an unresolved effect",
        )?;
        verify_task_plan(conn, &task, &blueprint)?;
        require_task_holds(conn, &task, &blueprint.input_artifacts)?;
        for input in &blueprint.input_artifacts {
            let artifact = source(conn, &task.owner, input)?;
            value(conn, &artifact, MAX_INPUT_BYTES)?;
        }
        expected += blueprint.input_artifacts.len() as i64;
    }
    let actual = conn.query_row(
        "SELECT count(*) FROM brokered_session_input_holds",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    require(
        actual == expected,
        "RecoveryRequired",
        "brokered session input contains an unbound Task hold",
    )
}

pub(crate) fn verify(conn: &Connection, database_format: u32) -> Result<()> {
    if database_format >= BROKERED_FORMAT {
        verify_task_holds(conn)?;
    }
    let mut sessions = conn.prepare(
        "SELECT * FROM current WHERE kind='Session' AND spec->>'input_artifacts' IS NOT NULL",
    )?;
    let mut expected_holds = 0i64;
    for session in sessions.query_map([], row)? {
        let session = session?;
        let inputs = configured(&session)?;
        if inputs.is_empty() {
            continue;
        }
        let original = conn.query_row(
            "SELECT * FROM versions WHERE id=?1 AND rev=1",
            [&session.id],
            row,
        )?;
        require(
            original.kind == "Session"
                && original.owner == session.owner
                && original.spec == session.spec
                && original.body["delegation_ref"] == session.body["delegation_ref"]
                && (session.spec["provider_queue"] != true || database_format >= QUEUED_FORMAT)
                && !session.spec["fallback_model_refs"]
                    .as_array()
                    .is_some_and(|refs| !refs.is_empty())
                && tool_context::enabled(&session),
            "RecoveryRequired",
            "session input differs from its original configuration",
        )?;
        if original.spec.get("origin_task_ref").is_some() {
            require(
                database_format >= BROKERED_FORMAT
                    && original.body["origin_task_ref"] == session.body["origin_task_ref"],
                "RecoveryRequired",
                "brokered private session predates its custody format or changed its origin",
            )?;
            verify_task_origin(conn, &original)?;
        }
        for input in inputs {
            let artifact = source(conn, &session.owner, &input)?;
            if !matches!(state(&session), "Succeeded" | "Failed" | "Cancelled") {
                require(
                    hold_exists(conn, &session.id, &artifact.id)?,
                    "RecoveryRequired",
                    "session input hold is missing",
                )?;
                value(conn, &artifact, MAX_INPUT_BYTES)?;
                expected_holds += 1;
            }
        }
    }
    let actual_holds = conn.query_row("SELECT count(*) FROM session_input_holds", [], |row| {
        row.get::<_, i64>(0)
    })?;
    require(
        actual_holds == expected_holds,
        "RecoveryRequired",
        "session input contains an unbound hold",
    )
}
