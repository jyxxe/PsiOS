//! Keep a resumable upload fenced to its original Node-scoped authority.
use super::{MAX_BYTES, TOTAL_STAGES, UPLOAD, close, valid_digest, valid_media_type};
use crate::{Result, db::Db, err, policy, require};
use psios_api::{ResourceRef, canonical, sha256};
use rusqlite::{Connection, OptionalExtension, Row};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = 97;
pub(crate) const SCHEMA: &str = include_str!("authority.sql");
pub(super) const STAGE_PREFIX: &str = "artifact-stage-bound";
const REVOKED: &str = "AuthorityRevoked";
const CLOSE_BATCH: u32 = 8;

pub(super) struct Binding {
    pub(super) delegation_ref: ResourceRef,
    pub(super) close_reason: Option<String>,
}

struct Record {
    owner: String,
    request_key: String,
    request_digest: String,
    digest: String,
    media_type: String,
    size_bytes: u32,
    state: String,
    source: String,
    reference: Option<Value>,
    ttl_seconds: Option<u32>,
    close_reason: Option<String>,
}

fn reserved_id(stage_id: &str) -> bool {
    stage_id
        .strip_prefix(STAGE_PREFIX)
        .is_some_and(|suffix| suffix.starts_with('-'))
}

fn record(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        owner: row.get("owner")?,
        request_key: row.get("request_key")?,
        request_digest: row.get("request_digest")?,
        digest: row.get("digest")?,
        media_type: row.get("declared_media_type")?,
        size_bytes: row.get("size_bytes")?,
        state: row.get("state")?,
        source: row.get("source_kind")?,
        reference: row.get("delegation_ref")?,
        ttl_seconds: row.get("ttl_seconds")?,
        close_reason: row.get("close_reason")?,
    })
}

fn get(conn: &Connection, stage_id: &str) -> Result<Record> {
    conn.prepare_cached(
        "SELECT u.owner,u.request_key,u.request_digest,u.digest,u.declared_media_type,
                u.size_bytes,u.state,u.source_kind,b.delegation_ref,b.ttl_seconds,b.close_reason
           FROM artifact_uploads u LEFT JOIN artifact_upload_authority b ON b.stage_id=u.id
          WHERE u.id=?1",
    )?
    .query_row([stage_id], record)
    .optional()?
    .ok_or_else(|| err("RecoveryRequired", "upload authority has no matching stage"))
}

impl Record {
    fn binding(&self, conn: &Connection, stage_id: &str) -> Result<Option<Binding>> {
        require(
            reserved_id(stage_id) == self.reference.is_some(),
            "RecoveryRequired",
            "artifact stage and its authority binding disagree",
        )?;
        let Some(reference) = &self.reference else {
            return Ok(None);
        };
        let delegation_ref: ResourceRef =
            serde_json::from_value(reference.clone()).map_err(|_| {
                err(
                    "RecoveryRequired",
                    "invalid original upload delegation reference",
                )
            })?;
        let ttl = self
            .ttl_seconds
            .filter(|ttl| (5..=3600).contains(ttl))
            .ok_or_else(|| err("RecoveryRequired", "invalid bound upload lifetime"))?;
        require(
            self.source == "LocalSubmission"
                && (1..=MAX_BYTES).contains(&self.size_bytes)
                && valid_digest(&self.digest)
                && valid_media_type(&self.media_type)
                && self
                    .close_reason
                    .as_deref()
                    .is_none_or(|reason| reason == REVOKED && self.state == "Discarded"),
            "RecoveryRequired",
            "bound artifact stage has invalid source or closure metadata",
        )?;
        let input = json!({
            "key":self.request_key,"size_bytes":self.size_bytes,"digest":self.digest,
            "declared_media_type":self.media_type,"ttl_seconds":ttl,"delegation_ref":delegation_ref,
        });
        require(
            sha256(&canonical(&input)) == self.request_digest,
            "RecoveryRequired",
            "bound artifact stage request digest disagrees with its authority",
        )?;
        let original = crate::db::resolve(conn, reference).map_err(|error| {
            if matches!(
                error.0.code.as_str(),
                "PreconditionFailed" | "InvalidArgument"
            ) {
                err(
                    "RecoveryRequired",
                    "original upload delegation is unavailable",
                )
            } else {
                error
            }
        })?;
        require(
            original.kind == "Delegation"
                && original.owner == self.owner
                && original.body["lifecycle"] == "Active"
                && original.body["actions"]
                    .as_array()
                    .is_some_and(|actions| actions.iter().any(|action| action == UPLOAD)),
            "RecoveryRequired",
            "original upload delegation disagrees with stage ownership or action",
        )?;
        Ok(Some(Binding {
            delegation_ref,
            close_reason: self.close_reason.clone(),
        }))
    }
}

