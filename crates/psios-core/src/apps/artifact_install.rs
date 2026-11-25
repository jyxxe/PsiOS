//! Adapt immutable local OCI declarations into the application catalog.
use super::release_archive;
use crate::{
    Result, artifact, broker,
    db::{Db, Doc, find, resolve, row},
    decode, host, policy, require,
};
use psios_api::{OciPublisherSignature, ResourceRef, parse};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = artifact::ingress::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("artifact_install.sql");
pub(crate) const TOOL: &str = "application.install_artifact";
pub(crate) const STAGE_FORMAT: u32 = super::release_retirement::FORMAT + 1;
pub(crate) const STAGE_SCHEMA: &str = include_str!("artifact_stage_release.sql");
pub(crate) const STAGE_TOOL: &str = "application.stage_artifact_release";
pub(super) const MEDIA: &str = "application/vnd.psios.oci-install.v1+json";
pub(super) const MEDIA_V2: &str = "application/vnd.psios.oci-install.v2+json";
const DESCRIPTOR: &str = "PsiOSLocalOciInstallV1";
const ARCHIVED_DESCRIPTOR: &str = "PsiOSLocalOciInstallV2";
const MAX_DESCRIPTOR: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub reference: ResourceRef,
    pub signature: Option<OciPublisherSignature>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageRequest {
    pub target: host::Target,
    pub reference: ResourceRef,
    pub signature: Option<OciPublisherSignature>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyDescriptor {
    format: String,
    install: host::Install,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchivedDescriptor {
    format: String,
    install: host::Install,
    image_archive_ref: ResourceRef,
}

pub(super) struct Descriptor {
    pub install: host::Install,
    pub archive: Option<ResourceRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    reference: ResourceRef,
    target: Option<host::Target>,
    signature: Option<OciPublisherSignature>,
    key: String,
    delegation_id: String,
    deadline_at: Option<String>,
}

pub(crate) fn source(db: &Db<'_>, actor: &str, reference: &ResourceRef) -> Result<Doc> {
    artifact::ingress::authorize(db, actor, artifact::ingress::READ)?;
    let doc = db.resolve(&json!(reference))?;
    policy::owned(actor, &doc)?;
    require(
        is_declaration(&doc),
        "UnsupportedCapability",
        "expected a local OCI installation declaration artifact",
    )?;
    artifact::retention::require_content(db.c, &doc)?;
    Ok(doc)
}

pub(super) fn is_declaration(doc: &Doc) -> bool {
    artifact::is_raw(doc)
        && matches!(
            doc.spec["declared_media_type"].as_str(),
            Some(MEDIA | MEDIA_V2)
        )
}

pub(super) fn read_descriptor(conn: &Connection, source: &Doc) -> Result<Descriptor> {
    require(
        is_declaration(source),
        "UnsupportedCapability",
        "unsupported local OCI installation declaration format",
    )?;
    let bytes = artifact::checkpoint_bytes(conn, source, MAX_DESCRIPTOR)?;
    let value = parse(&bytes)?;
    let (format, expected, install, archive) = if source.spec["declared_media_type"] == MEDIA_V2 {
        let descriptor: ArchivedDescriptor = decode(&value)?;
        (
            descriptor.format,
            ARCHIVED_DESCRIPTOR,
            descriptor.install,
            Some(descriptor.image_archive_ref),
        )
    } else {
        let descriptor: LegacyDescriptor = decode(&value)?;
        (descriptor.format, DESCRIPTOR, descriptor.install, None)
    };
    require(
        format == expected,
        "UnsupportedCapability",
        "OCI declaration does not match its media profile",
    )?;
    super::validate(&install)?;
    Ok(Descriptor { install, archive })
}

pub(crate) fn specification(
    db: &Db<'_>,
    actor: &str,
    reference: &ResourceRef,
) -> Result<host::Install> {
    Ok(read_descriptor(db.c, &source(db, actor, reference)?)?.install)
}

pub(crate) fn stage_source(
    db: &Db<'_>,
    actor: &str,
    input: &StageRequest,
    target: &Doc,
) -> Result<Doc> {
    let source = source(db, actor, &input.reference)?;
    let release = read_descriptor(db.c, &source)?.install;
    require(
        release.volume.is_none(),
        "InvalidArgument",
        "staged release inherits the application volume",
    )?;
    super::stage_precondition(target, &release)?;
    Ok(source)
}

pub(crate) fn add_source_audience(
    db: &Db<'_>,
    actor: &str,
    delegation: &Value,
    source: &Doc,
    proof: Option<&OciPublisherSignature>,
    audience: &mut Vec<Value>,
) -> Result<()> {
    let descriptor = read_descriptor(db.c, source)?;
    require(
        descriptor.archive.is_none() || proof.is_some(),
        "PermissionDenied",
        "archived OCI declarations require a publisher signature",
    )?;
    audience.push(source.reference());
    if let Some(signature) = proof {
        let (key, _) = super::publisher::check(db, source, signature)?;
        audience.push(key.reference());
    }
    if let Some(reference) = descriptor.archive {
        policy::tool_scope(db, delegation, &reference.resource_id)?;
        let artifact = db.resolve(&json!(reference))?;
        policy::owned(actor, &artifact)?;
        let archive = release_archive::read(db.c, artifact, &descriptor.install.image_id)?;
        archive.check_content(db)?;
        audience.push(archive.artifact.reference());
    }
    Ok(())
}

pub(crate) fn prepare(db: &Db<'_>, actor: &str, params: &Value, tool: &str) -> Result<Value> {
    let p: Prepare = decode(params)?;
    require(
        (tool == STAGE_TOOL) == p.target.is_some(),
        "InvalidArgument",
        "preparation for a staged release requires an application target",
    )?;
    let grant = policy::delegation(db, actor, &p.delegation_id, tool)?;
    policy::delegation(db, actor, &grant.id, "task.submit")?;
    let mut input = if let Some(target) = p.target {
        json!({"target":target,"reference":p.reference})
    } else {
        json!({"reference":p.reference})
    };
    if let Some(signature) = &p.signature {
        input["signature"] = json!(signature);
    }
    let action = host::Action::parse(tool, &input)?;
    let install = specification(db, actor, &p.reference)?;
    let target = match &action {
        host::Action::StageArtifactRelease(_) => action.target(db, actor)?,
        _ => host::install_target(db, actor, &install)?,
    };
    let refs = action.audience(db, actor, &grant.reference(), &target)?;
    let request = broker::prepare(
        db,
        actor,
        &json!({
            "tool":tool,"input":input,"key":p.key,
            "delegation_id":grant.id,"deadline_at":p.deadline_at,
        }),
    )?;
    let mut response =
        json!({"request":request,"source_artifact_ref":p.reference,"resource_refs":refs});
    if let Some(archive) = refs.get(release_archive::PLAN_POSITION) {
        response["image_archive_ref"] = archive.clone();
    }
    if let Some(signature) = &p.signature {
        let source = source(db, actor, &p.reference)?;
        let (key, publisher) = super::publisher::check(db, &source, signature)?;
        response["publisher"] = json!({"identity_ref":publisher.reference(),"key_ref":key.reference(),
            "source_trust":"PublisherVerified"});
    }
    let preview = json!({"name":install.name,"release_version":install.release_version,
            "image_id":install.image_id,"runtime_profile_ref":db.get("oci-local")?.reference(),
            "cpu_millicores":install.cpu_millicores,"memory_max_bytes":install.memory_max_bytes,
            "pids_max":install.pids_max,"ephemeral_max_bytes":install.ephemeral_max_bytes,
            "argv":install.argv,"environment":install.environment,"license_expression":install.license_expression});
    if tool == STAGE_TOOL {
        response["release"] = preview;
        response["application_ref"] = target.reference();
        response["selected_release_ref"] = target.spec["release_ref"].clone();
    } else {
        response["deployment"] = preview;
        response["deployment"]["desired_state"] = json!("Stopped");
    }
    Ok(response)
}

pub(crate) fn acquire(
    db: &Db<'_>,
    task: &Doc,
    action: &host::Action,
    audience: &[Value],
) -> Result<()> {
    let reference = match action {
        host::Action::InstallArtifact(input) => Some(&input.reference),
        host::Action::StageArtifactRelease(input) => Some(&input.reference),
        _ => None,
    };
    if let Some(reference) = reference {
        db.c.execute(
            "INSERT INTO application_artifact_pins(artifact,holder,kind) VALUES(?1,?2,'Task')",
            params![reference.resource_id, task.id],
        )?;
        if let Some(archive) = audience.get(release_archive::PLAN_POSITION) {
            release_archive::pin_task(db, &task.id, archive)?;
        }
    }
    Ok(())
}

pub(crate) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM application_artifact_pins WHERE kind='Task' AND holder=?1",
        [task],
    )?;
    release_archive::unpin(db, "Task", task)?;
    Ok(())
}

