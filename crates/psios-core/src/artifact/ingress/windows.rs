//! Prepared upload keys fence creation to one bounded authority clock window.
use super::{MAX_BYTES, Stage, Start, UPLOAD, authority, authorize, stage_row, validate_stage};
use crate::{Result, after, db::Db, decode, err, policy::deadline::Clock, require};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{ArtifactStagePreparation, PreparedArtifactStage, canonical};
use ring::hmac;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const FORMAT: u32 = crate::storage::ENTRY_CAPACITY_FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("windows.sql");
const PREFIX: &str = "psios-upload-v1-";
const DOMAIN_BYTES: usize = 16;
const TIME_END: usize = DOMAIN_BYTES + 8;
const TTL_END: usize = TIME_END + 2;
const HEADER_BYTES: usize = TTL_END + 16;
const KEY_BYTES: usize = HEADER_BYTES + 32;

fn clock_domain(db: &Db<'_>, clock: &Clock) -> [u8; DOMAIN_BYTES] {
    let input = canonical(&json!([db.incarnation, clock.boot.boot_id, clock.time_ns]));
    let mut domain = [0; DOMAIN_BYTES];
    domain.copy_from_slice(&Sha256::digest(input)[..DOMAIN_BYTES]);
    domain
}

fn signing_key(db: &Db<'_>) -> hmac::Key {
    let root = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
    let key = hmac::sign(&root, b"psios/artifact-upload-window/1");
    hmac::Key::new(hmac::HMAC_SHA256, key.as_ref())
}

fn signed_input(header: &[u8], actor: &str, request: &ArtifactStagePreparation) -> Vec<u8> {
    let mut bytes = header.to_vec();
    bytes.extend(canonical(&json!([actor, request])));
    bytes
}

pub(crate) fn prepare(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    authorize(db, actor, UPLOAD)?;
    let request: ArtifactStagePreparation = decode(input)?;
    validate_stage(
        request.size_bytes,
        &request.digest,
        &request.declared_media_type,
        request.ttl_seconds,
        MAX_BYTES,
    )?;
    if let Some(reference) = &request.delegation_ref {
        authority::authorize(db, actor, reference)?;
    }
    let clock = Clock::sample()?;
    let seconds = i64::from(request.ttl_seconds);
    let deadline = clock.boot.millis + seconds * 1000;
    let mut key = Vec::with_capacity(KEY_BYTES);
    key.extend(clock_domain(db, &clock));
    key.extend(deadline.to_be_bytes());
    key.extend((request.ttl_seconds as u16).to_be_bytes());
    key.extend(Uuid::new_v4().as_bytes());
    let tag = hmac::sign(&signing_key(db), &signed_input(&key, actor, &request));
    key.extend(tag.as_ref());
    Ok(json!(PreparedArtifactStage {
        key: format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(key)),
        expires_at: after(seconds)
    }))
}

pub(super) struct Window {
    encoded: [u8; KEY_BYTES],
    domain: [u8; DOMAIN_BYTES],
    deadline: i64,
    ttl_seconds: u32,
}

impl Window {
    fn parse(key: &str) -> Option<Self> {
        let text = key.strip_prefix(PREFIX)?;
        let encoded: [u8; KEY_BYTES] = URL_SAFE_NO_PAD.decode(text).ok()?.try_into().ok()?;
        if URL_SAFE_NO_PAD.encode(encoded) != text {
            return None;
        }
        let domain = encoded[..DOMAIN_BYTES].try_into().ok()?;
        let deadline = i64::from_be_bytes(encoded[DOMAIN_BYTES..TIME_END].try_into().ok()?);
        let ttl_seconds = u32::from(u16::from_be_bytes(
            encoded[TIME_END..TTL_END].try_into().ok()?,
        ));
        (deadline > 0 && (5..=3600).contains(&ttl_seconds)).then_some(Self {
            encoded,
            domain,
            deadline,
            ttl_seconds,
        })
    }

    fn authenticated(&self, db: &Db<'_>, actor: &str, start: &Start) -> bool {
        let (header, tag) = self.encoded.split_at(HEADER_BYTES);
        let request = start.preparation();
        self.ttl_seconds == request.ttl_seconds
            && hmac::verify(
                &signing_key(db),
                &signed_input(header, actor, &request),
                tag,
            )
            .is_ok()
    }

    fn elapsed(&self, domain: &[u8; DOMAIN_BYTES], boot_millis: i64) -> bool {
        &self.domain != domain || self.deadline <= boot_millis
    }

