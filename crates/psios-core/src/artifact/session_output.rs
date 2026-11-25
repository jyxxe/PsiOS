//! Publish exact bytes from a completed model turn under an independently scoped Task.
use super::ingress;
use crate::{
    Result,
    db::{Db, Doc, find, resolve, row},
    err,
    host::Work,
    policy, require,
};
use psios_api::{ResourceRef, canonical, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::apps::inventory::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("session_output.sql");
pub(crate) const TOOL: &str = "artifact.publish_session_summary";
const SOURCE: &str = "LocalSessionOutput";
const MAX_SUMMARY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    session_ref: ResourceRef,
    media_type: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            ingress::valid_media_type(&self.media_type),
            "InvalidArgument",
            "invalid declared media type",
        )
    }
}

struct Output {
    lease_id: String,
    decision_digest: String,
    provider: Value,
}

fn summary(session: &Doc) -> Result<&str> {
    session.body["summary"]
        .as_str()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "the session has no final model summary",
            )
        })
}

fn output(conn: &Connection, session: &Doc) -> Result<Output> {
    require(
        session.kind == "Session" && session.body["lifecycle"] == "Succeeded",
        "PreconditionFailed",
        "artifact publication requires a successful final session revision",
    )?;
    let text = summary(session)?;
    require(
        text.len() <= MAX_SUMMARY_BYTES,
        "QuotaExceeded",
        "final model summary exceeds 64 KiB",
    )?;
    let turn = session.body["turn"]
        .as_i64()
        .filter(|turn| *turn > 0)
        .ok_or_else(|| err("RecoveryRequired", "final session has no model turn"))?;
    let (lease, digest, receipt): (String, String, Value) = conn
        .query_row(
            "SELECT lease,decision_digest,receipt FROM session_turns
         WHERE session=?1 AND turn=?2 AND task IS NULL AND receipt IS NOT NULL",
            params![session.id, turn],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "final model decision receipt is unavailable",
            )
        })?;
    require(
        receipt["session_ref"] == session.reference()
            && receipt["lifecycle"] == "Succeeded"
            && receipt["decision_digest"] == digest
            && sha256(&canonical(&json!({"action":"finish","summary":text}))) == digest,
        "RecoveryRequired",
        "final summary disagrees with its authenticated model decision",
    )?;
    let model = resolve(conn, &session.spec["model_ref"])?;
    require(
        model.kind == "Model" && model.spec["provider_principal_id"].is_string(),
        "RecoveryRequired",
        "final model publisher identity is unavailable",
    )?;
    Ok(Output {
        lease_id: lease,
        decision_digest: digest,
        provider: model.spec["provider_principal_id"].clone(),
    })
}

pub(crate) fn source(db: &Db<'_>, actor: &str, request: &Request) -> Result<Doc> {
    ingress::authorize(db, actor, ingress::UPLOAD)?;
    policy::can(db, actor, "task.read")?;
    let session = db.resolve(&json!(request.session_ref))?;
    policy::owned(actor, &session)?;
    output(db.c, &session)?;
    Ok(session)
}

pub(crate) fn publish(db: &Db<'_>, work: &Work, request: &Request) -> Result<Value> {
    let session = db.resolve(&json!(request.session_ref))?;
    let proof = output(db.c, &session)?;
    let provenance = json!({"kind":SOURCE,"session_ref":session.reference(),
        "origin_task_ref":session.spec["origin_task_ref"],"model_ref":session.spec["model_ref"],
        "skill_ref":session.spec["skill_ref"],"provider_principal_id":proof.provider,
        "model_lease_id":proof.lease_id,"decision_digest":proof.decision_digest,
        "publication_task_ref":work.task.reference(),"publication_run_ref":work.run.reference(),
        "publication_operation_id":work.op.id});
    let accepted = ingress::publish_generated(
        db,
        &work.task.owner,
        summary(&session)?.as_bytes(),
        &request.media_type,
        provenance,
    )?;
    Ok(
        json!({"artifact_ref":accepted["artifact_ref"],"session_ref":session.reference(),
        "operation_id":work.op.id,"decision_digest":proof.decision_digest}),
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let inconsistent: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_uploads u JOIN versions a ON a.id=u.artifact_id AND a.rev=1
          WHERE a.spec->'source'->>'kind' IS NOT u.source_kind)", [], |r|r.get(0),
    )?;
    require(
        !inconsistent,
        "RecoveryRequired",
        "artifact provenance kind disagrees with its publication",
    )?;
    let mut generated = conn.prepare(
        "SELECT * FROM versions WHERE kind='Artifact' AND rev=1 AND spec->'source'->>'kind'=?1",
    )?;
    for row in generated.query_map([SOURCE], row)? {
        let artifact = row?;
        let source = &artifact.spec["source"];
        let session = resolve(conn, &source["session_ref"])?;
        let evidence = output(conn, &session)?;
        let task = resolve(conn, &source["publication_task_ref"])?;
        let run = resolve(conn, &source["publication_run_ref"])?;
        let input = resolve(conn, &task.spec["input_ref"])?;
        let requested: Request = crate::decode(crate::broker::dependencies::tool_input(&input)?)?;
        let operation_id = source["publication_operation_id"].as_str().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "published session output has no creating operation",
            )
        })?;
        let operation = find(conn, operation_id)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "session publication operation is missing",
            )
        })?;
        let plan = resolve(conn, &operation.body["plan_ref"])?;
        let text = summary(&session)?;
        require(
            super::is_raw(&artifact)
                && artifact.owner == source["principal_id"]
                && (artifact.owner == session.owner || artifact.owner == policy::OWNER)
                && artifact.spec["digest"] == sha256(text.as_bytes())
                && artifact.spec["size_bytes"]
                    .as_str()
                    .and_then(|size| size.parse::<usize>().ok())
                    == Some(text.len())
                && artifact.spec["declared_media_type"] == requested.media_type
                && json!(requested.session_ref) == session.reference()
                && source["model_ref"] == session.spec["model_ref"]
                && source["skill_ref"] == session.spec["skill_ref"]
                && source["origin_task_ref"] == session.spec["origin_task_ref"]
                && source["provider_principal_id"] == evidence.provider
                && source["model_lease_id"] == evidence.lease_id
                && source["decision_digest"] == evidence.decision_digest
                && task.kind == "Task"
                && task.owner == artifact.owner
                && task.spec["task_type"] == TOOL
                && run.kind == "Run"
                && run.owner == task.owner
                && run.body["task_id"] == task.id
                && task.body["current_run_id"] == run.id
                && operation.kind == "Operation"
                && operation.owner == task.owner
                && operation.body["lifecycle"] == "Succeeded"
                && operation.body["outcome_knowledge"] == "Known"
                && operation.spec["task_id"] == task.id
                && plan.kind == "OperationPlan"
                && plan.spec["action"] == TOOL
                && plan.body["preconditions"][1]["resource_ref"] == session.reference(),
            "RecoveryRequired",
            "session output publication provenance is inconsistent",
        )?;
    }
    Ok(())
}
