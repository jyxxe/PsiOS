//! Recover exactly one approved release image into a classic local Docker cache.
mod engine;
mod verification;
use super::{
    artifact_install, capture, publisher, release_archive, release_retirement, restore_archive,
};
use crate::{
    Result, artifact,
    db::{Db, Doc, find, resolve, row},
    decode, err,
    host::{Target, Work},
    now, policy, require,
};
use psios_api::ResourceRef;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::File;
pub(crate) use verification::verify;

pub(crate) const TOOL: &str = "application.restore_release_image";
pub(crate) const INSPECT: &str = "application.inspect_release_image_restore";
pub(crate) const FORMAT: u32 = restore_archive::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("restore_image.sql");
const INTENT: &str = "OciImageRestoreIntent";
const CHECKPOINT: &str = "OciImageRestoreCheckpoint";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Target,
    release_ref: ResourceRef,
    image_archive_ref: ResourceRef,
    publisher_key_ref: ResourceRef,
    expected_engine_id: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            capture::valid_engine_id(&self.expected_engine_id),
            "InvalidArgument",
            "restore requires an exact local Docker Engine ID",
        )
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    application_ref: ResourceRef,
    release_ref: ResourceRef,
    declaration_ref: ResourceRef,
    publisher_key_ref: ResourceRef,
    image_archive_ref: ResourceRef,
    image_capture_ref: ResourceRef,
    node_ref: ResourceRef,
    engine_id: String,
    image_id: String,
    diff_ids: Vec<String>,
}

impl Binding {
    fn audience(&self) -> Vec<Value> {
        [
            &self.application_ref,
            &self.release_ref,
            &self.declaration_ref,
            &self.publisher_key_ref,
            &self.image_archive_ref,
            &self.node_ref,
        ]
        .map(|reference| json!(reference))
        .to_vec()
    }

    fn matches_request(&self, request: &Request) -> bool {
        self.application_ref.resource_id == request.target.id
            && self.application_ref.version == request.target.expected_resource_version
            && self.release_ref == request.release_ref
            && self.image_archive_ref == request.image_archive_ref
            && self.publisher_key_ref == request.publisher_key_ref
            && self.engine_id == request.expected_engine_id
    }
}

fn source(
    db: &Db<'_>,
    app: &Doc,
    request: &Request,
) -> Result<(Binding, release_archive::Archive)> {
    require(
        app.body["lifecycle"] != "Uninstalled",
        "PreconditionFailed",
        "image restoration requires an installed application",
    )?;
    let release = release_retirement::associated(db, app, &json!(request.release_ref))?;
    release_retirement::available(db, &release)?;
    let declaration = artifact_install::pinned_source(db.c, &release)?.ok_or_else(|| {
        err(
            "PreconditionFailed",
            "restore requires a signed V2 OCI declaration",
        )
    })?;
    let archive = release_archive::check_release_pin(db.c, &release, Some(&declaration))?
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "release has no retained signed OCI archive",
            )
        })?;
    let descriptor = artifact_install::read_descriptor(db.c, &declaration)?;
    let spec = db.resolve(&release.body["release_spec_ref"])?;
    publisher::check_launch(db, &release, &spec)?;
    let verified =
        db.resolve(&spec.body["provenance"]["trust_evidence_or_admission_policy_ref"])?;
    let binding = Binding {
        application_ref: decode(&app.reference())?,
        release_ref: decode(&release.reference())?,
        declaration_ref: decode(&declaration.reference())?,
        publisher_key_ref: decode(&verified.body["signature"]["key_ref"])?,
        image_archive_ref: decode(&archive.artifact.reference())?,
        image_capture_ref: decode(&archive.proof.reference())?,
        node_ref: decode(&db.of(&db.meta("authority")?, "Node")?.reference())?,
        engine_id: request.expected_engine_id.clone(),
        image_id: decode(&archive.proof.body["image"]["config_digest"])?,
        diff_ids: decode(&archive.proof.body["image"]["diff_ids"])?,
    };
    require(
        binding.matches_request(request)
            && descriptor.archive.as_ref() == Some(&binding.image_archive_ref)
            && descriptor.install.image_id == binding.image_id,
        "PreconditionFailed",
        "requested archive, image or publisher differs from the signed release",
    )?;
    Ok((binding, archive))
}

