//! Durable ingress for opaque local artifacts. Publication verifies exact bytes.
use super::{CHUNK_BYTES, RAW_LAYOUT, abandonment};
use crate::{
    Empty, Error, Result, after,
    db::{Db, Doc, find},
    decode, err, label, now, policy, require,
    storage::oci_archive::Sealed,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::{ApiError, ResourceRef, canonical, id, sha256};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Read;

pub(crate) mod authority;

pub(crate) const FORMAT: u32 = crate::node::process::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("ingress.sql");
pub(crate) const UPLOAD: &str = "artifact.upload";
pub(crate) const READ: &str = "artifact.read";
pub(crate) const WITHDRAW: &str = "artifact.withdraw";
pub(crate) const MAX_BYTES: u32 = 8 * 1024 * 1024;
pub(crate) const OWNER_BYTES: u32 = 32 * 1024 * 1024;
pub(super) const TOTAL_BYTES: u32 = 128 * 1024 * 1024;
const OWNER_STAGES: u32 = 4;
const TOTAL_STAGES: u32 = 32;
const OWNER_REQUESTS: u32 = 4096;
const TOTAL_REQUESTS: u32 = 65536;
const WITHDRAWAL: &str = "ArtifactWithdrawal";
pub(super) const ABANDONMENT: &str = "ArtifactAbandonment";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Start {
    key: String,
    size_bytes: u32,
    digest: String,
    declared_media_type: String,
    ttl_seconds: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delegation_ref: Option<ResourceRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageId {
    stage_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Put {
    stage_id: String,
    offset: u32,
    data_base64: String,
}

struct Stage {
    id: String,
    request_digest: String,
    digest: String,
    declared_media_type: String,
    size: u32,
    received: u32,
    expires: String,
    state: String,
    artifact: Option<String>,
}

fn stage_row(row: &Row<'_>) -> rusqlite::Result<Stage> {
    Ok(Stage {
        id: row.get("id")?,
        request_digest: row.get("request_digest")?,
        digest: row.get("digest")?,
        declared_media_type: row.get("declared_media_type")?,
        size: row.get("size_bytes")?,
        received: row.get("received_bytes")?,
        expires: row.get("expires_at")?,
        state: row.get("state")?,
        artifact: row.get("artifact_id")?,
    })
}

fn owned(db: &Db<'_>, actor: &str, id: &str) -> Result<Stage> {
    label(id)?;
    db.c.query_row(
        "SELECT * FROM artifact_uploads WHERE owner=?1 AND id=?2",
        params![actor, id],
        stage_row,
    )
    .optional()?
    .ok_or_else(|| err("NotFound", "artifact stage unavailable"))
}

fn response(db: &Db<'_>, stage: &Stage) -> Result<Value> {
    let reference = stage
        .artifact
        .as_deref()
        .map(|id| db.of(id, "Artifact").map(|doc| doc.reference()))
        .transpose()?;
    let expired = stage.state == "Open"
        && policy::deadline::Clock::sample()?.expired(db, &stage.id, &stage.expires)?;
    let mut view = json!({
        "stage_id":stage.id,"digest":stage.digest,"size_bytes":stage.size,
        "received_bytes":stage.received,"expires_at":stage.expires,
        "state":if expired {"Expired"} else {&stage.state},"artifact_ref":reference,
    });
    if let Some(binding) = authority::lookup(db.c, &stage.id)? {
        view["delegation_ref"] = json!(binding.delegation_ref);
        if let Some(reason) = binding.close_reason {
            view["close_reason"] = json!(reason);
        }
    }
    Ok(view)
}

fn require_open(db: &Db<'_>, stage: &Stage) -> Result<()> {
    require(
        stage.state != "Expired"
            && !policy::deadline::Clock::sample()?.expired(db, &stage.id, &stage.expires)?,
        "DeadlineExceeded",
        "artifact upload expired",
    )?;
    require(
        stage.state == "Open",
        "PreconditionFailed",
        "artifact stage is closed",
    )
}

pub(crate) fn authorize(db: &Db<'_>, actor: &str, action: &str) -> Result<()> {
    policy::can_target(db, actor, action, &db.meta("authority")?)
}

fn authorize_stage(db: &Db<'_>, actor: &str, stage_id: &str) -> Result<()> {
    if let Some(binding) = authority::lookup(db.c, stage_id)? {
        authority::authorize(db, actor, &binding.delegation_ref)?;
    }
    Ok(())
}

pub(crate) fn profile(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let _: Empty = decode(params)?;
    authorize(db, actor, UPLOAD)?;
    Ok(json!({
        "format":"OpaqueBytesV1","media_type":"application/octet-stream",
        "max_bytes":MAX_BYTES,"chunk_bytes":CHUNK_BYTES,"owner_bytes":OWNER_BYTES,
        "total_bytes":TOTAL_BYTES,"owner_open_stages":OWNER_STAGES,"total_open_stages":TOTAL_STAGES,
        "min_ttl_seconds":5,"max_ttl_seconds":3600,
        "exact_delegation":true,
    }))
}

pub(super) fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

pub(super) fn valid_media_type(value: &str) -> bool {
    value.len() <= 96
        && value.split_once('/').is_some_and(|(kind, subtype)| {
            [kind, subtype].into_iter().all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&c))
            })
        })
}

