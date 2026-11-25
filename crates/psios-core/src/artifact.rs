use crate::{
    Error, Result,
    db::{Db, Doc, row},
    err, require,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::{canonical, id, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) mod availability;
pub(crate) mod capacity;
pub(crate) mod ingress;
pub(crate) mod repair;
pub(crate) mod retention;

pub const FORMAT: u32 = 30;
const LAYOUT: &str = "ChunkedJsonV1";
const RAW_LAYOUT: &str = "ChunkedBytesV1";
// The chunk size is part of the on-disk format.
const CHUNK_BYTES: usize = 128 * 1024;

pub(crate) fn is_content_unavailable(code: &str) -> bool {
    matches!(code, "ResultEvicted" | "ResultUnavailable")
}

pub(crate) fn require_task_result(result: &Value) -> Result<()> {
    match result["availability"].as_str() {
        Some("Evicted") => Err(err("ResultEvicted", "required task result was evicted")),
        Some("Unavailable") => Err(err(
            "ResultUnavailable",
            "required task result is unavailable",
        )),
        _ => Ok(()),
    }
}

pub(crate) fn dependency_error(error: Error) -> Error {
    if matches!(
        error.0.code.as_str(),
        "PreconditionFailed" | "RecoveryRequired"
    ) {
        err(
            "ResultUnavailable",
            "required artifact is unavailable or failed content verification",
        )
    } else {
        error
    }
}

pub fn stored_digest(artifact: &Doc) -> Option<&Value> {
    (is_json(artifact) || is_raw(artifact)).then(|| &artifact.spec["digest"])
}

pub(crate) fn is_json(artifact: &Doc) -> bool {
    artifact.kind == "Artifact" && artifact.spec["content_layout"] == LAYOUT
}

pub(crate) fn is_raw(artifact: &Doc) -> bool {
    artifact.kind == "Artifact" && artifact.spec["content_layout"] == RAW_LAYOUT
}

pub(crate) fn is_task_output(artifact: &Doc, task: &Doc) -> bool {
    artifact.kind == "Artifact"
        && artifact.owner == task.owner
        && artifact.spec["task_id"] == task.id
        && artifact.spec["run_id"] == task.body["current_run_id"]
}

pub fn publish(db: &Db<'_>, owner: &str, mut spec: Value, value: &Value) -> Result<Doc> {
    // The broker publishes the manifest and payload in its settlement transaction.
    let bytes = serde_json::to_vec(value)?;
    spec["content_layout"] = json!(LAYOUT);
    spec["media_type"] = json!("application/json");
    spec["size_bytes"] = json!(bytes.len().to_string());
    spec["digest"] = json!(sha256(&bytes));
    let digests: Vec<_> = bytes.chunks(CHUNK_BYTES).map(sha256).collect();
    let artifact = db.create(
        "Artifact",
        &id("artifact"),
        owner,
        spec,
        json!({"chunk_digests": digests}),
    )?;
    let mut insert = db.c.prepare_cached(
        "INSERT INTO artifact_chunks(artifact_id,artifact_rev,chunk_index,payload)
         VALUES(?1,?2,?3,?4)",
    )?;
    for (index, payload) in bytes.chunks(CHUNK_BYTES).enumerate() {
        insert.execute(params![artifact.id, artifact.rev, index as i64, payload])?;
    }
    Ok(artifact)
}

struct Manifest<'a> {
    artifact: &'a Doc,
    size: usize,
    digest: &'a str,
    chunks: &'a [Value],
}

impl<'a> Manifest<'a> {
    fn read(artifact: &'a Doc) -> Result<Option<Self>> {
        let Some(digest) = stored_digest(artifact) else {
            return Ok(None);
        };
        let invalid = || err("RecoveryRequired", "invalid artifact content manifest");
        let size: usize = artifact.spec["size_bytes"]
            .as_str()
            .and_then(|size| size.parse().ok())
            .filter(|size| *size != 0)
            .ok_or_else(invalid)?;
        let chunks = artifact.body["chunk_digests"]
            .as_array()
            .filter(|chunks| chunks.len() == size.div_ceil(CHUNK_BYTES))
            .ok_or_else(invalid)?;
        Ok(Some(Self {
            artifact,
            size,
            digest: digest.as_str().ok_or_else(invalid)?,
            chunks,
        }))
    }

    fn chunk(&self, conn: &Connection, index: usize) -> Result<Vec<u8>> {
        let expected_size = (self.size - index * CHUNK_BYTES).min(CHUNK_BYTES);
        let bytes: Vec<u8> = conn
            .prepare_cached(
                "SELECT payload FROM artifact_chunks
                 WHERE artifact_id=?1 AND artifact_rev=?2 AND chunk_index=?3 AND length(payload)=?4",
            )?
            .query_row(
                params![
                    self.artifact.id,
                    self.artifact.rev,
                    index as i64,
                    expected_size as i64
                ],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| err("RecoveryRequired", "artifact chunk is missing or has changed size"))?;
        require(
            self.chunks[index] == sha256(&bytes),
            "RecoveryRequired",
            "artifact chunk digest mismatch",
        )?;
        Ok(bytes)
    }

