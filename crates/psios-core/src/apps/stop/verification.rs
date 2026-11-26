//! Verify the witnessed stop journal without contacting Docker.
use super::*;
use crate::db::{find, resolve, row};

pub(super) struct Intent {
    doc: Doc,
    pub(super) dispatch: Option<Doc>,
    pub(super) removal: Option<Doc>,
}

pub(super) fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT seq FROM current WHERE id=?1 AND kind='OCIStopCheckpointEpoch'
         AND rev=1 AND owner=?2 AND body->>'format'=?3",
        params![EPOCH, crate::policy::SYSTEM, FORMAT],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| err("RecoveryRequired", "OCI stop checkpoint epoch is missing"))
}

fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "OCI stop journal resource is missing"))
}

fn marker(
    conn: &Connection,
    op: &Doc,
    intent: &Doc,
    kind: &str,
    field: &str,
) -> Result<Option<Doc>> {
    let found = find(conn, &id(kind, &op.id))?;
    require(
        match (intent.body.get(field), &found) {
            (None, None) => true,
            (Some(reference), Some(checkpoint)) => {
                bound(checkpoint, kind, op) && *reference == checkpoint.reference()
            }
            _ => false,
        },
        "RecoveryRequired",
        "OCI stop phase does not match its witnessed intent",
    )?;
    Ok(found)
}

pub(super) fn intent(conn: &Connection, op: &Doc, epoch: i64) -> Result<Option<Intent>> {
    let current_effect = resolve(conn, &op.body["effect_refs"][0])?;
    let (effect, seq): (Doc, i64) = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&current_effect.id],
        |r| Ok((row(r)?, r.get("seq")?)),
    )?;
    let tool = effect.body["action"].as_str().unwrap_or_default();
    let initial_phase = if seq < epoch {
        "EffectStarted"
    } else {
        "PermitBound"
    };
    require(
        op.kind == "Operation"
            && op.body["effect_refs"] == json!([current_effect.reference()])
            && effect.kind == "Effect"
            && current_effect.kind == effect.kind
            && current_effect.owner == effect.owner
            && current_effect.spec == effect.spec
            && effect.owner == op.owner
            && effect.body["operation_id"] == op.id
            && effect.body["phase"] == initial_phase
            && is_tool(tool),
        "RecoveryRequired",
        "OCI stop effect does not bind its original operation",
    )?;
    let doc = find(conn, &id(INTENT, &op.id))?;
    if seq < epoch {
        require(
            doc.is_none() && tool == "application.stop",
            "RecoveryRequired",
            "legacy OCI stop has an invalid dispatch intent",
        )?;
        return Ok(None);
    }
    let doc = doc.ok_or_else(|| err("RecoveryRequired", "OCI stop dispatch intent is missing"))?;
    let issued = resolve(conn, &doc.body["operation_ref"])?;
    let plan = resolve(conn, &doc.body["plan_ref"])?;
    let run = resolve(conn, &doc.body["run_ref"])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let cleanup = tool == TOOL;
    let binding = if cleanup {
        &doc.body["custody"]["binding"]
    } else {
        &doc.body["binding"]
    };
    require(
        bound(&doc, INTENT, op)
            && issued.kind == "Operation"
            && issued.id == op.id
            && issued.owner == op.owner
            && issued.spec == op.spec
            && issued.body["plan_ref"] == plan.reference()
            && issued.body["effect_refs"] == json!([effect.reference()])
            && issued.body["outcome_knowledge"] == "InFlight"
            && doc.body["effect_ref"] == effect.reference()
            && run.kind == "Run"
            && run.owner == op.owner
            && run.body["task_id"] == op.spec["task_id"]
            && run.body["operation_refs"] == json!([issued.reference()])
            && run.body["outcome_knowledge"] == "InFlight"
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && plan.body["allowed_effects"][0]["action"] == tool
            && plan.body["allowed_effects"][0]["resource_ref"] == binding["application_ref"]
            && effect.body["resource_ref"] == binding["application_ref"]
            && permit.kind == "ExecutionPermit"
            && permit.owner == op.owner
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"],
        "RecoveryRequired",
        "OCI stop intent disagrees with the authorized effect",
    )?;
    let dispatch = marker(
        conn,
        op,
        &doc,
        if cleanup { CLEANUP } else { STOP },
        "dispatch_ref",
    )?;
    let removal = marker(conn, op, &doc, REMOVE, "removal_ref")?;
    require(
        doc.rev == 1 + i64::from(dispatch.is_some()) + i64::from(removal.is_some())
            && (op.body["outcome_knowledge"] != "Unknown" || dispatch.is_some())
            && (!cleanup || op.body["lifecycle"] != "Succeeded" || dispatch.is_some()),
        "RecoveryRequired",
        "OCI stop journal disagrees with the recorded outcome",
    )?;
    let started_effect = if let Some(checkpoint) = &dispatch {
        let started = resolve(conn, &checkpoint.body["started_effect_ref"])?;
        let running = resolve(conn, &checkpoint.body["started_operation_ref"])?;
        let mut body = effect.body.clone();
        body["phase"] = json!("EffectStarted");
        require(
            started.kind == "Effect"
                && started.id == effect.id
                && started.rev == effect.rev + 1
                && started.spec == effect.spec
                && started.owner == effect.owner
                && started.body == body
                && running.kind == "Operation"
                && running.id == op.id
                && running.rev == issued.rev + 1
                && running.spec == issued.spec
                && running.owner == issued.owner
                && running.body["outcome_knowledge"] == "InFlight"
                && running.body["plan_ref"] == issued.body["plan_ref"]
                && running.body["effect_refs"] == json!([started.reference()]),
            "RecoveryRequired",
            "OCI stop command checkpoint has no started effect",
        )?;
        Some(started)
    } else {
        None
    };
    let phase_matches = match op.body["outcome_knowledge"].as_str() {
        Some("InFlight") => {
            current_effect.reference() == started_effect.as_ref().unwrap_or(&effect).reference()
        }
        Some("NotStarted") => {
            started_effect.is_none()
                && current_effect.body["phase"] == "PermitBound"
                && current_effect.rev == effect.rev + 1
        }
        Some("Unknown") => {
            started_effect.is_some()
                && current_effect.body["phase"] == "EffectStarted"
                && current_effect.rev == effect.rev + 2
        }
        Some("Known") => {
            matches!(
                current_effect.body["phase"].as_str(),
                Some("EffectConfirmed" | "EffectReconciled")
            ) && (started_effect.is_some()
                || (!cleanup && current_effect.body["phase"] == "EffectConfirmed"))
        }
        _ => false,
    };
    require(
        phase_matches && current_effect.body["outcome_knowledge"] == op.body["outcome_knowledge"],
        "RecoveryRequired",
        "OCI stop effect phase disagrees with the witnessed outcome",
    )?;
    if cleanup {
        if let Some(sample) = &dispatch {
            require(
                sample.body["custody"] == doc.body["custody"]
                    && exact_exited(binding, &sample.body["observation"])
                    && started(&sample.body["observation"])
                        == doc.body["custody"]["started_at"].as_str(),
                "RecoveryRequired",
                "OCI cleanup checkpoint does not match the original stopped invocation",
            )?;
        }
        require(
            removal.is_none(),
            "RecoveryRequired",
            "OCI cleanup has an unexpected stop phase",
        )?;
    } else if let Some(sample) = &dispatch {
        require(
            sample.body["binding"] == *binding
                && exact_instance(binding, &sample.body["observation"])
                && started(&sample.body["observation"]).is_some(),
            "RecoveryRequired",
            "OCI stop checkpoint does not match the bound invocation",
        )?;
        if let Some(remove) = &removal {
            require(
                remove.body["binding"] == *binding
                    && exact_exited(binding, &remove.body["observation"])
                    && started(&remove.body["observation"]) == started(&sample.body["observation"]),
                "RecoveryRequired",
                "OCI removal checkpoint does not follow its original stop",
            )?;
        }
    } else {
        require(
            removal.is_none(),
            "RecoveryRequired",
            "OCI removal precedes its stop checkpoint",
        )?;
    }
    Ok(Some(Intent {
        doc,
        dispatch,
        removal,
    }))
}