pub(crate) fn check_capacity(db: &Db<'_>, actor: &str, bytes: u32) -> Result<()> {
    let (total, own, total_open, owner_open, total_count, owner_count): (u32, u32, u32, u32, u32, u32) =
        db.c.query_row(
            "SELECT coalesce(sum(size_bytes),0),
                    coalesce(sum(size_bytes) FILTER(WHERE owner=?1),0),
                    count(*) FILTER(WHERE state='Open'),count(*) FILTER(WHERE owner=?1 AND state='Open'),
                    (SELECT count(*) FROM artifact_uploads),
                    (SELECT count(*) FROM artifact_uploads WHERE owner=?1)
               FROM artifact_uploads INDEXED BY artifact_active_uploads WHERE state IN ('Open','Published')",
            [actor], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
        )?;
    require(
        total_count < TOTAL_REQUESTS && owner_count < OWNER_REQUESTS,
        "QuotaExceeded",
        "artifact upload receipt capacity reached",
    )?;
    if total_open >= TOTAL_STAGES
        || owner_open >= OWNER_STAGES
        || total.saturating_add(bytes) > TOTAL_BYTES
        || own.saturating_add(bytes) > OWNER_BYTES
    {
        let mut quota = ApiError::new(
            "QuotaExceeded",
            "artifact upload staging or retained bytes are full",
        );
        quota.retryable = true;
        quota.retry_condition = "SameRequestAfterBackoff".into();
        return Err(Error(Box::new(quota)));
    }
    Ok(())
}

pub(crate) fn stage(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let stage = begin_stage(db, actor, decode(params)?, MAX_BYTES)?;
    response(db, &stage)
}

fn begin_stage(db: &Db<'_>, actor: &str, p: Start, maximum: u32) -> Result<Stage> {
    label(&p.key)?;
    require(
        valid_digest(&p.digest)
            && valid_media_type(&p.declared_media_type)
            && (5..=3600).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid artifact digest, media type or upload lifetime",
    )?;
    require(
        (1..=maximum).contains(&p.size_bytes),
        "QuotaExceeded",
        "artifact size outside ingress profile",
    )?;
    let request_digest = sha256(&canonical(&serde_json::to_value(&p)?));
    let prior =
        db.c.query_row(
            "SELECT * FROM artifact_uploads WHERE owner=?1 AND request_key=?2",
            params![actor, p.key],
            stage_row,
        )
        .optional()?;
    if let Some(prior) = prior {
        require(
            prior.request_digest == request_digest,
            "IdempotencyConflict",
            "upload key is already bound to a different request",
        )?;
        if prior.state == "Open" {
            authorize_stage(db, actor, &prior.id)?;
        }
        return Ok(prior);
    }
    if let Some(reference) = &p.delegation_ref {
        authority::authorize(db, actor, reference)?;
    }
    db.cfg.check_admission_space()?;
    check_capacity(db, actor, p.size_bytes)?;
    let id = id(if p.delegation_ref.is_some() {
        authority::STAGE_PREFIX
    } else {
        "artifact-stage"
    });
    let expires = after(i64::from(p.ttl_seconds));
    policy::deadline::Clock::sample()?.register_until(db, &id, &expires, None)?;
    db.c.execute(
        "INSERT INTO artifact_uploads(id,owner,request_key,request_digest,digest,declared_media_type,size_bytes,expires_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![id,actor,p.key,request_digest,p.digest,p.declared_media_type,p.size_bytes,expires],
    )?;
    if let Some(reference) = &p.delegation_ref {
        authority::attach(db, &id, reference, p.ttl_seconds)?;
    }
    db.audit(actor, "Artifact.Stage", Some(&id), "Accepted")?;
    owned(db, actor, &id)
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let p: StageId = decode(params)?;
    response(db, &owned(db, actor, &p.stage_id)?)
}

