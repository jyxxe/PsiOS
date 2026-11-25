//! Retain captured application bytes independently of the Docker cache.
use super::{artifact_install, capture, publisher, release_retirement};
use crate::{
    Result, artifact,
    db::{Db, Doc, find, resolve as resource, row},
    decode, err, require,
};
use psios_api::OciPublisherSignature;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = capture::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("release_archive.sql");
// Existing slots are the target, declaration and (for signed declarations) key.
pub(super) const PLAN_POSITION: usize = 3;

pub(super) struct Archive {
    pub artifact: Doc,
    pub proof: Doc,
}

impl Archive {
    pub fn check_content(&self, db: &Db<'_>) -> Result<()> {
        artifact::verify_raw_bytes(
            db.c,
            &db.cfg.state_dir,
            &self.artifact,
            artifact::ingress::MAX_BYTES as usize,
        )
    }

    pub fn check_image(&self, image: &Value) -> Result<()> {
        let captured = &self.proof.body["image"];
        require(
            captured["config_digest"] == image["config_digest"]
                && captured["os"] == image["os"]
                && captured["architecture"] == image["architecture"]
                && captured["diff_ids"] == image["layers"],
            "PreconditionFailed",
            "local Docker image disagrees with its captured archive",
        )
    }
}

pub(super) fn resolve(conn: &Connection, reference: &Value, image: &str) -> Result<Archive> {
    read(conn, resource(conn, reference)?, image)
}

pub(super) fn read(conn: &Connection, artifact: Doc, image: &str) -> Result<Archive> {
    let proof = capture::require_archive(conn, &artifact, image)?;
    Ok(Archive { artifact, proof })
}

pub(super) fn pin_task(db: &Db<'_>, task: &str, reference: &Value) -> Result<()> {
    let archive = db.resolve(reference)?;
    db.c.execute(
        "INSERT INTO application_image_archive_pins VALUES(?1,?2,'Task')",
        params![archive.id, task],
    )?;
    Ok(())
}

pub(super) fn check_pin(
    conn: &Connection,
    kind: &str,
    holder: &str,
    archive: Option<&Archive>,
) -> Result<()> {
    let held: Option<String> = conn
        .query_row(
            "SELECT artifact FROM application_image_archive_pins WHERE kind=?1 AND holder=?2",
            params![kind, holder],
            |r| r.get(0),
        )
        .optional()?;
    require(
        held.as_deref() == archive.map(|archive| archive.artifact.id.as_str()),
        "RecoveryRequired",
        "OCI archive disagrees with its retention pin",
    )?;
    if let Some(archive) = archive {
        artifact::retention::require_content(conn, &archive.artifact)?;
    }
    Ok(())
}

pub(super) fn pin_release(db: &Db<'_>, task: &str, release: &str, artifact: &str) -> Result<()> {
    let changed = db.c.execute(
        "INSERT INTO application_image_archive_pins SELECT artifact,?1,'Release' FROM application_image_archive_pins
         WHERE kind='Task' AND holder=?2 AND artifact=?3", params![release, task, artifact],
    )?;
    require(
        changed == 1,
        "RecoveryRequired",
        "catalog transaction lost its OCI archive pin",
    )
}

pub(super) fn unpin(db: &Db<'_>, kind: &str, holder: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM application_image_archive_pins WHERE kind=?1 AND holder=?2",
        params![kind, holder],
    )?;
    Ok(())
}

fn plan_archive(plan: &Doc) -> Option<&Value> {
    plan.body["preconditions"]
        .as_array()?
        .get(PLAN_POSITION)?
        .get("resource_ref")
}