pub(crate) fn pinned(db: &Db<'_>, artifact: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM application_artifact_pins WHERE artifact=?1)
             OR EXISTS(SELECT 1 FROM application_image_archive_pins WHERE artifact=?1)",
        [artifact],
        |r| r.get(0),
    )?)
}

pub(super) fn release_evidence(conn: &Connection, release: &Doc) -> Result<Option<Doc>> {
    Ok(conn
        .query_row(
            "SELECT * FROM versions WHERE kind IN ('ApplicationArtifactInstallation','ApplicationArtifactReleaseStage')
             AND rev=1 AND spec->>'release_id'=?1",
            [&release.id],
            row,
        )
        .optional()?)
}

pub(super) fn release_source(conn: &Connection, release: &Doc) -> Result<Option<Doc>> {
    let Some(evidence) = release_evidence(conn, release)? else {
        return Ok(None);
    };
    let reference = &evidence.body["source_artifact_ref"];
    let source = resolve(conn, reference)
        .map_err(|_| crate::err("RecoveryRequired", "release lost its exact source artifact"))?;
    require(
        evidence.spec["application_id"] == release.body["application_id"]
            && evidence.body["release_ref"] == release.reference()
            && artifact::is_raw(&source),
        "RecoveryRequired",
        "source evidence disagrees with its release",
    )?;
    if evidence.owner != release.owner || source.owner != release.owner {
        let operation = evidence.body["operation_id"].as_str().ok_or_else(|| {
            crate::err(
                "RecoveryRequired",
                "cross-principal receipt has no installation operation",
            )
        })?;
        let op = find(conn, operation)?.ok_or_else(|| {
            crate::err(
                "RecoveryRequired",
                "cross-principal installation operation missing",
            )
        })?;
        let plan = resolve(conn, &op.body["plan_ref"])?;
        let matches_target = match plan.spec["action"].as_str() {
            Some(STAGE_TOOL) => {
                plan.body["allowed_effects"][0]["resource_ref"]["resource_id"]
                    == release.body["application_id"]
            }
            Some(TOOL) => release.body["application_id"] == format!("app-{}", op.id),
            _ => false,
        };
        require(
            evidence.owner == policy::OWNER
                && op.owner == policy::OWNER
                && op.kind == "Operation"
                && op.body["lifecycle"] == "Succeeded"
                && plan.kind == "OperationPlan"
                && plan.owner == policy::OWNER
                && matches_target
                && plan.body["preconditions"][1]["resource_ref"] == source.reference(),
            "RecoveryRequired",
            "cross-principal release source has no approved local authority",
        )?;
    }
    Ok(Some(source))
}