pub(crate) fn put(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let p: Put = decode(params)?;
    let mut stage = owned(db, actor, &p.stage_id)?;
    require_open(db, &stage)?;
    authorize_stage(db, actor, &stage.id)?;
    let offset = p.offset as usize;
    require(
        p.offset < stage.size && offset.is_multiple_of(CHUNK_BYTES),
        "InvalidArgument",
        "upload offset must identify one complete chunk",
    )?;
    let length = ((stage.size - p.offset) as usize).min(CHUNK_BYTES);
    require(
        p.data_base64.len() == 4 * length.div_ceil(3),
        "InvalidArgument",
        "upload must supply one complete chunk",
    )?;
    let raw = STANDARD
        .decode(p.data_base64)
        .map_err(|_| err("InvalidArgument", "invalid artifact chunk encoding"))?;
    require(
        raw.len() == length,
        "InvalidArgument",
        "invalid artifact chunk size",
    )?;
    let index = (offset / CHUNK_BYTES) as i64;
    if p.offset < stage.received {
        let previous: Vec<u8> = db.c.query_row(
            "SELECT payload FROM artifact_upload_chunks WHERE upload_id=?1 AND chunk_index=?2",
            params![stage.id, index],
            |r| r.get(0),
        )?;
        require(
            previous == raw,
            "IdempotencyConflict",
            "upload chunk offset already holds other bytes",
        )?;
        return response(db, &stage);
    }
    require(
        p.offset == stage.received,
        "PreconditionFailed",
        "upload chunks must follow the committed offset",
    )?;
    db.cfg.check_admission_space()?;
    authorize(db, actor, UPLOAD)?;
    require_open(db, &stage)?;
    authorize_stage(db, actor, &stage.id)?;
    append_chunk(db, &mut stage, &raw)?;
    response(db, &stage)
}

fn append_chunk(db: &Db<'_>, stage: &mut Stage, bytes: &[u8]) -> Result<()> {
    let index = (stage.received as usize / CHUNK_BYTES) as i64;
    db.c.execute(
        "INSERT INTO artifact_upload_chunks VALUES(?1,?2,?3,?4)",
        params![stage.id, index, sha256(bytes), bytes],
    )?;
    stage.received += bytes.len() as u32;
    db.c.execute(
        "UPDATE artifact_uploads SET received_bytes=?2 WHERE id=?1",
        params![stage.id, stage.received],
    )?;
    Ok(())
}

fn scan_chunks(
    conn: &Connection,
    id: &str,
    size: usize,
    received: usize,
) -> Result<(String, Vec<String>)> {
    let mut content = Sha256::new();
    let mut digests = Vec::new();
    let mut statement = conn.prepare_cached(
        "SELECT chunk_index,digest,payload FROM artifact_upload_chunks WHERE upload_id=?1 ORDER BY chunk_index",
    )?;
    let mut rows = statement.query([id])?;
    let mut remaining = size;
    while let Some(row) = rows.next()? {
        let index: i64 = row.get(0)?;
        let expected: String = row.get(1)?;
        let payload: Vec<u8> = row.get(2)?;
        require(
            remaining > 0
                && index == digests.len() as i64
                && payload.len() == remaining.min(CHUNK_BYTES),
            "RecoveryRequired",
            "staged artifact chunks disagree with the committed offset",
        )?;
        remaining -= payload.len();
        require(
            sha256(&payload) == expected,
            "RecoveryRequired",
            "staged artifact chunk digest mismatch",
        )?;
        content.update(&payload);
        digests.push(expected);
    }
    require(
        size - remaining == received,
        "RecoveryRequired",
        "staged artifact offset disagrees with its chunks",
    )?;
    Ok((format!("sha256:{:x}", content.finalize()), digests))
}

