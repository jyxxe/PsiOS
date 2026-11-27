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
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::FileExt,
    path::Path,
};

pub(crate) mod abandonment;
pub(crate) mod availability;
pub(crate) mod capacity;
pub(crate) mod files;
pub(crate) mod ingress;
pub(crate) mod repair;
pub(crate) mod retention;
pub(crate) mod session_output;

pub const FORMAT: u32 = 30;
const LAYOUT: &str = "ChunkedJsonV1";
const RAW_LAYOUT: &str = "ChunkedBytesV1";
// The chunk size is part of the on-disk format.
pub(crate) const CHUNK_BYTES: usize = 128 * 1024;

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
    artifact.kind == "Artifact"
        && matches!(
            artifact.spec["content_layout"].as_str(),
            Some(RAW_LAYOUT | files::LAYOUT)
        )
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
        self.payload_chunk(conn, None, index)
    }

    fn payload_chunk(
        &self,
        conn: &Connection,
        file: Option<&File>,
        index: usize,
    ) -> Result<Vec<u8>> {
        let expected_size = (self.size - index * CHUNK_BYTES).min(CHUNK_BYTES);
        let bytes: Vec<u8> = if let Some(file) = file {
            let mut bytes = vec![0; expected_size];
            file.read_exact_at(&mut bytes, (index * CHUNK_BYTES) as u64)
                .map_err(|_| {
                    err(
                        "RecoveryRequired",
                        "managed OCI archive chunk is unreadable",
                    )
                })?;
            bytes
        } else {
            conn.prepare_cached(
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
            .ok_or_else(|| err("RecoveryRequired", "artifact chunk is missing or has changed size"))?
        };
        require(
            self.chunks[index] == sha256(&bytes),
            "RecoveryRequired",
            "artifact chunk digest mismatch",
        )?;
        Ok(bytes)
    }

    fn verify(&self, conn: &Connection) -> Result<()> {
        self.verify_into(conn, None, &mut io::sink()).map(|_| ())
    }

    fn verify_into(
        &self,
        conn: &Connection,
        file: Option<&File>,
        output: &mut impl Write,
    ) -> Result<usize> {
        let mut digest = Sha256::new();
        for index in 0..self.chunks.len() {
            let bytes = self.payload_chunk(conn, file, index)?;
            digest.update(&bytes);
            output.write_all(&bytes)?;
        }
        require(
            format!("sha256:{:x}", digest.finalize()) == self.digest,
            "RecoveryRequired",
            "artifact content digest mismatch",
        )?;
        Ok(self.size)
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
    require(
        !files::managed(artifact),
        "UnsupportedCapability",
        "managed OCI archive requires a payload directory",
    )?;
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

pub(crate) fn verify_raw_bytes(
    conn: &Connection,
    state: &Path,
    artifact: &Doc,
    max_bytes: usize,
) -> Result<()> {
    copy_verified_raw(conn, Some(state), artifact, max_bytes, &mut io::sink()).map(|_| ())
}

fn raw_manifest<'a>(
    conn: &Connection,
    artifact: &'a Doc,
    max_bytes: usize,
) -> Result<Manifest<'a>> {
    retention::require_content(conn, artifact)?;
    let manifest = Manifest::read(artifact)?
        .filter(|_| is_raw(artifact))
        .ok_or_else(|| err("RecoveryRequired", "invalid opaque artifact manifest"))?;
    require(
        manifest.size <= max_bytes,
        "QuotaExceeded",
        "opaque artifact exceeds verification limit",
    )?;
    Ok(manifest)
}

/// Copy verified bytes in one pass. Publish or use the private destination only on success.
pub(crate) fn copy_verified_raw(
    conn: &Connection,
    state: Option<&Path>,
    artifact: &Doc,
    max_bytes: usize,
    output: &mut impl Write,
) -> Result<usize> {
    let manifest = raw_manifest(conn, artifact, max_bytes)?;
    let file = files::open_payload(conn, state, artifact, manifest.size)?;
    manifest.verify_into(conn, file.as_ref(), output)
}

/// Read one immutable checked manifest under the caller's retained database snapshot.
pub(crate) struct VerifiedRaw<'a> {
    conn: &'a Connection,
    file: Option<File>,
    manifest: Manifest<'a>,
    position: usize,
    cached: Option<(usize, Vec<u8>)>,
}

/// Check the aggregate content digest before returning a bounded reader.
pub(crate) fn open_verified_raw<'a>(
    conn: &'a Connection,
    state: Option<&Path>,
    artifact: &'a Doc,
    max_bytes: usize,
) -> Result<VerifiedRaw<'a>> {
    let manifest = raw_manifest(conn, artifact, max_bytes)?;
    let file = files::open_payload(conn, state, artifact, manifest.size)?;
    manifest.verify_into(conn, file.as_ref(), &mut io::sink())?;
    Ok(VerifiedRaw {
        conn,
        file,
        manifest,
        position: 0,
        cached: None,
    })
}

impl Read for VerifiedRaw<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position == self.manifest.size {
            return Ok(0);
        }
        let index = self.position / CHUNK_BYTES;
        if self
            .cached
            .as_ref()
            .is_none_or(|(cached, _)| *cached != index)
        {
            self.cached = None;
            let chunk = self
                .manifest
                .payload_chunk(self.conn, self.file.as_ref(), index)
                .map_err(|error| {
                    if error.0.code == "RecoveryRequired" {
                        io::Error::new(io::ErrorKind::InvalidData, error)
                    } else {
                        io::Error::other(error)
                    }
                })?;
            self.cached = Some((index, chunk));
        }
        let bytes = &self.cached.as_ref().expect("loaded chunk").1[self.position % CHUNK_BYTES..];
        let count = bytes.len().min(output.len());
        output[..count].copy_from_slice(&bytes[..count]);
        self.position += count;
        Ok(count)
    }
}