pub(super) fn pinned_source(conn: &Connection, release: &Doc) -> Result<Option<Doc>> {
    let source = release_source(conn, release)?;
    let held: Option<String> = conn
        .query_row(
            "SELECT artifact FROM application_artifact_pins WHERE kind='Release' AND holder=?1",
            [&release.id],
            |r| r.get(0),
        )
        .optional()?;
    require(
        source.as_ref().map(|r| r.id.as_str()) == held.as_deref(),
        "RecoveryRequired",
        "release source disagrees with its artifact retention pin",
    )?;
    Ok(source)
}

pub(super) fn check_release_source(db: &Db<'_>, release: &Doc) -> Result<()> {
    let source = pinned_source(db.c, release)?;
    let binding = release_archive::check_release_pin(db.c, release, source.as_ref())?;
    if let Some(source) = source {
        let descriptor = read_descriptor(db.c, &source)?;
        require(
            descriptor
                .archive
                .as_ref()
                .map(|reference| json!(reference))
                == binding.as_ref().map(|archive| archive.artifact.reference()),
            "RecoveryRequired",
            "signed declaration disagrees with the retained OCI archive",
        )?;
    }
    if let Some(archive) = binding {
        archive.check_content(db)?;
    }
    Ok(())
}

pub(super) fn publisher_verification(
    db: &Db<'_>,
    work: &host::Work,
) -> Result<Option<super::publisher::Verification>> {
    let (reference, signature) = match &work.action {
        host::Action::InstallArtifact(input) => (&input.reference, &input.signature),
        host::Action::StageArtifactRelease(input) => (&input.reference, &input.signature),
        _ => return Ok(None),
    };
    let Some(signature) = signature else {
        return Ok(None);
    };
    let source = source(db, &work.task.owner, reference)?;
    super::publisher::record(db, &work.task.owner, &work.op.id, &source, signature).map(Some)
}

