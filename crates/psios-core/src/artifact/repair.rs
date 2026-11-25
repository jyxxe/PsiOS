use super::{CHUNK_BYTES, Manifest, retention};
use crate::{Result, db::Db, decode, err, policy, require};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::sha256;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repair {
    reference: Value,
    offset: usize,
    data_base64: String,
}

pub fn chunk(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Repair = decode(params)?;
    let artifact = db.resolve(&p.reference)?;
    require(
        artifact.kind == "Artifact",
        "InvalidArgument",
        "expected result artifact",
    )?;
    retention::require_content(db.c, &artifact)?;
    let manifest = Manifest::read(&artifact)?.ok_or_else(|| {
        err(
            "UnsupportedCapability",
            "legacy inline artifacts have no separately repairable content",
        )
    })?;
    require(
        p.offset < manifest.size && p.offset.is_multiple_of(CHUNK_BYTES),
        "InvalidArgument",
        "repair offset must identify a stored chunk",
    )?;
    let expected = (manifest.size - p.offset).min(CHUNK_BYTES);
    require(
        p.data_base64.len() == 4 * expected.div_ceil(3),
        "InvalidArgument",
        "repair must supply one complete chunk",
    )?;
    let payload = STANDARD
        .decode(p.data_base64)
        .map_err(|_| err("InvalidArgument", "invalid artifact chunk encoding"))?;
    let index = p.offset / CHUNK_BYTES;
    require(
        payload.len() == expected && manifest.chunks[index] == sha256(&payload),
        "PreconditionFailed",
        "repair content differs from the original artifact chunk",
    )?;
    let replaced = db.c.execute(
        "INSERT INTO artifact_chunks(artifact_id,artifact_rev,chunk_index,payload)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(artifact_id,artifact_rev,chunk_index) DO UPDATE SET payload=excluded.payload
         WHERE artifact_chunks.payload != excluded.payload",
        params![artifact.id, artifact.rev, index as i64, payload],
    )? != 0;
    if replaced {
        db.audit(
            actor,
            "Artifact.RepairChunk",
            Some(&artifact.id),
            "Repaired",
        )?;
    }
    Ok(json!({"reference":p.reference,"offset":p.offset,"repaired":replaced}))
}
