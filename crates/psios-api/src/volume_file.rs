use crate::{ApiError, ApiResult, Client, ResourceRef, VolumeFileChunkQuery, id, parse, sha256};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

/// Maximum input byte count of a single `volume.read_chunk` Task.
pub const MAX_VOLUME_FILE_READ_CHUNK_BYTES: usize = 96 * 1024;
/// Upper bound for a whole-file read using the existing audited chunk Tasks.
pub const MAX_VOLUME_FILE_READ_BYTES: usize = 4 * MAX_VOLUME_FILE_READ_CHUNK_BYTES;
const MAX_CHUNK_RECORD_BYTES: usize = MAX_VOLUME_FILE_READ_CHUNK_BYTES.div_ceil(3) * 4 + 16 * 1024;

#[derive(Deserialize)]
struct Chunk {
    path: String,
    offset: usize,
    next_offset: usize,
    total_bytes: u64,
    eof: bool,
    data_base64: String,
    digest: String,
    volume_version: String,
    file_revision: String,
}

impl Client {
    /// Read a bounded file from one exact Volume revision. Every read uses the
    /// supplied delegation, and all chunks must describe the same file revision.
    pub fn volume_file_bytes(
        &self,
        volume: &str,
        version: &str,
        path: &str,
        delegation: &str,
        max_bytes: usize,
    ) -> ApiResult<Vec<u8>> {
        self.volume_file_bytes_checked(volume, version, path, delegation, max_bytes, || Ok(()))
    }

    /// Check the consumer before issuing chunk Tasks and retrieving their results.
    pub fn volume_file_bytes_checked(
        &self,
        volume: &str,
        version: &str,
        path: &str,
        delegation: &str,
        max_bytes: usize,
        active: impl Fn() -> ApiResult<()>,
    ) -> ApiResult<Vec<u8>> {
        let query = VolumeFileChunkQuery {
            path: path.to_owned(),
            offset: 0,
            limit: None,
            volume_version: Some(version.to_owned()),
            file_revision: None,
        };
        if !(1..=MAX_VOLUME_FILE_READ_BYTES).contains(&max_bytes) || !query.within_limits() {
            return Err(
                ApiError::new("InvalidArgument", "invalid bounded volume file read").into(),
            );
        }
        let transfer = id("volume-read");
        let deadline = Instant::now() + Duration::from_secs(45);
        let check = || -> ApiResult<()> {
            active()?;
            if Instant::now() >= deadline {
                Err(ApiError::new("DeadlineExceeded", "volume file read timed out").into())
            } else {
                Ok(())
            }
        };
        let limit = max_bytes.min(MAX_VOLUME_FILE_READ_CHUNK_BYTES);
        let mut bytes = Vec::new();
        let mut total = None;
        let mut revision = None;
        let invalid = || ApiError::new("RecoveryRequired", "invalid volume file chunk evidence");
        loop {
            check()?;
            let offset = bytes.len();
            let prepared = self.call(
                "Task.Prepare",
                json!({"tool":"volume.read_chunk","key":format!("{transfer}-{offset}"),
                "delegation_id":delegation,"input":{
                    "target":{"id":volume,"expected_resource_version":version},
                    "path":path,"offset":offset,"limit":limit,"expected_file_revision":revision,
                }}),
            )?;
            check()?;
            let receipt = self.submit_prepared_while(prepared, || check().is_ok())?;
            let task_id = receipt["task_id"].as_str().ok_or_else(invalid)?;
            let task = self.wait(task_id, deadline.saturating_duration_since(Instant::now()))?;
            if task["lifecycle"] != "Succeeded" {
                let code = task["conditions"][0]["type"]
                    .as_str()
                    .unwrap_or("DependencyUnavailable");
                return Err(ApiError::new(code, "volume file chunk did not complete").into());
            }
            let [reference] = task["result"]["artifact_refs"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default()
            else {
                return Err(invalid().into());
            };
            let reference: ResourceRef =
                serde_json::from_value(reference.clone()).map_err(|_| invalid())?;
            let encoded = self.artifact_bytes_checked(&reference, MAX_CHUNK_RECORD_BYTES, check)?;
            let chunk: Chunk = serde_json::from_value(parse(&encoded)?).map_err(|_| invalid())?;
            if chunk.total_bytes > max_bytes as u64 {
                return Err(ApiError::new(
                    "QuotaExceeded",
                    "volume file exceeds client byte limit",
                )
                .into());
            }
            let size = chunk.total_bytes as usize;
            let data = STANDARD.decode(&chunk.data_base64).map_err(|_| invalid())?;
            let end = offset + data.len();
            if chunk.path != path
                || chunk.volume_version != version
                || !VolumeFileChunkQuery::valid_file_revision(&chunk.file_revision)
                || revision
                    .as_ref()
                    .is_some_and(|version| version != &chunk.file_revision)
                || *total.get_or_insert(size) != size
                || chunk.offset != offset
                || offset > size
                || data.len() != (size - offset).min(limit)
                || chunk.next_offset != end
                || chunk.eof != (end == size)
                || chunk.digest != sha256(&data)
            {
                return Err(invalid().into());
            }
            revision = Some(chunk.file_revision);
            bytes.extend(data);
            if chunk.eof {
                check()?;
                return Ok(bytes);
            }
        }
    }
}
