//! Adapt an immutable local OCI installation declaration into the existing broker.
use crate::{
    Result, artifact, broker,
    db::{Db, Doc},
    decode, host, policy, require,
};
use psios_api::{ResourceRef, parse};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = artifact::ingress::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("artifact_install.sql");
pub(crate) const TOOL: &str = "application.install_artifact";
const MEDIA: &str = "application/vnd.psios.oci-install.v1+json";
const DESCRIPTOR: &str = "PsiOSLocalOciInstallV1";
const MAX_DESCRIPTOR: usize = 64 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub reference: ResourceRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    format: String,
    install: host::Install,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    reference: ResourceRef,
    key: String,
    delegation_id: String,
    deadline_at: Option<String>,
}

pub(crate) fn source(db: &Db<'_>, actor: &str, input: &Request) -> Result<Doc> {
    artifact::ingress::authorize(db, actor, artifact::ingress::READ)?;
    let doc = db.resolve(&json!(input.reference))?;
    policy::owned(actor, &doc)?;
    require(
        artifact::is_raw(&doc) && doc.spec["declared_media_type"] == MEDIA,
        "UnsupportedCapability",
        "expected a local OCI installation declaration artifact",
    )?;
    artifact::retention::require_content(db.c, &doc)?;
    Ok(doc)
}

pub(crate) fn specification(db: &Db<'_>, actor: &str, input: &Request) -> Result<host::Install> {
    let doc = source(db, actor, input)?;
    let bytes = artifact::checkpoint_bytes(db.c, &doc, MAX_DESCRIPTOR)?;
    let descriptor: Descriptor = decode(&parse(&bytes)?)?;
    require(
        descriptor.format == DESCRIPTOR,
        "UnsupportedCapability",
        "unsupported local OCI installation declaration format",
    )?;
    super::validate(&descriptor.install)?;
    Ok(descriptor.install)
}

pub(crate) fn prepare(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Prepare = decode(params)?;
    let grant = policy::delegation(db, actor, &p.delegation_id, TOOL)?;
    policy::delegation(db, actor, &grant.id, "task.submit")?;
    let input = Request {
        reference: p.reference,
    };
    let install = specification(db, actor, &input)?;
    let target = host::install_target(db, actor, &install)?;
    let refs = host::Action::InstallArtifact(input.clone()).audience(
        db,
        actor,
        &grant.reference(),
        &target,
    )?;
    let request = broker::prepare(
        db,
        actor,
        &json!({
            "tool":TOOL,"input":{"reference":input.reference},"key":p.key,
            "delegation_id":grant.id,"deadline_at":p.deadline_at,
        }),
    )?;
    Ok(json!({
        "request":request,"source_artifact_ref":input.reference,"resource_refs":refs,
        "deployment":{"name":install.name,"release_version":install.release_version,
            "image_id":install.image_id,"runtime_profile_ref":db.get("oci-local")?.reference(),
            "cpu_millicores":install.cpu_millicores,"memory_max_bytes":install.memory_max_bytes,
            "pids_max":install.pids_max,"ephemeral_max_bytes":install.ephemeral_max_bytes,
            "argv":install.argv,"environment":install.environment,"license_expression":install.license_expression,
            "desired_state":"Stopped"},
    }))
}

pub(crate) fn acquire(db: &Db<'_>, task: &Doc, action: &host::Action) -> Result<()> {
    if let host::Action::InstallArtifact(input) = action {
        db.c.execute(
            "INSERT INTO application_artifact_pins(artifact,holder,kind) VALUES(?1,?2,'Task')",
            params![input.reference.resource_id, task.id],
        )?;
    }
    Ok(())
}

pub(crate) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM application_artifact_pins WHERE kind='Task' AND holder=?1",
        [task],
    )?;
    Ok(())
}

pub(crate) fn pinned(db: &Db<'_>, artifact: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM application_artifact_pins WHERE artifact=?1)",
        [artifact],
        |r| r.get(0),
    )?)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let inconsistent: bool = conn.query_row(
        "WITH retained(artifact,holder,kind) AS (
           SELECT coalesce(i.body->'tool_input',i.body)->'reference'->>'resource_id', q.task, 'Task'
             FROM queue q JOIN current t ON t.id=q.task AND t.kind='Task'
             LEFT JOIN versions i ON i.id=t.spec->'input_ref'->>'resource_id'
                  AND ('r'||i.rev)=t.spec->'input_ref'->>'version'
            WHERE q.phase<>'Done' AND t.spec->>'task_type'=?1
           UNION ALL
           SELECT e.body->'source_artifact_ref'->>'resource_id', e.spec->>'release_id', 'Release'
             FROM versions e WHERE e.kind='ApplicationArtifactInstallation' AND e.rev=1
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
        [TOOL],
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
    deployment: &Doc,
) -> Result<Option<Value>> {
    let host::Action::InstallArtifact(input) = &work.action else {
        return Ok(None);
    };
    let applied = db.c.execute(
        "INSERT INTO application_artifact_pins(artifact,holder,kind)
         SELECT artifact,?3,'Release' FROM application_artifact_pins
          WHERE artifact=?1 AND holder=?2 AND kind='Task'",
        params![input.reference.resource_id, work.task.id, release.id],
    )?;
    require(
        applied == 1,
        "RecoveryRequired",
        "installation lost its artifact retention pin",
    )?;
    let result = json!({"application_ref":application.reference(),"release_ref":release.reference(),
        "deployment_ref":deployment.reference(),"source_artifact_ref":input.reference,
        "operation_id":work.op.id});
    let evidence = db.create(
        "ApplicationArtifactInstallation",
        &format!("artifact-install-{}", work.op.id),
        &work.task.owner,
        json!({"application_id":application.id,"release_id":release.id}),
        result.clone(),
    )?;
    let mut result = result;
    result["installation_ref"] = evidence.reference();
    Ok(Some(result))
}