pub(super) fn work_archive(
    db: &Db<'_>,
    work: &host::Work,
) -> Result<Option<release_archive::Archive>> {
    let reference = match &work.action {
        host::Action::InstallArtifact(input) => &input.reference,
        host::Action::StageArtifactRelease(input) => &input.reference,
        _ => return Ok(None),
    };
    let source = source(db, &work.task.owner, reference)?;
    let descriptor = read_descriptor(db.c, &source)?;
    let archive = descriptor
        .archive
        .map(|reference| {
            release_archive::resolve(db.c, &json!(reference), &descriptor.install.image_id)
        })
        .transpose()?;
    release_archive::check_pin(db.c, "Task", &work.task.id, archive.as_ref())?;
    Ok(archive)
}

pub(super) fn drop_release_pin(db: &Db<'_>, release: &Doc) -> Result<Option<Value>> {
    let source = pinned_source(db.c, release)?;
    if source.is_some() {
        db.c.execute(
            "DELETE FROM application_artifact_pins WHERE kind='Release' AND holder=?1",
            [&release.id],
        )?;
    }
    Ok(source.as_ref().map(Doc::reference))
}

fn catalog_receipt(tool: &str) -> (&'static str, &'static str) {
    if tool == TOOL {
        ("ApplicationArtifactInstallation", "artifact-install")
    } else {
        ("ApplicationArtifactReleaseStage", "artifact-stage-release")
    }
}

