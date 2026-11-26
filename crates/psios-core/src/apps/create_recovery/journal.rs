use super::{CREATE, EPOCH, INTENT, cid_ok, held, validate_binding};
use crate::{
    Result,
    db::{self, Db, Doc},
    err,
    host::Work,
    now, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

fn intent_id(operation: &str) -> String {
    format!("oci-create-intent-{operation}")
}

pub(super) fn start_id(operation: &str) -> String {
    format!("oci-start-{operation}")
}

pub(super) struct Journal {
    pub intent: Doc,
    pub app: Doc,
    pub volume: Option<Doc>,
    pub issue: Option<Doc>,
    pub start: Option<Doc>,
}

pub(crate) fn begin(db: &Db<'_>, w: &Work) -> Result<()> {
    require(
        rustix::process::geteuid().as_raw() != 0,
        "UnsupportedCapability",
        "OCI profile requires non-root service identity",
    )?;
    held(db.c, &w.op, &w.target, w.volume.as_ref())?;
    let permit = db.resolve(&w.effect.spec["permit_ref"])?;
    let decision = db.resolve(&permit.body["decision_ref"])?;
    let approval_use = db.of(&format!("use-{}", decision.id), "ApprovalUse")?;
    require(
        w.effect.body["phase"] == "PermitBound" && approval_use.body["operation_id"] == w.op.id,
        "Fenced",
        "OCI create has no current bound approval",
    )?;
    db.create(
        INTENT, &intent_id(&w.op.id), &w.op.owner, json!({"operation_id":w.op.id}),
        json!({
            "phase":"Reserved","task_ref":w.task.reference(),"run_ref":w.run.reference(),
            "operation_ref":w.op.reference(),"effect_ref":w.effect.reference(),
            "permit_ref":permit.reference(),"approval_use_ref":approval_use.reference(),
            "plan_ref":w.op.body["plan_ref"],"application_ref":w.target.reference(),
            "deployment_ref":w.target.spec["deployment_ref"],"release_ref":w.target.spec["release_ref"],
            "volume_ref":w.volume.as_ref().map(Doc::reference),
            "volume_data_incarnation":w.volume.as_ref().map(|volume| &volume.body["data_incarnation"]),
            "volume_device":w.volume.as_ref().map(|volume| &volume.spec["device"]),
            "volume_inode":w.volume.as_ref().map(|volume| &volume.spec["inode"]),
            "user":format!("{}:{}",rustix::process::geteuid().as_raw(),rustix::process::getegid().as_raw()),
        }),
    )?;
    Ok(())
}

pub(crate) fn issue(db: &Db<'_>, w: &Work, engine: &str, attempt: &str, boot: &str) -> Result<()> {
    let intent = db.of(&intent_id(&w.op.id), INTENT)?;
    require(
        intent.body["phase"] == "Reserved"
            && intent.body["operation_ref"] == w.op.reference()
            && engine_valid(engine)
            && cid_ok(attempt)
            && boot_valid(boot)
            && boot == crate::apps::restart::boot_id()?
            && db.of(&w.effect.id, "Effect")?.reference() == w.effect.reference()
            && db.of(&w.op.id, "Operation")?.reference() == w.op.reference()
            && db.of(&w.run.id, "Run")?.reference() == w.run.reference()
            && db.of(&w.task.id, "Task")?.reference() == w.task.reference(),
        "Fenced",
        "OCI create execution is no longer bound to the original permit",
    )?;
    held_work(db, w)?;
    let mut e = w.effect.body.clone();
    e["phase"] = json!("EffectStarted");
    let effect = db.status(&w.effect, e)?;
    let mut o = w.op.body.clone();
    o["effect_refs"] = json!([effect.reference()]);
    o["updated_at"] = json!(now());
    let op = db.status(&w.op, o)?;
    for subject in [&w.run, &w.task] {
        let mut state = subject.body.clone();
        state["operation_refs"] = json!([op.reference()]);
        db.status(subject, state)?;
    }
    let mut state = intent.body.clone();
    state["phase"] = json!("CreateIssued");
    state["engine_id"] = json!(engine);
    state["boot_id"] = json!(boot);
    state["attempt"] = json!(attempt);
    state["started_effect_ref"] = effect.reference();
    state["started_operation_ref"] = op.reference();
    db.status(&intent, state)?;
    Ok(())
}

pub(crate) fn acknowledge(db: &Db<'_>, w: &Work, cid: &str) -> Result<()> {
    let op = db.of(&w.op.id, "Operation")?;
    let task = db.of(&w.task.id, "Task")?;
    let journal = required(db.c, &op)?;
    require(
        journal.intent.body["phase"] == "CreateIssued"
            && journal.intent.body["run_ref"] == w.run.reference()
            && journal.intent.body["started_operation_ref"] == op.reference()
            && task.body["current_run_id"] == w.run.id
            && matches!(
                task.body["lifecycle"].as_str(),
                Some("Running" | "Cancelling")
            )
            && task.body["outcome_knowledge"] == "InFlight"
            && journal.start.is_none()
            && cid_ok(cid),
        "Fenced",
        "OCI create acknowledgement does not match the in-flight operation",
    )?;
    held(db.c, &op, &journal.app, journal.volume.as_ref())?;
    let mut state = journal.intent.body.clone();
    state["phase"] = json!("CreateAcknowledged");
    state["issue_ref"] = journal.intent.reference();
    state["container_id"] = json!(cid);
    db.status(&journal.intent, state)?;
    Ok(())
}

pub(crate) fn allow_start(db: &Db<'_>, w: &Work, engine: &str, cid: &str) -> Result<Doc> {
    let op = db.of(&w.op.id, "Operation")?;
    let journal = required(db.c, &op)?;
    require(
        journal.intent.body["phase"] == "CreateAcknowledged"
            && journal.intent.body["engine_id"] == engine
            && journal.intent.body["container_id"] == cid
            && journal.intent.body["boot_id"] == crate::apps::restart::boot_id()?
            && journal.intent.body["started_operation_ref"] == op.reference()
            && journal.intent.body["run_ref"] == w.run.reference()
            && journal.start.is_none(),
        "Fenced",
        "OCI Start requires the exact current CREATE acknowledgement",
    )?;
    held_work(db, w)?;
    Ok(journal.intent)
}

fn held_work(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db.c, &work.op, &work.target, work.volume.as_ref())?;
    require(
        work.volume
            .as_ref()
            .map(|volume| crate::storage::volume_dir(db.cfg, volume))
            .transpose()?
            == work.data,
        "PreconditionFailed",
        "approved OCI volume identity changed",
    )
}