pub(crate) fn publish(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let p: StageId = decode(params)?;
    publish_stage(db, actor, &p.stage_id, None)
}

pub(crate) fn publish_generated(
    db: &Db<'_>,
    actor: &str,
    bytes: &[u8],
    media: &str,
    provenance: Value,
) -> Result<Value> {
    let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    publish_generated_reader(db, actor, bytes, size, &sha256(bytes), media, provenance)
}

pub(crate) fn publish_generated_reader(
    db: &Db<'_>,
    actor: &str,
    mut bytes: impl Read,
    size: u32,
    digest: &str,
    media: &str,
    provenance: Value,
) -> Result<Value> {
    require(
        (1..=MAX_BYTES).contains(&size),
        "QuotaExceeded",
        "generated content is outside the artifact ingress profile",
    )?;
    let mut stage = generated(db, actor, size, digest, media, MAX_BYTES)?;
    let mut buffer = vec![0; (size as usize).min(CHUNK_BYTES)];
    while stage.received < size {
        let count = ((size - stage.received) as usize).min(buffer.len());
        bytes.read_exact(&mut buffer[..count])?;
        append_chunk(db, &mut stage, &buffer[..count])?;
    }
    let mut extra = [0; 1];
    require(
        bytes.read(&mut extra)? == 0,
        "PreconditionFailed",
        "generated artifact exceeds its declared size",
    )?;
    publish_stage(db, actor, &stage.id, Some(provenance))
}

fn generated(
    db: &Db<'_>,
    actor: &str,
    size: u32,
    digest: &str,
    media: &str,
    maximum: u32,
) -> Result<Stage> {
    authorize(db, actor, UPLOAD)?;
    begin_stage(
        db,
        actor,
        Start {
            key: id("generated-output"),
            size_bytes: size,
            digest: digest.to_owned(),
            declared_media_type: media.to_owned(),
            ttl_seconds: 120,
            delegation_ref: None,
        },
        maximum,
    )
}

pub(crate) fn publish_managed_oci(
    db: &Db<'_>,
    actor: &str,
    artifact_id: &str,
    archive: Sealed,
    media: &str,
    source: Value,
) -> Result<Value> {
    require(
        source["kind"] == "LocalOciImageCapture",
        "RecoveryRequired",
        "managed payload has wrong source",
    )?;
    super::files::check_capacity(db)?;
    let stage = generated(
        db,
        actor,
        archive.size(),
        archive.digest(),
        media,
        db.cfg.oci_capture_max_bytes,
    )?;
    require_open(db, &stage)?;
    let artifact = create_artifact(
        db,
        actor,
        &stage,
        artifact_id,
        super::files::LAYOUT,
        archive.chunks(),
        source,
    )?;
    archive.publish(&artifact.id)?;
    let value = finish_published(db, actor, stage, &artifact)?;
    db.c.execute(
        "INSERT INTO oci_archive_blobs(artifact) VALUES(?1)",
        [&artifact.id],
    )?;
    Ok(value)
}