pub(crate) fn audience(
    db: &Db<'_>,
    app: &Doc,
    input: &Request,
    resources: &mut Vec<Value>,
) -> Result<()> {
    let (binding, _) = source(db, app, input)?;
    resources.extend(binding.audience().into_iter().skip(1));
    Ok(())
}

pub(crate) struct Prepared {
    binding: Binding,
    archive: File,
}

pub(crate) fn prepare(db: &Db<'_>, app: &Doc, request: &Request) -> Result<Prepared> {
    let (binding, archive) = source(db, app, request)?;
    let (archive, _) = engine::seal_file(|output| {
        artifact::copy_verified_raw(
            db.c,
            Some(&db.cfg.state_dir),
            &archive.artifact,
            artifact::ingress::MAX_BYTES as usize,
            output,
        )
    })?;
    Ok(Prepared { binding, archive })
}

fn intent_id(op: &str) -> String {
    format!("oci-image-restore-intent-{op}")
}

fn checkpoint_id(op: &str) -> String {
    format!("oci-image-restore-checkpoint-{op}")
}

fn held(conn: &Connection, op: &Doc, binding: &Binding) -> Result<()> {
    let held = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)
            AND EXISTS(SELECT 1 FROM oci_image_restore_holds
                       WHERE operation=?2 AND engine_id=?3 AND image_id=?4)",
        params![
            binding.application_ref.resource_id,
            op.id,
            binding.engine_id,
            binding.image_id
        ],
        |row| row.get::<_, bool>(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "original application and engine/image holds are missing",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work, prepared: &Prepared) -> Result<()> {
    let binding = &prepared.binding;
    require(
        db.c.execute(
            "INSERT OR IGNORE INTO oci_image_restore_holds VALUES(?1,?2,?3)",
            params![binding.engine_id, binding.image_id, work.op.id],
        )? == 1,
        "Conflict",
        "local OCI engine and image are held by another restore",
    )?;
    held(db.c, &work.op, binding)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,"binding":binding}),
    )?;
    Ok(())
}

pub(crate) fn release(db: &Db<'_>, operation: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM oci_image_restore_holds WHERE operation=?1",
        [operation],
    )?;
    Ok(())
}

fn intent(conn: &Connection, op: &Doc, doc: &Doc) -> Result<Binding> {
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let input = resolve(conn, &plan.body["purpose_ref"])?;
    let request: Request = decode(crate::broker::dependencies::tool_input(&input)?)?;
    let binding: Binding = decode(&doc.body["binding"])?;
    let references = binding.audience();
    require(
        op.kind == "Operation"
            && op.owner == policy::OWNER
            && plan.spec["action"] == TOOL
            && plan.body["allowed_effects"][0]["resource_ref"] == references[0]
            && plan.body["preconditions"]
                .as_array()
                .is_some_and(|preconditions| {
                    preconditions.len() == references.len()
                        && preconditions
                            .iter()
                            .zip(&references)
                            .all(|(condition, rf)| condition["resource_ref"] == *rf)
                })
            && doc.kind == INTENT
            && doc.owner == op.owner
            && doc.rev == 1
            && doc.id == intent_id(&op.id)
            && doc.spec["operation_id"] == op.id
            && doc.body["plan_ref"] == op.body["plan_ref"]
            && doc.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
            && binding.matches_request(&request)
            && capture::valid_engine_id(&binding.engine_id)
            && super::valid_image_id(&binding.image_id)
            && (1..=128).contains(&binding.diff_ids.len())
            && binding
                .diff_ids
                .iter()
                .all(|digest| super::valid_image_id(digest)),
        "RecoveryRequired",
        "OCI image restore intent disagrees with the approved operation",
    )?;
    Ok(binding)
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(document) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    let binding = intent(db.c, op, &document)?;
    held(db.c, op, &binding)?;
    Ok(db.find(&checkpoint_id(&op.id))?.is_none())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Dispatch {
    normalized_digest: String,
}

pub(crate) fn record(
    db: &Db<'_>,
    work: &Work,
    target: &Doc,
    request: &Request,
    dispatch: &Dispatch,
) -> Result<()> {
    let (binding, archive) = source(db, target, request)?;
    let expected = &work
        .restore_image
        .as_ref()
        .ok_or_else(missing_input)?
        .binding;
    require(
        binding == *expected
            && not_dispatched(db, &work.op)?
            && super::valid_image_id(&dispatch.normalized_digest),
        "Fenced",
        "restore dispatch differs from the original permitted target",
    )?;
    archive.check_content(db)?;
    let doc = db.get(&intent_id(&work.op.id))?;
    db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"intent_ref":doc.reference(),"plan_ref":work.op.body["plan_ref"],
            "effect_id":work.effect.id,"approved_archive_digest":binding.image_archive_ref.digest,
            "normalized_archive_digest":dispatch.normalized_digest,"previous_presence":"Absent",
            "checkpointed_at":now()}),
    )?;
    Ok(())
}