pub(super) fn recorded(
    conn: &Connection,
    release: &Doc,
    source: Option<&Doc>,
) -> Result<Option<Archive>> {
    let image = resource(conn, &release.body["artifact_ref"])?;
    let evidence = artifact_install::release_evidence(conn, release)?;
    let reference = &image.body["image_archive_ref"];
    let captured = &image.body["image_capture_ref"];
    let Some(source) =
        source.filter(|doc| doc.spec["declared_media_type"] == artifact_install::MEDIA_V2)
    else {
        require(
            reference.is_null()
                && captured.is_null()
                && evidence.as_ref().is_none_or(|e| {
                    e.body["image_archive_ref"].is_null() && e.body["image_capture_ref"].is_null()
                }),
            "RecoveryRequired",
            "OCI archive has no signed archive declaration",
        )?;
        return Ok(None);
    };

    let evidence = evidence
        .ok_or_else(|| err("RecoveryRequired", "OCI archive catalog evidence is absent"))?;
    let operation = find(
        conn,
        evidence.body["operation_id"].as_str().unwrap_or_default(),
    )?
    .ok_or_else(|| err("RecoveryRequired", "OCI archive operation is absent"))?;
    let plan = resource(conn, &operation.body["plan_ref"])?;
    let input = resource(conn, &plan.body["purpose_ref"])?;
    let input = crate::broker::dependencies::tool_input(&input)?;
    let spec = resource(conn, &release.body["release_spec_ref"])?;
    let provenance = &spec.body["provenance"];
    let signed = resource(conn, &provenance["trust_evidence_or_admission_policy_ref"])?;
    let archive = resolve(
        conn,
        reference,
        image.body["config_digest"].as_str().unwrap_or_default(),
    )?;
    require(
        image.kind == "OCIImage"
            && image.owner == release.owner
            && spec.kind == "ApplicationReleaseSpec"
            && spec.owner == release.owner
            && provenance["source_trust"] == "PublisherVerified"
            && signed.kind == "OciPublisherVerification"
            && signed.spec["operation_id"] == operation.id
            && signed.body["artifact_ref"] == source.reference()
            && signed.body["signature"] == input["signature"]
            && provenance["source_refs"]
                .as_array()
                .is_some_and(|sources| sources.contains(reference) && sources.contains(captured))
            && evidence.body["image_archive_ref"] == *reference
            && evidence.body["image_capture_ref"] == *captured
            && captured == &archive.proof.reference()
            && operation.kind == "Operation"
            && operation.body["lifecycle"] == "Succeeded"
            && operation.body["outcome_knowledge"] == "Known"
            && plan.kind == "OperationPlan"
            && matches!(
                plan.spec["action"].as_str(),
                Some(artifact_install::TOOL | artifact_install::STAGE_TOOL)
            )
            && input["reference"] == source.reference()
            && input["signature"].is_object()
            && plan.body["preconditions"][1]["resource_ref"] == source.reference()
            && plan.body["preconditions"][2]["resource_ref"] == input["signature"]["key_ref"]
            && plan_archive(&plan) == Some(reference),
        "RecoveryRequired",
        "OCI archive provenance disagrees with its approved release",
    )?;
    archive.check_image(&image.body["inspection"])?;
    Ok(Some(archive))
}

pub(super) fn check_release_pin(
    conn: &Connection,
    release: &Doc,
    source: Option<&Doc>,
) -> Result<Option<Archive>> {
    let archive = recorded(conn, release, source)?;
    check_pin(conn, "Release", &release.id, archive.as_ref())?;
    Ok(archive)
}

fn verify_pending(conn: &Connection, task: &Doc) -> Result<bool> {
    let op = resource(conn, &task.body["operation_refs"][0])?;
    let plan = resource(conn, &op.body["plan_ref"])?;
    let input = resource(conn, &task.spec["input_ref"])?;
    let input = crate::broker::dependencies::tool_input(&input)?;
    let source = resource(conn, &input["reference"])?;
    let descriptor = artifact_install::read_descriptor(conn, &source)?;
    let archive = if let Some(reference) = descriptor.archive {
        let signature: OciPublisherSignature = decode(&input["signature"])?;
        let (key, _) = publisher::signature(conn, &source, &signature)?;
        let reference = json!(reference);
        require(
            plan_archive(&plan) == Some(&reference)
                && plan.body["preconditions"][2]["resource_ref"] == key.reference(),
            "RecoveryRequired",
            "pending OCI archive disagrees with its signed plan",
        )?;
        Some(resolve(conn, &reference, &descriptor.install.image_id)?)
    } else {
        require(
            plan_archive(&plan).is_none(),
            "RecoveryRequired",
            "legacy OCI plan has unexpected archive",
        )?;
        None
    };
    check_pin(conn, "Task", &task.id, archive.as_ref())?;
    Ok(archive.is_some())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut retained = 0i64;
    let mut pending = conn.prepare(
        "SELECT t.* FROM queue q JOIN current t ON t.id=q.task
        WHERE q.phase<>'Done' AND t.spec->>'task_type' IN (?1,?2)",
    )?;
    for task in pending.query_map(
        params![artifact_install::TOOL, artifact_install::STAGE_TOOL],
        row,
    )? {
        retained += i64::from(verify_pending(conn, &task?)?);
    }
    let mut releases =
        conn.prepare("SELECT * FROM versions WHERE kind='ApplicationRelease' AND rev=1")?;
    for release in releases.query_map([], row)? {
        let release = release?;
        let source = artifact_install::release_source(conn, &release)?;
        let archive = recorded(conn, &release, source.as_ref())?;
        let retired = find(conn, &release_retirement::receipt_id(&release.id))?;
        let active = archive.as_ref().filter(|_| retired.is_none());
        if let Some(archive) = active {
            let source = source
                .as_ref()
                .ok_or_else(|| err("RecoveryRequired", "retained archive has no declaration"))?;
            let descriptor = artifact_install::read_descriptor(conn, source)?;
            require(
                json!(descriptor.archive) == archive.artifact.reference(),
                "RecoveryRequired",
                "signed declaration disagrees with the retained OCI archive",
            )?;
        }
        check_pin(conn, "Release", &release.id, active)?;
        retained += i64::from(active.is_some());
    }
    let actual: i64 = conn.query_row(
        "SELECT count(*) FROM application_image_archive_pins",
        [],
        |r| r.get(0),
    )?;
    require(
        actual == retained,
        "RecoveryRequired",
        "OCI archive has orphaned retention pins",
    )
}