fn publish_stage(db: &Db<'_>, actor: &str, stage_id: &str, source: Option<Value>) -> Result<Value> {
    let stage = owned(db, actor, stage_id)?;
    if stage.artifact.is_some() {
        require(
            source.is_none(),
            "Conflict",
            "generated artifact stage is already published",
        )?;
        return response(db, &stage);
    }
    require_open(db, &stage)?;
    authorize_stage(db, actor, &stage.id)?;
    require(
        stage.received == stage.size,
        "PreconditionFailed",
        "artifact upload is incomplete",
    )?;
    db.cfg.check_admission_space()?;
    let (digest, digests) = scan_chunks(
        db.c,
        &stage.id,
        stage.size as usize,
        stage.received as usize,
    )?;
    require(
        digest == stage.digest,
        "PreconditionFailed",
        "uploaded bytes do not match the declared artifact digest",
    )?;
    authorize(db, actor, UPLOAD)?;
    require_open(db, &stage)?;
    authorize_stage(db, actor, &stage.id)?;
    let source = source.unwrap_or_else(|| json!({"kind":"LocalSubmission"}));
    let artifact = create_artifact(
        db,
        actor,
        &stage,
        &id("artifact"),
        RAW_LAYOUT,
        &digests,
        source,
    )?;
    let copied = db.c.execute(
        "INSERT INTO artifact_chunks(artifact_id,artifact_rev,chunk_index,payload)
         SELECT ?2,?3,chunk_index,payload FROM artifact_upload_chunks WHERE upload_id=?1",
        params![stage.id, artifact.id, artifact.rev],
    )?;
    require(
        copied == digests.len(),
        "RecoveryRequired",
        "artifact chunk publication count changed",
    )?;
    finish_published(db, actor, stage, &artifact)
}

fn create_artifact(
    db: &Db<'_>,
    actor: &str,
    stage: &Stage,
    artifact_id: &str,
    layout: &str,
    digests: &[String],
    mut source: Value,
) -> Result<Doc> {
    source["principal_id"] = json!(actor);
    source["stage_id"] = json!(stage.id);
    require(
        source["kind"].is_string(),
        "RecoveryRequired",
        "generated artifact source has no provenance kind",
    )?;
    db.create(
        "Artifact",
        artifact_id,
        actor,
        json!({
            "content_layout":layout,"media_type":"application/octet-stream",
            "declared_media_type":stage.declared_media_type,"size_bytes":stage.size.to_string(),
            "digest":stage.digest,"source":source,
        }),
        json!({"chunk_digests":digests}),
    )
}

fn finish_published(db: &Db<'_>, actor: &str, mut stage: Stage, artifact: &Doc) -> Result<Value> {
    let kind = artifact.spec["source"]["kind"].as_str();
    stage.artifact = Some(artifact.id.clone());
    db.c.execute(
        "UPDATE artifact_uploads SET state='Published',artifact_id=?2,source_kind=?3,
             received_bytes=size_bytes WHERE id=?1",
        params![stage.id, stage.artifact, kind],
    )?;
    db.c.execute(
        "DELETE FROM artifact_upload_chunks WHERE upload_id=?1",
        [&stage.id],
    )?;
    db.c.execute(
        "DELETE FROM authorization_deadlines WHERE resource=?1",
        [&stage.id],
    )?;
    db.audit(
        actor,
        "Artifact.Publish",
        stage.artifact.as_deref(),
        "Published",
    )?;
    stage.state = "Published".into();
    stage.received = stage.size;
    response(db, &stage)
}

fn close(db: &Db<'_>, id: &str, owner: &str, state: &str) -> Result<()> {
    db.c.execute(
        "UPDATE artifact_uploads SET state=?2 WHERE id=?1 AND state='Open'",
        params![id, state],
    )?;
    db.c.execute(
        "DELETE FROM artifact_upload_chunks WHERE upload_id=?1",
        [id],
    )?;
    db.c.execute(
        "DELETE FROM authorization_deadlines WHERE resource=?1",
        [id],
    )?;
    db.audit(owner, "Artifact.CloseStage", Some(id), state)
}

pub(crate) fn discard(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let p: StageId = decode(params)?;
    let mut stage = owned(db, actor, &p.stage_id)?;
    require(
        stage.artifact.is_none(),
        "PreconditionFailed",
        "a published artifact needs separate withdrawal",
    )?;
    if stage.state == "Open" {
        let expired = policy::deadline::Clock::sample()?.expired(db, &stage.id, &stage.expires)?;
        stage.state = if expired { "Expired" } else { "Discarded" }.into();
        close(db, &stage.id, actor, &stage.state)?;
    }
    response(db, &stage)
}