pub(super) fn lookup(conn: &Connection, stage_id: &str) -> Result<Option<Binding>> {
    get(conn, stage_id)?.binding(conn, stage_id)
}

pub(super) fn authorize(db: &Db<'_>, actor: &str, reference: &ResourceRef) -> Result<()> {
    policy::active(db, actor)?;
    let fixed = json!(reference);
    let current = db.find(&reference.resource_id)?;
    require(
        current.is_some_and(|delegation| {
            delegation.kind == "Delegation"
                && delegation.owner == actor
                && delegation.reference() == fixed
        }),
        "PermissionDenied",
        "artifact upload requires its original active delegation",
    )?;
    policy::delegation(db, actor, &reference.resource_id, UPLOAD)?;
    policy::tool_scope(db, &fixed, &db.meta("authority")?)
}

pub(super) fn attach(
    db: &Db<'_>,
    stage_id: &str,
    reference: &ResourceRef,
    ttl_seconds: u32,
) -> Result<()> {
    require_transaction(db)?;
    require(
        reserved_id(stage_id),
        "RecoveryRequired",
        "bound upload needs a reserved stage identity",
    )?;
    db.c.execute(
        "INSERT INTO artifact_upload_authority(stage_id,delegation_ref,ttl_seconds) VALUES(?1,?2,?3)",
        rusqlite::params![stage_id, json!(reference), ttl_seconds],
    )?;
    let stage = get(db.c, stage_id)?;
    require(
        stage.state == "Open",
        "RecoveryRequired",
        "new upload authority belongs to a closed stage",
    )?;
    stage.binding(db.c, stage_id)?;
    authorize(db, &stage.owner, reference)
}

fn require_transaction(db: &Db<'_>) -> Result<()> {
    require(
        !db.c.is_autocommit(),
        "RecoveryRequired",
        "artifact upload authority must change in a state transaction",
    )
}

pub(crate) fn reclaim(db: &Db<'_>) -> Result<()> {
    require_transaction(db)?;
    let ids =
        db.c.prepare_cached(
            "SELECT id FROM artifact_uploads INDEXED BY artifact_open_uploads WHERE state='Open'
          ORDER BY expires_at,id LIMIT ?1",
        )?
        .query_map([TOTAL_STAGES + 1], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    require(
        ids.len() <= TOTAL_STAGES as usize,
        "RecoveryRequired",
        "artifact stage count exceeds its node limit",
    )?;
    let mut count = 0;
    for id in ids {
        let stage = get(db.c, &id)?;
        let Some(binding) = stage.binding(db.c, &id)? else {
            continue;
        };
        match authorize(db, &stage.owner, &binding.delegation_ref) {
            Ok(()) => continue,
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
        close(db, &id, &stage.owner, "Discarded")?;
        let changed = db.c.execute(
            "UPDATE artifact_upload_authority SET close_reason='AuthorityRevoked'
              WHERE stage_id=?1 AND close_reason IS NULL
                AND EXISTS(SELECT 1 FROM artifact_uploads WHERE id=?1 AND state='Discarded')
                AND NOT EXISTS(SELECT 1 FROM artifact_upload_chunks WHERE upload_id=?1)
                AND NOT EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=?1)",
            [&id],
        )?;
        require(
            changed == 1,
            "RecoveryRequired",
            "revoked upload did not close with its staging bytes and deadline released",
        )?;
        count += 1;
        if count == CLOSE_BATCH {
            break;
        }
    }
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id FROM artifact_uploads WHERE id GLOB 'artifact-stage-bound-*'
          UNION SELECT stage_id FROM artifact_upload_authority",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let stage_id: String = row.get(0)?;
        lookup(conn, &stage_id)?;
    }
    let residual: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artifact_upload_authority b WHERE b.close_reason IS NOT NULL
            AND (EXISTS(SELECT 1 FROM artifact_upload_chunks WHERE upload_id=b.stage_id)
              OR EXISTS(SELECT 1 FROM authorization_deadlines WHERE resource=b.stage_id)))",
        [],
        |row| row.get(0),
    )?;
    require(
        !residual,
        "RecoveryRequired",
        "revoked upload still retains staged bytes or a deadline",
    )
}