fn check_link(conn: &Connection, journal: &Intent, task: &Doc, epoch: i64) -> Result<Doc> {
    let custody = &journal.doc.body["custody"];
    let original = resolve(conn, &custody["original_operation_ref"])?;
    let original_journal = intent(conn, &original, epoch)?
        .ok_or_else(|| err("RecoveryRequired", "OCI cleanup references a legacy stop"))?;
    let dispatch = original_journal.dispatch.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI cleanup has no original stop checkpoint",
        )
    })?;
    let input = resolve(conn, &task.spec["input_ref"])?;
    let request: Request = crate::decode(crate::broker::dependencies::tool_input(&input)?)?;
    let proof = evidence::record(
        conn,
        task,
        task.body["outcome_knowledge"] == "InFlight" && journal.dispatch.is_none(),
    )?;
    require(
        dispatch.kind == STOP
            && original.body["outcome_knowledge"] == "Unknown"
            && original_journal.removal.is_none()
            && request.operation_ref == original.reference()
            && request.evidence_ref == custody["exit_evidence_ref"]
            && journal.doc.body["exit_provenance_ref"] == proof.reference()
            && proof.body["snapshot"]["artifact_ref"] == request.evidence_ref
            && proof.body["binding"] == custody["binding"]
            && started(&proof.body["snapshot"]["observation"]) == custody["started_at"].as_str()
            && request.target.id == custody["binding"]["application_ref"]["resource_id"]
            && request.target.expected_resource_version
                == custody["binding"]["application_ref"]["version"]
            && custody["original_checkpoint_ref"] == dispatch.reference()
            && custody["binding"] == dispatch.body["binding"]
            && custody["started_at"].as_str().is_some()
            && custody["started_at"].as_str() == started(&dispatch.body["observation"]),
        "RecoveryRequired",
        "OCI cleanup does not bind the witnessed original stop and evidence",
    )?;
    Ok(original)
}