    pub(super) fn attach(&self, db: &Db<'_>, stage: &str) -> Result<()> {
        db.c.execute(
            "INSERT INTO artifact_stage_windows(stage_id,clock_domain,deadline_boot_ms) VALUES(?1,?2,?3)",
            params![stage, self.domain.as_slice(), self.deadline],
        )?;
        Ok(())
    }
}

pub(super) fn authorize_new(
    db: &Db<'_>,
    actor: &str,
    start: &Start,
    clock: &Clock,
) -> Result<Option<Window>> {
    if !start.key.starts_with(PREFIX) {
        return Ok(None);
    }
    let window = Window::parse(&start.key)
        .filter(|window| window.authenticated(db, actor, start))
        .ok_or_else(|| {
            err(
                "PermissionDenied",
                "prepared upload key does not authorize this request",
            )
        })?;
    require(
        !window.elapsed(&clock_domain(db, clock), clock.boot.millis),
        "DeadlineExceeded",
        "prepared upload window ended",
    )?;
    Ok(Some(window))
}

fn verified(conn: &Connection, stage: &str) -> Result<(Window, Start, String)> {
    let binding = authority::lookup(conn, stage)?;
    let (domain, deadline, actor, key, source, receipt): (Vec<u8>, i64, String, String, String, Stage) = conn.query_row(
        "SELECT w.clock_domain,w.deadline_boot_ms,u.*
         FROM artifact_stage_windows w JOIN artifact_uploads u ON u.id=w.stage_id WHERE w.stage_id=?1",
        [stage], |row| Ok((row.get(0)?, row.get(1)?, row.get("owner")?, row.get("request_key")?,
                         row.get("source_kind")?, stage_row(row)?)),
    )?;
    let window = Window::parse(&key)
        .ok_or_else(|| err("RecoveryRequired", "stored prepared upload key is invalid"))?;
    let start = Start {
        key,
        size_bytes: receipt.size,
        digest: receipt.digest,
        declared_media_type: receipt.declared_media_type,
        ttl_seconds: window.ttl_seconds,
        delegation_ref: binding.map(|binding| binding.delegation_ref),
    };
    require(
        domain == window.domain
            && deadline == window.deadline
            && source == "LocalSubmission"
            && matches!(receipt.state.as_str(), "Open" | "Discarded" | "Expired")
            && start.request_digest()? == receipt.request_digest,
        "RecoveryRequired",
        "prepared upload window disagrees with its stage receipt",
    )?;
    Ok((window, start, actor))
}

pub(crate) fn reclaim(db: &Db<'_>) -> Result<()> {
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_stage_windows)",
        [],
        |row| row.get(0),
    )?;
    if !pending {
        return Ok(());
    }
    let clock = Clock::sample()?;
    let domain = clock_domain(db, &clock);
    let candidates = db.c.prepare_cached(
        "SELECT w.stage_id FROM artifact_stage_windows w JOIN artifact_uploads u ON u.id=w.stage_id
         WHERE u.state IN ('Discarded','Expired') AND (w.clock_domain<>?1 OR w.deadline_boot_ms<=?2)
         ORDER BY w.deadline_boot_ms,w.stage_id LIMIT 8",
    )?.query_map(params![domain.as_slice(), clock.boot.millis], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for stage in candidates {
        let (window, start, actor) = verified(db.c, &stage)?;
        require(
            window.authenticated(db, &actor, &start) && window.elapsed(&domain, clock.boot.millis),
            "RecoveryRequired",
            "prepared upload reclamation has no valid expired key",
        )?;
        db.c.execute(
            "DELETE FROM artifact_upload_authority WHERE stage_id=?1",
            [&stage],
        )?;
        db.c.execute(
            "DELETE FROM artifact_stage_windows WHERE stage_id=?1",
            [&stage],
        )?;
        let removed = db.c.execute(
            "DELETE FROM artifact_uploads WHERE id=?1 AND state IN ('Discarded','Expired')",
            [&stage],
        )?;
        require(
            removed == 1,
            "RecoveryRequired",
            "expired prepared upload receipt changed",
        )?;
        db.audit(
            crate::policy::SYSTEM,
            "Artifact.Stage.Reclaim",
            Some(&stage),
            "ExpiredPreparation",
        )?;
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut windows = conn.prepare("SELECT stage_id FROM artifact_stage_windows")?;
    for stage in windows.query_map([], |row| row.get::<_, String>(0))? {
        verified(conn, &stage?)?;
    }
    Ok(())
}