    fn verify(&self, conn: &Connection) -> Result<()> {
        let mut digest = Sha256::new();
        for index in 0..self.chunks.len() {
            digest.update(self.chunk(conn, index)?);
        }
        require(
            format!("sha256:{:x}", digest.finalize()) == self.digest,
            "RecoveryRequired",
            "artifact content digest mismatch",
        )
    }

    fn original_bytes(&self, conn: &Connection, max_bytes: Option<usize>) -> Result<Vec<u8>> {
        require(
            max_bytes.is_none_or(|limit| self.size <= limit),
            "QuotaExceeded",
            "artifact exceeds requested content byte limit",
        )?;
        let mut bytes = Vec::new();
        for index in 0..self.chunks.len() {
            bytes.extend(self.chunk(conn, index)?);
        }
        require(
            sha256(&bytes) == self.digest,
            "RecoveryRequired",
            "artifact content digest mismatch",
        )?;
        Ok(bytes)
    }
}

pub fn read(db: &Db<'_>, artifact: &Doc, max_bytes: Option<usize>) -> Result<Value> {
    retention::require_content(db.c, artifact)?;
    require(
        !is_raw(artifact),
        "UnsupportedCapability",
        "use Artifact.ReadChunk for opaque artifact bytes",
    )?;
    let Some(manifest) = Manifest::read(artifact)? else {
        return Ok(artifact.body.clone());
    };
    serde_json::from_slice(&manifest.original_bytes(db.c, max_bytes)?)
        .map_err(|_| err("RecoveryRequired", "artifact JSON is unreadable"))
}

pub(crate) fn checkpoint_bytes(
    conn: &Connection,
    artifact: &Doc,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    retention::require_content(conn, artifact)?;
    if let Some(manifest) = Manifest::read(artifact)? {
        return manifest.original_bytes(conn, Some(max_bytes));
    }
    let bytes = canonical(&artifact.body);
    require(
        bytes.len() <= max_bytes,
        "QuotaExceeded",
        "artifact exceeds requested content byte limit",
    )?;
    Ok(bytes)
}

pub fn page(db: &Db<'_>, artifact: &Doc, offset: usize, length: usize) -> Result<Value> {
    retention::require_content(db.c, artifact)?;
    let page_end = |size: usize| -> Result<usize> {
        require(
            offset <= size,
            "InvalidArgument",
            "artifact offset beyond end",
        )?;
        Ok(offset.saturating_add(length).min(size))
    };
    let (data, end, size) = if let Some(manifest) = Manifest::read(artifact)? {
        let end = page_end(manifest.size)?;
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let bytes = manifest.chunk(db.c, position / CHUNK_BYTES)?;
            let start = position % CHUNK_BYTES;
            let count = (bytes.len() - start).min(end - position);
            data.extend_from_slice(&bytes[start..start + count]);
            position += count;
        }
        (STANDARD.encode(data), end, manifest.size)
    } else {
        // Existing references pin their historical canonical byte stream.
        let bytes = canonical(&artifact.body);
        let end = page_end(bytes.len())?;
        (STANDARD.encode(&bytes[offset..end]), end, bytes.len())
    };
    Ok(json!({
        "offset": offset, "total_bytes": size, "data_base64": data,
        "next_offset": (end < size).then_some(end)
    }))
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT * FROM versions WHERE kind='Artifact' AND json_extract(spec,'$.content_layout') IN (?1,?2)",
    )?;
    let mut expected_chunks = 0;
    for artifact in stmt.query_map([LAYOUT, RAW_LAYOUT], row)? {
        let artifact = artifact?;
        if retention::receipt(conn, &artifact)?.is_some() {
            continue;
        }
        let manifest = Manifest::read(&artifact)?
            .ok_or_else(|| err("RecoveryRequired", "missing artifact content manifest"))?;
        manifest.verify(conn)?;
        expected_chunks += manifest.chunks.len() as i64;
    }
    let actual_chunks: i64 =
        conn.query_row("SELECT COUNT(*) FROM artifact_chunks", [], |r| r.get(0))?;
    require(
        actual_chunks == expected_chunks,
        "RecoveryRequired",
        "artifact storage contains unreferenced chunks",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store, policy};

    #[test]
    fn stored_json_preserves_large_integer_values_and_hashes_the_original_bytes() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config::local(&temporary.path().join("state"));
        Store::init(&config).unwrap();
        let mut store = Store::open(config).unwrap();
        store
            .transaction("publish-integer", |db| {
                let value = json!({"value": 9_007_199_254_740_993u64});
                let artifact = publish(db, policy::OWNER, json!({"task_id":"test-task"}), &value)?;
                let raw = br#"{"value":9007199254740993}"#;
                assert_eq!(artifact.reference()["digest"], sha256(raw));
                assert_eq!(read(db, &artifact, None)?, value);
                assert_eq!(
                    page(db, &artifact, 0, CHUNK_BYTES)?["data_base64"],
                    STANDARD.encode(raw)
                );
                verify(db.c)?;
                Ok(())
            })
            .unwrap();
    }
}
