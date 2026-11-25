//! Export bytes from the trusted local Docker Engine without pulling or loading an image.
use super::{Engine, valid_image_id};
use crate::{
    Result, artifact, command,
    db::{Db, Doc, find, resolve},
    decode, err,
    host::Work,
    policy, require,
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

pub(crate) const FORMAT: u32 = super::publisher::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("capture.sql");
pub(crate) const TOOL: &str = "application.capture_oci_image";
const SOURCE: &str = "LocalOciImageCapture";
const PROOF: &str = "OciImageCapture";
const MEDIA: &str = "application/vnd.psios.docker-image.v1+tar";
const ARCHIVE_FORMAT: &str = "DockerImageArchiveV1";

fn valid_engine_id(engine: &str) -> bool {
    (16..=128).contains(&engine.len()) && engine.bytes().all(|b| b.is_ascii_graphic())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    image_id: String,
    expected_engine_id: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            valid_image_id(&self.image_id) && valid_engine_id(&self.expected_engine_id),
            "InvalidArgument",
            "capture requires an exact local OCI image digest and expected engine identity",
        )
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ImageIdentity {
    config_digest: String,
    os: String,
    architecture: String,
    diff_ids: Vec<String>,
}

impl ImageIdentity {
    fn matches(&self, request: &Request) -> bool {
        self.config_digest == request.image_id
            && self.os == "linux"
            && self.architecture == "amd64"
            && (1..=128).contains(&self.diff_ids.len())
            && self.diff_ids.iter().all(|id| valid_image_id(id))
    }
}

pub(crate) struct Captured {
    bytes: Vec<u8>,
    image: ImageIdentity,
    engine: String,
}

pub(crate) fn authorize(db: &Db<'_>, actor: &str) -> Result<()> {
    policy::admin(actor)?;
    artifact::ingress::authorize(db, actor, artifact::ingress::UPLOAD)
}

fn identity(engine: &Engine<'_>, request: &Request) -> Result<ImageIdentity> {
    let inspection = engine.image(&request.image_id)?;
    let identity: ImageIdentity = decode(&json!({
        "config_digest":inspection["config_digest"], "os":inspection["os"],
        "architecture":inspection["architecture"], "diff_ids":inspection["layers"],
    }))?;
    require(
        identity.matches(request),
        "UnsupportedCapability",
        "capture requires a layered local Linux amd64 image",
    )?;
    Ok(identity)
}

pub(crate) fn export(
    work: &Work,
    request: &Request,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<Captured> {
    let engine = Engine(&work.cfg);
    check_authority()?;
    let id = engine.identity()?;
    require(
        id == request.expected_engine_id,
        "PreconditionFailed",
        "local Docker engine identity changed",
    )?;
    let image = identity(&engine, request)?;
    let bytes = command::capture_bytes(
        engine.command_line(&["image".into(), "save".into(), request.image_id.clone()]),
        artifact::ingress::MAX_BYTES as usize,
        Duration::from_secs(20),
        check_authority,
    )?;
    require(
        identity(&engine, request)? == image && engine.identity()? == id,
        "PreconditionFailed",
        "local OCI image or Docker engine changed during capture",
    )?;
    check_authority()?;
    Ok(Captured {
        bytes,
        image,
        engine: id,
    })
}

pub(crate) fn check_capacity(db: &Db<'_>, work: &Work, capture: &Captured) -> Result<()> {
    db.cfg.check_admission_space()?;
    artifact::ingress::check_capacity(db, &work.task.owner, capture.bytes.len() as u32)
}

pub(crate) fn publish(db: &Db<'_>, work: &Work, capture: Captured) -> Result<Value> {
    let owner = &work.task.owner;
    let proof_id = format!("oci-image-capture-{}", work.op.id);
    let source = json!({"kind":SOURCE,"capture_id":proof_id,
        "publication_task_ref":work.task.reference(),"publication_run_ref":work.run.reference(),
        "publication_effect_ref":work.effect.reference(),"publication_operation_id":work.op.id});
    let accepted = artifact::ingress::publish_generated(db, owner, &capture.bytes, MEDIA, source)?;
    let reference = &accepted["artifact_ref"];
    let artifact = db.resolve(reference)?;
    let proof = db.create(
        PROOF, &proof_id, owner,
        json!({"operation_id":work.op.id}),
        json!({"artifact_ref":reference,"archive_format":ARCHIVE_FORMAT,"engine_id":capture.engine,
            "image":capture.image,"archive_digest":artifact.spec["digest"],"size_bytes":artifact.spec["size_bytes"]}),
    )?;
    db.c.execute(
        "INSERT INTO oci_image_captures VALUES(?1,?2)",
        params![artifact.id, proof.id],
    )?;
    db.audit(owner, TOOL, Some(&proof.id), "Published")?;
    Ok(json!({"artifact_ref":reference,"capture_ref":proof.reference(),"operation_id":work.op.id}))
}

fn existing(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "OCI capture evidence is missing"))
}

