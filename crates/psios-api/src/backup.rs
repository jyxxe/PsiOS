use crate::{ApiError, ApiResult, Client, MAX_ARTIFACT_CHUNK, ResourceRef};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum BackupMember {
    Manifest,
    Blob { index: usize },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupChunk {
    pub offset: u64,
    pub total_bytes: u64,
    pub digest: String,
    pub next_offset: Option<u64>,
    pub data_base64: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BackupMemberReceipt {
    pub bytes: u64,
    pub digest: String,
}

impl Client {
    /// Stream a version-pinned member to a private sink and verify its complete digest.
    /// Publish the sink only after this method returns successfully.
    pub fn download_backup_member(
        &self,
        backup: &ResourceRef,
        member: BackupMember,
        max_bytes: u64,
        output: &mut impl Write,
    ) -> ApiResult<BackupMemberReceipt> {
        let mut hash = Sha256::new();
        let mut offset = 0;
        let mut identity = None;
        let invalid = || ApiError::new("RecoveryRequired", "invalid backup export chunk sequence");
        loop {
            let requested = json!({"reference":backup,"member":member,"offset":offset,"length":MAX_ARTIFACT_CHUNK});
            let deadline = Instant::now() + Duration::from_secs(3);
            let raw = loop {
                match self.call("Backup.Export", requested.clone()) {
                    Err(error) if error.retryable && Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    response => break response?,
                }
            };
            let chunk: BackupChunk = serde_json::from_value(raw).map_err(|_| invalid())?;
            if chunk.total_bytes > max_bytes {
                return Err(ApiError::new(
                    "QuotaExceeded",
                    "backup member exceeds client byte limit",
                )
                .into());
            }
            let identity =
                identity.get_or_insert_with(|| (chunk.total_bytes, chunk.digest.clone()));
            let data = STANDARD.decode(chunk.data_base64).map_err(|_| invalid())?;
            let remaining = chunk.total_bytes.checked_sub(offset).ok_or_else(invalid)?;
            let end = offset + remaining.min(MAX_ARTIFACT_CHUNK as u64);
            if chunk.offset != offset
                || *identity != (chunk.total_bytes, chunk.digest)
                || data.len() as u64 != end - offset
                || chunk.next_offset != (end < chunk.total_bytes).then_some(end)
            {
                return Err(invalid().into());
            }
            output.write_all(&data).map_err(|e| {
                ApiError::new(
                    "DependencyUnavailable",
                    format!("could not write backup member: {e}"),
                )
            })?;
            hash.update(&data);
            offset = end;
            if chunk.next_offset.is_none() {
                let digest = format!("sha256:{:x}", hash.finalize());
                if digest != identity.1 {
                    return Err(
                        ApiError::new("RecoveryRequired", "backup member digest mismatch").into(),
                    );
                }
                return Ok(BackupMemberReceipt {
                    bytes: offset,
                    digest,
                });
            }
        }
    }
}