impl Seek for VerifiedRaw<'_> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => self.position as i128 + i128::from(offset),
            SeekFrom::End(offset) => self.manifest.size as i128 + i128::from(offset),
        };
        self.position = usize::try_from(position)
            .ok()
            .filter(|position| *position <= self.manifest.size)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek outside artifact"))?;
        Ok(self.position as u64)
    }
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
        let file = files::open_payload(db.c, Some(&db.cfg.state_dir), artifact, manifest.size)?;
        let mut data = Vec::with_capacity(end - offset);
        let mut position = offset;
        while position < end {
            let bytes = manifest.payload_chunk(db.c, file.as_ref(), position / CHUNK_BYTES)?;
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

pub(crate) fn verify(conn: &Connection, state: Option<&Path>) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT * FROM versions WHERE kind='Artifact' AND json_extract(spec,'$.content_layout') IN (?1,?2,?3)",
    )?;
    let mut expected_chunks = 0;
    for artifact in stmt.query_map([LAYOUT, RAW_LAYOUT, files::LAYOUT], row)? {
        let artifact = artifact?;
        if retention::receipt(conn, &artifact)?.is_some() {
            continue;
        }
        let manifest = Manifest::read(&artifact)?
            .ok_or_else(|| err("RecoveryRequired", "missing artifact content manifest"))?;
        let file = files::open_payload(conn, state, &artifact, manifest.size)?;
        manifest.verify_into(conn, file.as_ref(), &mut io::sink())?;
        if file.is_none() {
            expected_chunks += manifest.chunks.len() as i64;
        }
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
    fn raw_reader_checks_bounds_aggregate_digest_chunks_and_retention() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config::local(&temporary.path().join("state"));
        Store::init(&config).unwrap();
        let mut store = Store::open(config).unwrap();
        store
            .transaction("verify-raw-reader", |db| {
                let node = db.meta("authority")?;
                policy::grant(
                    db,
                    policy::OWNER,
                    &json!({"principal_id":policy::OWNER,
                    "actions":[ingress::UPLOAD, ingress::WITHDRAW],"tool_targets":[node],
                    "units":2,"ttl_seconds":60}),
                )?;
                let mut bytes = vec![0x51; CHUNK_BYTES];
                bytes.extend([0x2a; 8]);
                let upload = ingress::publish_generated(
                    db,
                    policy::OWNER,
                    &bytes,
                    "application/octet-stream",
                    json!({"kind":"LocalSubmission"}),
                )?;
                let reference = &upload["artifact_ref"];
                let artifact = db.resolve(reference)?;

                assert!(
                    matches!(open_verified_raw(db.c, None, &artifact, bytes.len() - 1),
                    Err(error) if error.0.code == "QuotaExceeded")
                );
                let mut different_digest = artifact.clone();
                different_digest.spec["digest"] = json!(sha256(b"different archive"));
                assert!(
                    matches!(open_verified_raw(db.c, None, &different_digest, bytes.len()),
                    Err(error) if error.0.code == "RecoveryRequired")
                );

                let mut file = tempfile::tempfile_in(temporary.path())?;
                assert_eq!(
                    copy_verified_raw(db.c, None, &artifact, bytes.len(), &mut file)?,
                    bytes.len()
                );
                file.rewind()?;
                let mut copied = Vec::new();
                file.read_to_end(&mut copied)?;
                assert_eq!(copied, bytes);

                let mut reader = open_verified_raw(db.c, None, &artifact, bytes.len())?;
                assert_eq!(reader.seek(SeekFrom::End(-9))?, (CHUNK_BYTES - 1) as u64);
                let mut sample = [0; 9];
                reader.read_exact(&mut sample)?;
                assert_eq!(sample.as_slice(), &bytes[CHUNK_BYTES - 1..]);
                assert_eq!(reader.read(&mut sample)?, 0);
                for invalid in [
                    SeekFrom::Start(u64::MAX),
                    SeekFrom::Current(i64::MIN),
                    SeekFrom::End(1),
                ] {
                    assert_eq!(
                        reader.seek(invalid).unwrap_err().kind(),
                        io::ErrorKind::InvalidInput
                    );
                    assert_eq!(reader.stream_position()?, bytes.len() as u64);
                }

                db.c.execute(
                    "UPDATE artifact_chunks SET payload=zeroblob(length(payload))
                    WHERE artifact_id=?1 AND chunk_index=0",
                    [&artifact.id],
                )?;
                reader.rewind()?;
                assert_eq!(
                    reader.read(&mut sample).unwrap_err().kind(),
                    io::ErrorKind::InvalidData
                );
                let mut discarded = io::sink();
                assert_eq!(
                    copy_verified_raw(db.c, None, &artifact, bytes.len(), &mut discarded)
                        .unwrap_err()
                        .0
                        .code,
                    "RecoveryRequired"
                );

                ingress::withdraw(
                    db,
                    policy::OWNER,
                    &json!({"reference":reference,"reason":"test withdrawal"}),
                )?;
                assert!(
                    matches!(open_verified_raw(db.c, None, &artifact, bytes.len()),
                    Err(error) if error.0.code == "ResultEvicted")
                );
                Ok(())
            })
            .unwrap();
    }

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
                verify(db.c, None)?;
                Ok(())
            })
            .unwrap();
    }
}
