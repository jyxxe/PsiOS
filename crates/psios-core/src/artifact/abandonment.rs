//! Inventory remaining SQLite payload rows under an immutable artifact manifest.

use super::{Manifest, files, ingress::valid_digest};
use crate::{Result, err, require};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::chat::CAPACITY_FORMAT + 1;
pub(super) const MAX_BYTES: usize = 64 * 1024 * 1024;

pub(super) struct ChunkInventory {
    pub(super) count: usize,
    pub(super) bytes: u64,
    pub(super) damage: Option<Value>,
}

pub(super) fn inventory(conn: &Connection, manifest: &Manifest<'_>) -> Result<ChunkInventory> {
    let artifact = manifest.artifact;
    require(
        !files::managed(artifact),
        "UnsupportedCapability",
        "file-backed artifact payloads require separate recovery",
    )?;
    require(
        valid_digest(manifest.digest)
            && manifest
                .chunks
                .iter()
                .all(|chunk| chunk.as_str().is_some_and(valid_digest)),
        "RecoveryRequired",
        "artifact manifest has invalid digest evidence",
    )?;
    let (count, bytes, outside_manifest): (i64, i64, bool) = conn.query_row(
        "SELECT count(*),coalesce(sum(length(payload)),0),
                count(*) FILTER (WHERE chunk_index<0 OR chunk_index>=?3)>0
         FROM artifact_chunks WHERE artifact_id=?1 AND artifact_rev=?2",
        params![artifact.id, artifact.rev, manifest.chunks.len() as i64],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    require(
        !outside_manifest,
        "RecoveryRequired",
        "artifact has payload rows outside its immutable manifest",
    )?;
    let count = usize::try_from(count)
        .map_err(|_| err("RecoveryRequired", "invalid surviving artifact chunk count"))?;
    let bytes = u64::try_from(bytes)
        .map_err(|_| err("RecoveryRequired", "invalid surviving artifact byte count"))?;
    let damage = match manifest.verify(conn) {
        Ok(()) => None,
        Err(error) if error.0.code == "RecoveryRequired" => Some(json!({
            "artifact_ref": artifact.reference(),
            "condition": if count < manifest.chunks.len() { "MissingChunks" } else { "IntegrityMismatch" },
            "expected_chunks": manifest.chunks.len(),
            "surviving_chunks": count,
            "surviving_bytes": bytes.to_string(),
        })),
        Err(error) => return Err(error),
    };
    Ok(ChunkInventory {
        count,
        bytes,
        damage,
    })
}
