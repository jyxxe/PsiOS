//! Application log capture and custody under the application.logs capability.
use super::*;
use crate::db::row;

pub(crate) const TOOL: &str = "application.logs";
pub(crate) const PRIVATE_KIND: &str = "ApplicationLogTail";
const STREAM_BYTES: usize = 8192;

fn tail_id(operation: &str) -> String {
    format!("application-log-tail-{operation}")
}

pub(crate) fn redact_status(kind: &str, body: &mut Value) {
    let slot = match kind {
        "Application" => "last_observation",
        "RuntimeInstance" => "observation",
        _ => return,
    };
    if let Some(fields) = body.as_object_mut() {
        fields.remove("last_logs");
        if let Some(observation) = fields.get_mut(slot).and_then(Value::as_object_mut) {
            observation.remove("last_logs");
        }
    }
}

pub(crate) fn redact_late_receipt(db: &Db<'_>, doc: &mut Doc) -> Result<()> {
    let receipt = &doc.body["evidence"]["receipt"];
    if doc.kind == "LateReceipt"
        && (receipt.get("last_logs").is_some() || receipt.get("stdout").is_some())
    {
        let effect = db.resolve(&doc.body["effect_ref"])?;
        if matches!(
            effect.body["action"].as_str(),
            Some(TOOL | "application.stop")
        ) {
            doc.body["evidence"] = json!({"code":"Fenced"});
            doc.spec["evidence"] = doc.body["evidence"].clone();
        }
    }
    Ok(())
}

pub(crate) fn authorize_task(db: &Db<'_>, actor: &str, task: &Doc) -> Result<()> {
    if task.kind != "Task" || task.spec["task_type"] != TOOL {
        return Ok(());
    }
    let operation = db.resolve(&task.body["operation_refs"][0])?;
    let plan = db.resolve(&operation.body["plan_ref"])?;
    let app = db.resolve(&plan.body["allowed_effects"][0]["resource_ref"])?;
    require(
        operation.kind == "Operation"
            && operation.spec["task_id"] == task.id
            && plan.kind == "OperationPlan"
            && plan.body["allowed_effects"][0]["action"] == TOOL
            && app.kind == "Application",
        "RecoveryRequired",
        "application log result has no bound authorizing plan",
    )?;
    policy::owned(actor, &app)?;
    policy::can_target(db, actor, TOOL, &app.id)
}

pub(crate) fn authorize_artifact(db: &Db<'_>, actor: &str, artifact: &Doc) -> Result<()> {
    if artifact.kind == "Artifact"
        && let Some(task) = artifact.spec["task_id"].as_str()
    {
        authorize_task(db, actor, &db.of(task, "Task")?)?;
    }
    Ok(())
}