pub(crate) fn expire(db: &Db<'_>) -> Result<()> {
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_uploads WHERE state='Open')",
        [],
        |r| r.get(0),
    )?;
    if !pending {
        return Ok(());
    }
    let clock = policy::deadline::Clock::sample()?;
    let due =
        db.c.prepare_cached(
            "SELECT u.id,u.owner FROM artifact_uploads u
         LEFT JOIN authorization_deadlines d ON d.resource=u.id AND d.boot_id=?1
         WHERE u.state='Open' AND (julianday(u.expires_at)<=julianday('now')
                 OR d.time_ns<>?2 OR d.deadline_boot_ms<=?3)
         ORDER BY u.expires_at,u.id LIMIT 8",
        )?
        .query_map(
            params![clock.boot.boot_id, clock.time_ns, clock.boot.millis],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, owner) in due {
        close(db, &id, &owner, "Expired")?;
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let oversized: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_uploads u WHERE u.size_bytes>?1 AND NOT EXISTS(
            SELECT 1 FROM versions v WHERE v.id=u.artifact_id AND v.rev=1 AND v.kind='Artifact'
            AND v.spec->>'content_layout'=?2 AND v.spec->'source'->>'kind'='LocalOciImageCapture'))",
        params![MAX_BYTES, super::files::LAYOUT],
        |row| row.get(0),
    )?;
    require(
        !oversized,
        "RecoveryRequired",
        "oversized upload is outside the managed OCI archive profile",
    )?;
    let mut open = conn
        .prepare("SELECT id,size_bytes,received_bytes FROM artifact_uploads WHERE state='Open'")?;
    let mut expected_chunks = 0i64;
    for upload in open.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, u32>(1)?,
            r.get::<_, u32>(2)?,
        ))
    })? {
        let (id, size, received) = upload?;
        let (_, digests) = scan_chunks(conn, &id, size as usize, received as usize)?;
        expected_chunks += digests.len() as i64;
    }
    let stored: i64 = conn.query_row("SELECT count(*) FROM artifact_upload_chunks", [], |r| {
        r.get(0)
    })?;
    require(
        stored == expected_chunks,
        "RecoveryRequired",
        "closed stage retains unpublished chunks",
    )?;
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_uploads u
           WHERE u.artifact_id IS NOT NULL AND NOT EXISTS(
             SELECT 1 FROM versions v WHERE v.id=u.artifact_id AND v.rev=1 AND v.kind='Artifact'
              AND v.owner=u.owner AND (v.spec->>'content_layout'=?1
                   OR (v.spec->>'content_layout'=?2 AND v.spec->'source'->>'kind'='LocalOciImageCapture'))
              AND v.spec->>'digest'=u.digest
              AND v.spec->'source'->>'stage_id'=u.id))",
        [RAW_LAYOUT, super::files::LAYOUT],
        |r| r.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "upload receipt does not match a published artifact",
    )
}

fn withdrawal_id(artifact: &Doc) -> String {
    format!("withdrawal-{}", artifact.id)
}