pub(super) fn recorded_release(
    conn: &Connection,
    op: &Doc,
    action: &str,
    source: &Doc,
) -> Result<Doc> {
    require(
        matches!(action, TOOL | STAGE_TOOL),
        "RecoveryRequired",
        "unsupported release receipt operation",
    )?;
    let (kind, prefix) = catalog_receipt(action);
    let receipt = find(conn, &format!("{prefix}-{}", op.id))?.ok_or_else(|| {
        crate::err(
            "RecoveryRequired",
            "signed release lost its catalog receipt",
        )
    })?;
    let release = resolve(conn, &receipt.body["release_ref"])?;
    let app = resolve(conn, &receipt.body["application_ref"])?;
    require(
        receipt.kind == kind
            && receipt.rev == 1
            && receipt.owner == op.owner
            && receipt.body["operation_id"] == op.id
            && receipt.body["source_artifact_ref"] == source.reference()
            && receipt.spec["application_id"] == app.id
            && receipt.spec["release_id"] == release.id
            && release.kind == "ApplicationRelease"
            && release.body["application_id"] == app.id
            && app.kind == "Application"
            && app.owner == release.owner,
        "RecoveryRequired",
        "signed release disagrees with its exact catalog receipt",
    )?;
    Ok(release)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let inconsistent: bool = conn.query_row(
        "WITH retained(artifact,holder,kind) AS (
           SELECT coalesce(i.body->'tool_input',i.body)->'reference'->>'resource_id', q.task, 'Task'
             FROM queue q JOIN current t ON t.id=q.task AND t.kind='Task'
             LEFT JOIN versions i ON i.id=t.spec->'input_ref'->>'resource_id'
                  AND ('r'||i.rev)=t.spec->'input_ref'->>'version'
            WHERE q.phase<>'Done' AND t.spec->>'task_type' IN (?1,?2)
           UNION ALL
           SELECT e.body->'source_artifact_ref'->>'resource_id', e.spec->>'release_id', 'Release'
             FROM versions e WHERE e.kind IN ('ApplicationArtifactInstallation','ApplicationArtifactReleaseStage') AND e.rev=1
                AND NOT EXISTS(SELECT 1 FROM versions r WHERE r.id='retired-'||(e.spec->>'release_id')
                   AND r.kind='ApplicationReleaseRetirement' AND r.rev=1)
         ) SELECT
          EXISTS(SELECT 1 FROM retained e LEFT JOIN application_artifact_pins p
                   ON p.kind=e.kind AND p.holder=e.holder AND p.artifact=e.artifact
                  WHERE p.holder IS NULL)
          OR EXISTS(SELECT 1 FROM application_artifact_pins p
                     WHERE NOT EXISTS(SELECT 1 FROM retained e WHERE p.kind=e.kind
                       AND p.holder=e.holder AND p.artifact=e.artifact)
                       OR NOT EXISTS(SELECT 1 FROM artifact_uploads u
                         WHERE u.artifact_id=p.artifact AND u.state='Published')
                       OR (p.kind='Release' AND NOT EXISTS(SELECT 1 FROM versions v
                         WHERE v.id=p.holder AND v.kind='ApplicationRelease' AND v.rev=1)))",
        [TOOL, STAGE_TOOL],
        |r| r.get(0),
    )?;
    require(
        !inconsistent,
        "RecoveryRequired",
        "application artifact retention pins disagree with installed or pending state",
    )
}

pub(super) fn retain(
    db: &Db<'_>,
    work: &host::Work,
    application: &Doc,
    release: &Doc,
    deployment: Option<&Doc>,
) -> Result<Option<Value>> {
    let (reference, tool) = match (&work.action, deployment) {
        (host::Action::InstallArtifact(input), Some(_)) => (&input.reference, TOOL),
        (host::Action::StageArtifactRelease(input), None) => (&input.reference, STAGE_TOOL),
        _ => return Ok(None),
    };
    let (kind, record) = catalog_receipt(tool);
    let applied = db.c.execute(
        "INSERT INTO application_artifact_pins(artifact,holder,kind)
         SELECT artifact,?3,'Release' FROM application_artifact_pins
          WHERE artifact=?1 AND holder=?2 AND kind='Task'",
        params![reference.resource_id, work.task.id, release.id],
    )?;
    require(
        applied == 1,
        "RecoveryRequired",
        "application catalog change lost its artifact retention pin",
    )?;
    let mut result = json!({"application_ref":application.reference(),"release_ref":release.reference(),
        "source_artifact_ref":reference,
        "operation_id":work.op.id});
    if let Some(archive) = work_archive(db, work)? {
        release_archive::pin_release(db, &work.task.id, &release.id, &archive.artifact.id)?;
        result["image_archive_ref"] = archive.artifact.reference();
        result["image_capture_ref"] = archive.proof.reference();
    }
    if let Some(deployment) = deployment {
        result["deployment_ref"] = deployment.reference();
    }
    let evidence = db.create(
        kind,
        &format!("{record}-{}", work.op.id),
        &work.task.owner,
        json!({"application_id":application.id,"release_id":release.id}),
        result.clone(),
    )?;
    let key = if deployment.is_some() {
        "installation_ref"
    } else {
        "stage_ref"
    };
    result[key] = evidence.reference();
    Ok(Some(result))
}