fn active_phase(conn: &Connection, op: &Doc, task: &Doc, phase: &str) -> Result<()> {
    let knowledge = if phase == "Dispatched" {
        "InFlight"
    } else {
        "Unknown"
    };
    require(
        op.spec["task_id"] == task.id
            && task.body["operation_refs"] == json!([op.reference()])
            && task.owner == op.owner
            && task.body["outcome_knowledge"] == knowledge
            && op.body["outcome_knowledge"] == knowledge,
        "RecoveryRequired",
        "active OCI stop operation and queue state disagree",
    )?;
    let queue_phase: Option<String> = conn
        .query_row(
            "SELECT phase FROM queue WHERE task=?1 AND operation=?2",
            params![task.id, op.id],
            |r| r.get(0),
        )
        .optional()?;
    require(
        queue_phase.as_deref() == Some(phase),
        "RecoveryRequired",
        "active OCI stop queue row is missing",
    )
}

fn active_binding(conn: &Connection, binding: &Value, original: &Doc) -> Result<()> {
    let app = resolve(conn, &binding["application_ref"])?;
    let volume = if binding["volume_ref"].is_null() {
        None
    } else {
        let v = resolve(conn, &binding["volume_ref"])?;
        require(
            needed(conn, &v.id)?.reference() == v.reference(),
            "RecoveryRequired",
            "OCI stop holds an obsolete volume revision",
        )?;
        Some(v)
    };
    require(
        needed(conn, &app.id)?.reference() == app.reference()
            && original_app_binding(conn, &app, original, volume.as_ref())? == *binding,
        "RecoveryRequired",
        "active OCI stop custody disagrees with its application or volume hold",
    )
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    let epoch = epoch(conn)?;
    evidence::verify_pins(conn)?;
    let mut statement = conn.prepare(
        "SELECT q.task,q.operation,q.phase FROM queue q JOIN current t ON q.task=t.id
         WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type' IN ('application.stop',?1)",
    )?;
    let mut cleanups = 0i64;
    for row in statement.query_map([TOOL], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (task, op, phase) = row?;
        let task = needed(conn, &task)?;
        let op = needed(conn, &op)?;
        active_phase(conn, &op, &task, &phase)?;
        let Some(journal) = intent(conn, &op, epoch)? else {
            continue;
        };
        if task.spec["task_type"] == TOOL {
            let original = check_link(conn, &journal, &task, epoch)?;
            require(
                needed(conn, &original.id)?.reference() == original.reference()
                    && active_cleanup(conn, &original.id)?.as_deref() == Some(op.id.as_str()),
                "RecoveryRequired",
                "active OCI removal continuation has a missing or mismatched latch",
            )?;
            let original_task =
                needed(conn, original.spec["task_id"].as_str().unwrap_or_default())?;
            active_phase(conn, &original, &original_task, "Unknown")?;
            active_binding(conn, &journal.doc.body["custody"]["binding"], &original)?;
            cleanups += 1;
        } else {
            active_binding(conn, &journal.doc.body["binding"], &op)?;
        }
    }
    let latches: i64 =
        conn.query_row("SELECT count(*) FROM oci_stop_cleanups", [], |r| r.get(0))?;
    require(
        latches == cleanups,
        "RecoveryRequired",
        "OCI stop cleanup has an orphaned latch",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_active(conn)?;
    evidence::verify_history(conn)?;
    let epoch = epoch(conn)?;
    let mut statement = conn.prepare(
        "SELECT body->>'operation_id' FROM versions WHERE kind='Effect' AND rev=1 AND seq>?1
          AND body->>'action' IN ('application.stop',?2)",
    )?;
    let mut markers = 0i64;
    for operation in statement.query_map(params![epoch, TOOL], |r| r.get::<_, String>(0))? {
        let op = needed(conn, &operation?)?;
        let journal = intent(conn, &op, epoch)?
            .ok_or_else(|| err("RecoveryRequired", "witnessed OCI stop intent is missing"))?;
        let task = needed(conn, op.spec["task_id"].as_str().unwrap_or_default())?;
        if task.spec["task_type"] == TOOL {
            check_link(conn, &journal, &task, epoch)?;
        }
        markers += 1 + i64::from(journal.dispatch.is_some()) + i64::from(journal.removal.is_some());
    }
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM current WHERE kind IN (?1,?2,?3,?4)",
        params![INTENT, STOP, REMOVE, CLEANUP],
        |r| r.get(0),
    )?;
    require(
        count == markers,
        "RecoveryRequired",
        "OCI stop journal contains unbound evidence",
    )
}