pub(super) fn retention(conn: &Connection, artifact: &Doc) -> Result<Option<Doc>> {
    let recorded: Option<(String, String)> = conn
        .query_row(
            "SELECT owner,state FROM artifact_uploads WHERE artifact_id=?1",
            [&artifact.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (owner, state) =
        recorded.ok_or_else(|| err("RecoveryRequired", "uploaded artifact provenance missing"))?;
    require(
        artifact.owner == owner,
        "RecoveryRequired",
        "artifact owner disagrees with its publication",
    )?;
    let receipt = find(conn, &withdrawal_id(artifact))?;
    require(
        match (state.as_str(), &receipt) {
            ("Published", None) => true,
            ("Withdrawn", Some(doc)) => {
                matches!(doc.kind.as_str(), WITHDRAWAL | ABANDONMENT)
                    && doc.rev == 1
                    && doc.owner == owner
                    && doc.body["artifact_ref"] == artifact.reference()
                    && (doc.kind != ABANDONMENT
                        || doc.body["damage"]["artifact_ref"] == artifact.reference())
            }
            _ => false,
        },
        "RecoveryRequired",
        "artifact retention disagrees with its publication",
    )?;
    Ok(receipt)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Withdraw {
    reference: Value,
    reason: String,
}

pub(crate) fn withdraw(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    release_upload(db, actor, params, WITHDRAWAL)
}

pub(crate) fn abandon(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    release_upload(db, actor, params, ABANDONMENT)
}

fn release_view(receipt: &Doc, artifact: &Value) -> Value {
    if receipt.kind == ABANDONMENT {
        json!({"abandonment_ref":receipt.reference(),"artifact_ref":artifact})
    } else {
        json!({"withdrawal_ref":receipt.reference(),"artifact_ref":artifact})
    }
}

fn release_upload(db: &Db<'_>, actor: &str, params: &Value, kind: &str) -> Result<Value> {
    authorize(db, actor, WITHDRAW)?;
    let p: Withdraw = decode(params)?;
    require(
        !p.reason.trim().is_empty() && p.reason.len() <= 256,
        "InvalidArgument",
        "a bounded reason for withdrawal is required",
    )?;
    let artifact = db.resolve(&p.reference)?;
    policy::owned(actor, &artifact)?;
    require(
        super::is_raw(&artifact),
        "InvalidArgument",
        "expected a published local upload",
    )?;
    if let Some(receipt) = retention(db.c, &artifact)? {
        require(
            receipt.kind == kind && receipt.body["reason"] == p.reason,
            "IdempotencyConflict",
            "artifact release already recorded with another request",
        )?;
        return Ok(release_view(&receipt, &p.reference));
    }
    require(
        !crate::apps::artifact_install::pinned(db, &artifact.id)?,
        "Conflict",
        "artifact is retained by an installation or an immutable release",
    )?;
    require(
        !crate::agent::session_inputs::retains(db, &artifact.id)?,
        "Conflict",
        "artifact is retained by an active Agent session or accepted launch Task",
    )?;
    let manifest = super::Manifest::read(&artifact)?
        .ok_or_else(|| err("RecoveryRequired", "artifact manifest unavailable"))?;
    let damage = if kind == ABANDONMENT {
        require(
            artifact.spec["content_layout"] == RAW_LAYOUT
                && artifact.spec["source"]["kind"] == "LocalSubmission",
            "UnsupportedCapability",
            "abandonment requires a local SQLite upload",
        )?;
        require(
            manifest.size <= MAX_BYTES as usize,
            "QuotaExceeded",
            "upload exceeds the abandonment verification limit",
        )?;
        let inventory = abandonment::inventory(db.c, &manifest)?;
        let evidence = inventory
            .damage
            .ok_or_else(|| err("PreconditionFailed", "no upload damage remains to abandon"))?;
        Some((inventory.count, inventory.bytes, evidence))
    } else {
        None
    };
    let expected = damage.as_ref().map_or_else(
        || {
            if super::files::managed(&artifact) {
                0
            } else {
                manifest.chunks.len()
            }
        },
        |(count, _, _)| *count,
    );
    let count = db.c.execute(
        "DELETE FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2",
        params![artifact.id, artifact.rev],
    )?;
    require(
        count == expected,
        "RecoveryRequired",
        "uploaded artifact chunks are incomplete",
    )?;
    db.c.execute(
        "UPDATE artifact_uploads SET state='Withdrawn' WHERE artifact_id=?1",
        [&artifact.id],
    )?;
    let (body, method, decision) = if let Some((_, bytes, evidence)) = damage {
        (
            json!({"artifact_ref":p.reference,"reason":p.reason,"abandoned_at":now(),"abandoned_by":actor,
            "damage":evidence,"content_bytes_removed":bytes.to_string(),"logical_bytes_released":manifest.size.to_string()}),
            "Artifact.AbandonUpload",
            "Abandoned",
        )
    } else {
        (
            json!({"artifact_ref":p.reference,"reason":p.reason,"withdrawn_at":now(),"withdrawn_by":actor}),
            "Artifact.Withdraw",
            "Withdrawn",
        )
    };
    let receipt = db.create(
        kind,
        &withdrawal_id(&artifact),
        &artifact.owner,
        json!({"artifact_id":artifact.id}),
        body,
    )?;
    db.audit(actor, method, Some(&artifact.id), decision)?;
    Ok(release_view(&receipt, &p.reference))
}