fn verify_receipt(
    conn: &Connection,
    operation: &Doc,
    started_effect: &Doc,
    archive: &Doc,
    proof: &Doc,
) -> Result<()> {
    let effect_ref = operation.body["effect_refs"]
        .as_array()
        .and_then(|effects| effects.last())
        .ok_or_else(|| err("RecoveryRequired", "OCI capture has no settled effect"))?;
    let settled = resolve(conn, effect_ref)?;
    let result = resolve(conn, &settled.body["evidence_refs"][0])?;
    let body = psios_api::parse(&artifact::checkpoint_bytes(conn, &result, 4096)?)?;
    require(
        settled.id == started_effect.id
            && settled.kind == "Effect"
            && settled.body["phase"] == "EffectConfirmed"
            && settled.body["outcome_knowledge"] == "Known"
            && result.kind == "Artifact"
            && result.owner == archive.owner
            && result.spec["operation_id"] == operation.id
            && result.spec["effect_id"] == started_effect.id
            && body["operation_id"] == operation.id
            && body["artifact_ref"] == archive.reference()
            && body["capture_ref"] == proof.reference(),
        "RecoveryRequired",
        "OCI capture evidence disagrees with the settled receipt",
    )
}

fn verify_one(conn: &Connection, artifact: &Doc, proof: &Doc) -> Result<()> {
    let source = &artifact.spec["source"];
    let task = resolve(conn, &source["publication_task_ref"])?;
    let run = resolve(conn, &source["publication_run_ref"])?;
    let effect = resolve(conn, &source["publication_effect_ref"])?;
    let operation = existing(
        conn,
        proof.spec["operation_id"].as_str().unwrap_or_default(),
    )?;
    let plan = resolve(conn, &operation.body["plan_ref"])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let decision = resolve(conn, &permit.body["decision_ref"])?;
    let used = existing(conn, &format!("use-{}", decision.id))?;
    let input = resolve(conn, &task.spec["input_ref"])?;
    let request: Request = decode(crate::broker::dependencies::tool_input(&input)?)?;
    let image: ImageIdentity = decode(&proof.body["image"])?;
    require(
        artifact::is_raw(artifact)
            && artifact.rev == 1
            && artifact.owner == policy::OWNER
            && artifact.owner == source["principal_id"]
            && source["kind"] == SOURCE
            && artifact.spec["declared_media_type"] == MEDIA
            && source["capture_id"] == proof.id
            && proof.kind == PROOF
            && proof.rev == 1
            && proof.owner == artifact.owner
            && proof.id == format!("oci-image-capture-{}", operation.id)
            && proof.body["archive_format"] == ARCHIVE_FORMAT
            && proof.body["artifact_ref"] == artifact.reference()
            && proof.body["archive_digest"] == artifact.spec["digest"]
            && proof.body["size_bytes"] == artifact.spec["size_bytes"]
            && valid_image_id(artifact.spec["digest"].as_str().unwrap_or_default())
            && proof.body["size_bytes"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|size| (1..=artifact::ingress::MAX_BYTES).contains(&size))
            && proof.body["engine_id"] == request.expected_engine_id
            && valid_engine_id(&request.expected_engine_id)
            && image.matches(&request)
            && valid_image_id(&request.image_id)
            && task.kind == "Task"
            && task.owner == artifact.owner
            && task.spec["task_type"] == TOOL
            && task.body["current_run_id"] == run.id
            && run.kind == "Run"
            && run.owner == artifact.owner
            && run.body["task_id"] == task.id
            && effect.kind == "Effect"
            && effect.owner == artifact.owner
            && effect.body["operation_id"] == operation.id
            && effect.body["action"] == TOOL
            && operation.kind == "Operation"
            && operation.owner == artifact.owner
            && operation.spec["task_id"] == task.id
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && source["publication_operation_id"] == operation.id
            && plan.kind == "OperationPlan"
            && plan.spec["action"] == TOOL
            && plan.body["purpose_ref"] == input.reference()
            && plan.body["input_refs"][0] == input.reference()
            && permit.kind == "ExecutionPermit"
            && permit.body["operation_id"] == operation.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && decision.kind == "ApprovalDecision"
            && decision.body["decision"] == "Approved"
            && decision.body["approver_principal_id"] == policy::OWNER
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && used.kind == "ApprovalUse"
            && used.body["operation_id"] == operation.id
            && used.body["decision_ref"] == decision.reference()
            && used.body["binding_status"] == "Bound",
        "RecoveryRequired",
        "OCI archive provenance disagrees with the approved capture",
    )?;
    verify_receipt(conn, &operation, &effect, artifact, proof)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let inconsistent: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_uploads u WHERE u.source_kind=?1 AND NOT EXISTS(
             SELECT 1 FROM oci_image_captures c WHERE c.artifact=u.artifact_id))
          OR EXISTS(SELECT 1 FROM versions p WHERE p.kind=?2 AND p.rev=1 AND NOT EXISTS(
             SELECT 1 FROM oci_image_captures c WHERE c.proof=p.id))
          OR EXISTS(SELECT 1 FROM oci_image_captures c JOIN artifact_uploads u ON u.artifact_id=c.artifact
             WHERE u.source_kind!=?1)",
        params![SOURCE, PROOF], |row| row.get(0),
    )?;
    require(
        !inconsistent,
        "RecoveryRequired",
        "OCI capture provenance index is inconsistent",
    )?;
    let mut captures = conn.prepare("SELECT artifact,proof FROM oci_image_captures")?;
    for capture in captures.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (artifact, proof) = capture?;
        verify_one(conn, &existing(conn, &artifact)?, &existing(conn, &proof)?)?;
    }
    Ok(())
}