fn source(app: &Doc) -> Result<Value> {
    let container = app.body["container_id"].as_str();
    require(
        app.kind == "Application"
            && app.body["lifecycle"] == "Running"
            && container.is_some_and(|id| {
                id.len() == 64
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            && app.body["engine_id"]
                .as_str()
                .is_some_and(capture::valid_engine_id)
            && app.body["start_operation_id"].is_string()
            && app.body["instance_ref"].is_object(),
        "RecoveryRequired",
        "application has no bound OCI log source",
    )?;
    Ok(
        json!({"application_ref":app.reference(),"instance_ref":app.body["instance_ref"],
        "container_id":app.body["container_id"],"engine_id":app.body["engine_id"],
        "start_operation_id":app.body["start_operation_id"]}),
    )
}

pub(super) fn check_live(db: &Db<'_>, app: &Doc) -> Result<()> {
    let source = source(app)?;
    let instance = db.resolve(&source["instance_ref"])?;
    let start = source["start_operation_id"].as_str().unwrap();
    let operation = db.of(start, "Operation")?;
    let plan = db.resolve(&operation.body["plan_ref"])?;
    let effect = db.resolve(&operation.body["effect_refs"][0])?;
    let approved = &plan.body["allowed_effects"][0];
    let target = db.resolve(&approved["resource_ref"])?;
    require(
        instance.kind == "RuntimeInstance"
            && instance.owner == app.owner
            && instance.spec["application_id"] == app.id
            && instance.spec["deployment_ref"] == app.spec["deployment_ref"]
            && ["container_id", "engine_id", "start_operation_id"]
                .into_iter()
                .all(|field| instance.spec[field] == source[field])
            && operation.body["outcome_knowledge"] == "Known"
            && matches!(
                operation.body["lifecycle"].as_str(),
                Some("Succeeded" | "Cancelled")
            )
            && plan.kind == "OperationPlan"
            && approved["action"] == "application.start"
            && effect.kind == "Effect"
            && effect.owner == operation.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["action"] == "application.start"
            && effect.body["resource_ref"] == approved["resource_ref"]
            && target.kind == "Application"
            && target.owner == app.owner
            && target.id == app.id,
        "RecoveryRequired",
        "OCI log source disagrees with the original start operation",
    )?;
    if let Some(checkpoint) = db.find(&format!("oci-start-{start}"))? {
        require(
            checkpoint.kind == "OCIStartCheckpoint"
                && checkpoint.body["application_ref"] == approved["resource_ref"]
                && ["container_id", "engine_id", "start_operation_id"]
                    .into_iter()
                    .all(|field| checkpoint.body[field] == source[field]),
            "RecoveryRequired",
            "OCI log source differs from the original start checkpoint",
        )?;
    }
    Ok(())
}

fn bounded(b: &[u8]) -> (String, bool) {
    let len = b.len();
    let lossy = String::from_utf8_lossy(&b[len.saturating_sub(STREAM_BYTES)..]);
    let start = (lossy.len().saturating_sub(STREAM_BYTES)..=lossy.len())
        .find(|position| lossy.is_char_boundary(*position))
        .unwrap_or(lossy.len());
    (lossy[start..].to_owned(), len > STREAM_BYTES || start > 0)
}

pub(super) fn capture(work: &Work, engine: &Engine<'_>) -> Result<Value> {
    let source = source(&work.target)?;
    let container = source["container_id"].as_str().unwrap();
    let start = source["start_operation_id"].as_str().unwrap();
    engine.managed(work, container, start)?;
    let output = crate::command::capture_tail(
        engine.command_line(&["logs", "--tail", "64", container].map(String::from)),
        "Docker",
        20,
        STREAM_BYTES,
    )?;
    engine.managed(work, container, start)?;
    require(
        Some(engine.identity()?.as_str()) == source["engine_id"].as_str(),
        "PreconditionFailed",
        "OCI engine changed during log capture",
    )?;
    let (stdout, stdout_cut) = bounded(&output.stdout);
    let (stderr, stderr_cut) = bounded(&output.stderr);
    Ok(
        json!({"availability":"Available","stdout":stdout,"stderr":stderr,
        "truncated":output.truncated || stdout_cut || stderr_cut,
        "source":source,"captured_at":now()}),
    )
}

fn valid_bytes(tail: &Value, stream_bytes: usize) -> bool {
    tail["truncated"].is_boolean()
        && ["stdout", "stderr"].into_iter().all(|field| {
            tail[field]
                .as_str()
                .is_some_and(|stream| stream.len() <= stream_bytes)
        })
}

pub(super) fn retain(db: &Db<'_>, app: &Doc, operation: &str, tail: &Value) -> Result<()> {
    check_live(db, app)?;
    require(
        tail["availability"] == "Available"
            && tail["source"] == source(app)?
            && tail["captured_at"].is_string()
            && valid_bytes(tail, STREAM_BYTES),
        "RecoveryRequired",
        "stopped application log tail disagrees with the admitted instance",
    )?;
    db.create(
        PRIVATE_KIND,
        &tail_id(operation),
        &app.owner,
        json!({"application_id":app.id,"stop_operation_id":operation}),
        tail.clone(),
    )?;
    Ok(())
}

pub(crate) fn retained(db: &Db<'_>, app: &Doc) -> Result<Value> {
    let unavailable =
        || json!({"availability":"Unavailable","stdout":"","stderr":"","truncated":false});
    if let Some(operation) = app.body["last_stop_operation_id"].as_str() {
        let Some(record) = db.find(&tail_id(operation))? else {
            return Ok(unavailable());
        };
        let original = db.resolve(&record.body["source"]["application_ref"])?;
        let stop = db.of(operation, "Operation")?;
        let plan = db.resolve(&stop.body["plan_ref"])?;
        require(
            record.kind == PRIVATE_KIND
                && record.owner == app.owner
                && record.spec["application_id"] == app.id
                && record.spec["stop_operation_id"] == operation
                && original.id == app.id
                && original.owner == app.owner
                && record.body["source"] == source(&original)?
                && stop.body["outcome_knowledge"] == "Known"
                && matches!(
                    stop.body["lifecycle"].as_str(),
                    Some("Succeeded" | "Cancelled")
                )
                && plan.kind == "OperationPlan"
                && plan.body["allowed_effects"][0]["action"] == "application.stop"
                && plan.body["allowed_effects"][0]["resource_ref"] == original.reference()
                && record.body["availability"] == "Available"
                && record.body["captured_at"].is_string()
                && valid_bytes(&record.body, STREAM_BYTES),
            "RecoveryRequired",
            "retained log tail does not match its original stop operation",
        )?;
        check_live(db, &original)?;
        return Ok(record.body);
    }
    let prior = db.c.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev<=?2 AND kind='Application' AND owner=?3
         AND body->>'lifecycle'='Stopped' AND json_type(body,'$.last_logs')='object' ORDER BY rev DESC LIMIT 1",
        params![app.id, app.rev, app.owner], row,
    ).optional()?;
    let Some(prior) = prior else {
        return Ok(unavailable());
    };
    let tail = &prior.body["last_logs"];
    if !valid_bytes(tail, 3 * STREAM_BYTES) {
        return Ok(unavailable());
    }
    let (stdout, stdout_cut) = bounded(tail["stdout"].as_str().unwrap().as_bytes());
    let (stderr, stderr_cut) = bounded(tail["stderr"].as_str().unwrap().as_bytes());
    Ok(
        json!({"availability":"Available","stdout":stdout,"stderr":stderr,
        "truncated":tail["truncated"] == true || stdout_cut || stderr_cut,
        "source":{"application_ref":prior.reference(),"layout":"LegacyStoppedStatus"}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use psios_api::Request;

    #[test]
    fn legacy_status_is_redacted_without_changing_exact_references() {
        let directory = tempfile::tempdir().unwrap();
        let config = Config::local(&directory.path().join("core"));
        Store::init(&config).unwrap();
        let mut store = Store::open(config.clone()).unwrap();
        let (legacy, private, late) = store.transaction("legacy-tail-fixture", |db| {
            let app = db.create("Application", "legacy-app", policy::OWNER, json!({}),
                json!({"lifecycle":"Stopped"}))?;
            let tail = json!({"stdout":"private fixture","stderr":"private error","truncated":false});
            let old = json!({"lifecycle":"Stopped","last_logs":tail,"last_observation":{"last_logs":tail}});
            db.c.execute("UPDATE versions SET body=?1 WHERE id=?2 AND rev=1", params![old, app.id])?;
            let legacy = db.get(&app.id)?;
            let loaded = retained(db, &legacy)?;
            assert_eq!(loaded["stdout"], "private fixture");
            assert_eq!(loaded["source"]["application_ref"], legacy.reference());
            let private = db.create(PRIVATE_KIND, "private-tail", policy::OWNER, json!({}), tail.clone())?;
            let effect = db.create("Effect", "effect-fixture", policy::OWNER, json!({}), json!({
                "schema_version":VERSION,"effect_id":"effect-fixture","operation_id":"operation-fixture",
                "resource_ref":legacy.reference(),"action":TOOL,"phase":"EffectStarted",
                "outcome_knowledge":"InFlight","evidence_refs":[]
            }))?;
            let late = db.record("LateReceipt", policy::OWNER, json!({"effect_ref":effect.reference(),
                "evidence":{"receipt":tail}}))?;
            Ok((legacy.reference(), private.reference(), late.reference()))
        }).unwrap();
        let token = std::fs::read_to_string(config.witness_dir.join("owner.token")).unwrap();
        let mut call = |method: &str, params| {
            store.request(
                rustix::process::geteuid().as_raw(),
                Request {
                    schema_version: VERSION.to_owned(),
                    request_id: id("fixture"),
                    token: token.clone(),
                    method: method.to_owned(),
                    params,
                },
            )
        };
        for visible in [
            call("Resource.Get", json!({"id":"legacy-app"})).unwrap(),
            call("Resource.Resolve", json!({"reference":legacy})).unwrap(),
        ] {
            assert_eq!(visible["reference"], legacy);
            assert!(!visible.to_string().contains("private fixture"));
            assert!(visible["value"].get("last_logs").is_none());
        }
        assert_eq!(
            call("Resource.Resolve", json!({"reference":private}))
                .unwrap_err()
                .0
                .code,
            "PermissionDenied"
        );
        assert_eq!(
            call("Resource.List", json!({"kind":PRIVATE_KIND})).unwrap()["items"],
            json!([])
        );
        let late_view = call("Resource.Resolve", json!({"reference":late})).unwrap();
        assert_eq!(late_view["reference"], late);
        assert!(!late_view.to_string().contains("private fixture"));
        assert_eq!(late_view["resource"]["spec"]["evidence"]["code"], "Fenced");
    }

    #[test]
    fn tail_stream_retains_the_end_and_limits_lossy_encoded_utf8() {
        let mut bytes = vec![b'x'; STREAM_BYTES];
        bytes.extend_from_slice(b"last line");
        let (bounded_tail, truncated) = bounded(&bytes);
        assert!(
            truncated && bounded_tail.len() == STREAM_BYTES && bounded_tail.ends_with("last line")
        );
        let (substituted, truncated) = bounded(&[0xff; STREAM_BYTES]);
        assert!(
            truncated
                && substituted.len() <= STREAM_BYTES
                && substituted.chars().all(|c| c == '\u{fffd}')
        );
    }
}