fn missing_input() -> crate::Error {
    err(
        "RecoveryRequired",
        "OCI restore work is missing its admitted input",
    )
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum Presence {
    Absent,
    Present,
}

fn observation(binding: &Binding, presence: Presence) -> Value {
    let mut sample = json!({"engine_id":binding.engine_id,"image_id":binding.image_id,
        "presence":presence,"observed_at":now()});
    if presence == Presence::Present {
        sample["image_diff_ids"] = json!(binding.diff_ids);
    }
    sample
}

fn unknown() -> crate::Error {
    err(
        "OutcomeUnknown",
        "Docker did not confirm the checkpointed image cache postcondition",
    )
}

pub(crate) fn run(work: &Work, checkpoint: impl FnOnce(&Dispatch) -> Result<()>) -> Result<Value> {
    let prepared = work.restore_image.as_ref().ok_or_else(missing_input)?;
    let binding = &prepared.binding;
    let (input, normalized_digest) = engine::Input::seal(|output| {
        restore_archive::write_load(
            &prepared.archive,
            output,
            &binding.image_id,
            &binding.diff_ids,
        )
    })?;
    let dispatch = Dispatch { normalized_digest };
    engine::runtime()?.block_on(async {
        let mut peer = engine::Connection::open(&work.cfg.docker_socket).await?;
        peer.identity(&binding.engine_id).await?;
        require(
            peer.observe(binding).await? == Presence::Absent,
            "PreconditionFailed",
            "the exact release image is already present in the Docker cache",
        )?;
        peer.ready().await?;
        checkpoint(&dispatch)?;
        peer.load(input, &binding.image_id)
            .await
            .map_err(|_| unknown())?;
        let presence = peer.observe(binding).await.map_err(|_| unknown())?;
        require(
            presence == Presence::Present,
            "OutcomeUnknown",
            "Docker replied without the approved image",
        )?;
        Ok(observation(binding, presence))
    })
}

pub(crate) struct Recorded {
    binding: Binding,
    checkpoint: Doc,
}

fn recorded(conn: &Connection, op: &Doc, intent_doc: &Doc, checkpoint: Doc) -> Result<Recorded> {
    let binding = intent(conn, op, intent_doc)?;
    require(
        checkpoint.kind == CHECKPOINT
            && checkpoint.rev == 1
            && checkpoint.id == checkpoint_id(&op.id)
            && checkpoint.spec["operation_id"] == op.id
            && checkpoint.owner == op.owner
            && checkpoint.body["intent_ref"] == intent_doc.reference()
            && checkpoint.body["plan_ref"] == op.body["plan_ref"]
            && checkpoint.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
            && checkpoint.body["approved_archive_digest"] == binding.image_archive_ref.digest
            && checkpoint.body["previous_presence"] == "Absent"
            && checkpoint.body["normalized_archive_digest"]
                .as_str()
                .is_some_and(super::valid_image_id),
        "RecoveryRequired",
        "OCI load checkpoint disagrees with the original effect",
    )?;
    Ok(Recorded {
        binding,
        checkpoint,
    })
}

fn load_checkpoint(db: &Db<'_>, op: &Doc) -> Result<Recorded> {
    let intent = db.of(&intent_id(&op.id), INTENT)?;
    let checkpoint = db.of(&checkpoint_id(&op.id), CHECKPOINT)?;
    let recorded = recorded(db.c, op, &intent, checkpoint)?;
    held(db.c, op, &recorded.binding)?;
    Ok(recorded)
}

fn open_operation(db: &Db<'_>, op: &Doc) -> Result<Recorded> {
    require(
        op.body["outcome_knowledge"] == "Unknown"
            && db.c.query_row(
                "SELECT EXISTS(SELECT 1 FROM queue WHERE operation=?1 AND phase='Unknown')",
                [&op.id],
                |row| row.get::<_, bool>(0),
            )?,
        "PreconditionFailed",
        "inspection requires the unresolved original OCI restore operation",
    )?;
    let recorded = load_checkpoint(db, op)?;
    let application = db.of(&recorded.binding.application_ref.resource_id, "Application")?;
    require(
        application.reference() == json!(recorded.binding.application_ref),
        "RecoveryRequired",
        "application revision differs from the original OCI restore",
    )?;
    Ok(recorded)
}

pub(crate) fn inspection_audience(
    db: &Db<'_>,
    operation: &Doc,
    resources: &mut Vec<Value>,
) -> Result<()> {
    let recorded = open_operation(db, operation)?;
    resources.extend(recorded.binding.audience());
    Ok(())
}

pub(crate) fn pending(db: &Db<'_>, operation: &Doc) -> Result<Recorded> {
    open_operation(db, operation)
}

pub(crate) fn inspect(work: &Work) -> Result<Value> {
    let recorded = work
        .restore_image_inspection
        .as_ref()
        .ok_or_else(missing_input)?;
    let binding = &recorded.binding;
    engine::runtime()?.block_on(async {
        let mut peer = engine::Connection::open(&work.cfg.docker_socket).await?;
        peer.identity(&binding.engine_id).await?;
        let presence = peer.observe(binding).await?;
        let mut receipt = observation(binding, presence);
        receipt["checkpoint_ref"] = recorded.checkpoint.reference();
        receipt["original_operation_ref"] = work.target.reference();
        Ok(receipt)
    })
}

fn confirmed(db: &Db<'_>, op: &Doc, app: &Doc, observed: &Value) -> Result<Recorded> {
    let recorded = load_checkpoint(db, op)?;
    let binding = &recorded.binding;
    require(
        json!(binding.application_ref) == app.reference() && cache_present(binding, observed),
        "OutcomeUnknown",
        "inspection does not confirm the exact cache postcondition of the original OCI effect",
    )?;
    Ok(recorded)
}

fn cache_present(binding: &Binding, observed: &Value) -> bool {
    observed["engine_id"] == binding.engine_id
        && observed["image_id"] == binding.image_id
        && observed["image_diff_ids"] == json!(binding.diff_ids)
        && observed["presence"] == "Present"
        && observed["observed_at"]
            .as_str()
            .is_some_and(|at| chrono::DateTime::parse_from_rfc3339(at).is_ok())
}

fn receipt(op: &Doc, recorded: &Recorded, observed: &Value) -> Value {
    json!({"operation_id":op.id,"application_ref":recorded.binding.application_ref,
        "release_ref":recorded.binding.release_ref,"image_archive_ref":recorded.binding.image_archive_ref,
        "checkpoint_ref":recorded.checkpoint.reference(),"observation":observed})
}

pub(crate) fn settle(db: &Db<'_>, work: &Work, observed: &Value) -> Result<Value> {
    let recorded = confirmed(db, &work.op, &work.target, observed)?;
    Ok(receipt(&work.op, &recorded, observed))
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    app: &Doc,
    proof: &Doc,
    observed: &Value,
) -> Result<(Value, Value)> {
    let recorded = confirmed(db, op, app, observed)?;
    require(
        observed["original_operation_ref"] == op.reference()
            && observed["checkpoint_ref"] == recorded.checkpoint.reference(),
        "OutcomeUnknown",
        "inspection did not read this unresolved operation and checkpoint",
    )?;
    Ok((
        receipt(op, &recorded, observed),
        json!({"observation_ref":proof.reference(),"checkpoint_ref":recorded.checkpoint.reference(),
            "conclusion":"ObservedExactOciImageCache"}),
    ))
}