fn engine_valid(engine: &str) -> bool {
    (16..=128).contains(&engine.len()) && engine.bytes().all(|byte| byte.is_ascii_graphic())
}

fn boot_valid(boot: &str) -> bool {
    uuid::Uuid::parse_str(boot).is_ok_and(|id| !id.is_nil() && id.to_string() == boot)
}

pub(super) fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [EPOCH], |row| {
        row.get::<_, String>(0)
    })?
    .parse::<i64>()
    .ok()
    .filter(|value| *value > 0)
    .ok_or_else(|| err("RecoveryRequired", "OCI create custody epoch is invalid"))
}

pub(super) fn modern(conn: &Connection, op: &Doc) -> Result<bool> {
    let Some(effect) = op.body["effect_refs"][0]["resource_id"].as_str() else {
        return Ok(false);
    };
    let seq = conn
        .query_row(
            "SELECT seq FROM versions WHERE id=?1 AND rev=1 AND kind='Effect'",
            [effect],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    require(
        seq.is_some(),
        "RecoveryRequired",
        "original OCI create effect is missing",
    )?;
    Ok(seq.unwrap() > epoch(conn)?)
}

pub(super) fn required(conn: &Connection, op: &Doc) -> Result<Journal> {
    load(conn, op)?.ok_or_else(|| {
        err(
            "UnsupportedCapability",
            "legacy OCI Start has no durable create intent",
        )
    })
}

pub(super) fn load(conn: &Connection, op: &Doc) -> Result<Option<Journal>> {
    let found = db::find(conn, &intent_id(&op.id))?;
    if !modern(conn, op)? {
        require(
            found.is_none(),
            "RecoveryRequired",
            "legacy OCI create cannot have modern custody",
        )?;
        return Ok(None);
    }
    let intent = found.ok_or_else(|| err("RecoveryRequired", "OCI create intent is missing"))?;
    let reserved = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&intent.id],
        db::row,
    )?;
    let b = &reserved.body;
    let original = db::resolve(conn, &b["operation_ref"])?;
    let task = db::resolve(conn, &b["task_ref"])?;
    let run = db::resolve(conn, &b["run_ref"])?;
    let effect = db::resolve(conn, &b["effect_ref"])?;
    let permit = db::resolve(conn, &b["permit_ref"])?;
    let use_record = db::resolve(conn, &b["approval_use_ref"])?;
    let decision = db::resolve(conn, &permit.body["decision_ref"])?;
    let plan = db::resolve(conn, &b["plan_ref"])?;
    let app = db::resolve(conn, &b["application_ref"])?;
    let volume = (!b["volume_ref"].is_null())
        .then(|| db::resolve(conn, &b["volume_ref"]))
        .transpose()?;
    let invalid = "OCI create intent does not match its historical authorization";
    require(
        reserved.kind == INTENT
            && intent.kind == INTENT
            && reserved.spec == json!({"operation_id":op.id})
            && reserved.owner == op.owner
            && intent.owner == op.owner
            && intent.spec == reserved.spec
            && (1..=3).contains(&intent.rev)
            && (reserved.generation, intent.generation) == (1, 1)
            && original.kind == "Operation"
            && original.id == op.id
            && original.spec == op.spec
            && task.kind == "Task"
            && task.spec["task_type"] == CREATE
            && task.id == op.spec["task_id"]
            && task.owner == op.owner
            && task.body["operation_refs"][0] == original.reference()
            && run.kind == "Run"
            && run.id == task.body["current_run_id"]
            && run.body["operation_refs"][0] == original.reference()
            && effect.kind == "Effect"
            && effect.body["action"] == CREATE
            && effect.body["phase"] == "PermitBound"
            && original.body["effect_refs"][0] == effect.reference()
            && effect.body["operation_id"] == op.id
            && effect.spec["permit_ref"] == permit.reference()
            && b["phase"] == "Reserved"
            && op.body["plan_ref"] == plan.reference()
            && original.body["plan_ref"] == plan.reference()
            && permit.kind == "ExecutionPermit"
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && use_record.kind == "ApprovalUse"
            && use_record.id == format!("use-{}", decision.id)
            && use_record.body["operation_id"] == op.id
            && use_record.body["validity_status"] == "Valid"
            && decision.kind == "ApprovalDecision"
            && decision.body["decision"] == "Approved"
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == CREATE
            && plan.body["allowed_effects"][0]["resource_ref"] == app.reference()
            && permit.body["audience_resource_refs"]
                == json!(
                    std::iter::once(app.reference())
                        .chain(volume.as_ref().map(Doc::reference))
                        .collect::<Vec<_>>()
                )
            && validate_binding(b, &app, volume.as_ref()),
        "RecoveryRequired",
        invalid,
    )?;
    let issue =
        if intent.rev > 1 {
            let issued = conn.query_row(
                "SELECT * FROM versions WHERE id=?1 AND rev=2",
                [&intent.id],
                db::row,
            )?;
            let body = &issued.body;
            let started = db::resolve(conn, &body["started_effect_ref"])?;
            let operation = db::resolve(conn, &body["started_operation_ref"])?;
            require(
                issued.kind == INTENT
                    && issued.owner == intent.owner
                    && issued.generation == 1
                    && issued.spec == intent.spec
                    && b.as_object()
                        .unwrap()
                        .iter()
                        .all(|(key, value)| key == "phase" || body.get(key) == Some(value))
                    && body["phase"] == "CreateIssued"
                    && body["engine_id"].as_str().is_some_and(engine_valid)
                    && body["boot_id"].as_str().is_some_and(boot_valid)
                    && body["attempt"].as_str().is_some_and(cid_ok)
                    && started.id == effect.id
                    && started.rev == effect.rev + 1
                    && started.spec == effect.spec
                    && started.body["phase"] == "EffectStarted"
                    && operation.id == op.id
                    && operation.spec == op.spec
                    && operation.rev == original.rev + 1
                    && operation.body["effect_refs"][0] == started.reference(),
                "RecoveryRequired",
                "OCI create dispatch does not match its intent",
            )?;
            if intent.rev == 3 {
                require(
                    intent.body["phase"] == "CreateAcknowledged"
                        && intent.body["issue_ref"] == issued.reference()
                        && intent.body["container_id"].as_str().is_some_and(cid_ok)
                        && body.as_object().unwrap().iter().all(|(key, value)| {
                            key == "phase" || intent.body.get(key) == Some(value)
                        }),
                    "RecoveryRequired",
                    "OCI create acknowledgement does not match its dispatch",
                )?;
            }
            Some(issued)
        } else {
            None
        };
    let start = db::find(conn, &start_id(&op.id))?;
    if let Some(checkpoint) = &start {
        require(
            intent.rev == 3
                && checkpoint.kind == "OCIStartCheckpoint"
                && checkpoint.rev == 1
                && checkpoint.owner == op.owner
                && checkpoint.body["create_ack_ref"] == intent.reference()
                && checkpoint.body["application_ref"] == app.reference()
                && checkpoint.body["volume_ref"] == intent.body["volume_ref"]
                && checkpoint.body["effect_ref"] == intent.body["started_effect_ref"]
                && checkpoint.body["engine_id"] == intent.body["engine_id"]
                && checkpoint.body["container_id"] == intent.body["container_id"]
                && checkpoint.body["start_operation_id"] == op.id,
            "RecoveryRequired",
            "OCI Start checkpoint bypassed its CREATE acknowledgement",
        )?;
    }
    Ok(Some(Journal {
        intent,
        app,
        volume,
        issue,
        start,
    }))
}

pub(super) fn read_all(conn: &Connection, active: bool) -> Result<()> {
    let query = if active {
        "SELECT o.* FROM queue q JOIN current t ON t.id=q.task JOIN current o ON o.id=q.operation
         WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type'=?1"
    } else {
        "SELECT o.* FROM versions e JOIN current o ON o.id=e.body->>'operation_id'
         WHERE e.kind='Effect' AND e.rev=1 AND e.body->>'action'=?1"
    };
    let mut statement = conn.prepare(query)?;
    let records = statement.query_map(params![CREATE], db::row)?;
    for op in records {
        let op = op?;
        if let Some(journal) = load(conn, &op)?
            && (active || op.body["outcome_knowledge"] == "Unknown")
        {
            held(conn, &op, &journal.app, journal.volume.as_ref())?;
        }
    }
    Ok(())
}
