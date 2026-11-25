use crate::{ApiError, ApiResult, Client, MAX_ARTIFACT_CHUNK, ResourceRef};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Seek, SeekFrom, Write},
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

fn verification_progress(error: &ApiError) -> Option<(u64, u64)> {
    if error.code != "QuotaExceeded" || error.retry_condition != "SameRequestAfterBackoff" {
        return None;
    }
    let details = error.details.as_ref()?;
    let verified = details
        .get("backup_verified_bytes")
        .and_then(Value::as_u64)?;
    let total = details.get("backup_member_bytes").and_then(Value::as_u64)?;
    (verified > 0 && verified < total).then_some((verified, total))
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
        self.stream_backup_member(backup, member, max_bytes, output, 0, Sha256::new())
    }

    /// Continue a private staging file and authenticate its entire prefix and suffix.
    /// The caller must hold the staging file exclusively through verification.
    pub fn resume_backup_member(
        &self,
        backup: &ResourceRef,
        member: BackupMember,
        max_bytes: u64,
        output: &mut (impl Read + Write + Seek),
    ) -> ApiResult<BackupMemberReceipt> {
        let staging_error = |error| {
            ApiError::new(
                "DependencyUnavailable",
                format!("could not read backup staging file: {error}"),
            )
        };
        let offset = output.seek(SeekFrom::End(0)).map_err(staging_error)?;
        if offset > max_bytes {
            return Err(ApiError::new(
                "QuotaExceeded",
                "staged backup member exceeds client byte limit",
            )
            .into());
        }
        output.seek(SeekFrom::Start(0)).map_err(staging_error)?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 32 * 1024];
        let mut remaining = offset;
        while remaining > 0 {
            let len = remaining.min(buffer.len() as u64) as usize;
            output
                .read_exact(&mut buffer[..len])
                .map_err(staging_error)?;
            hash.update(&buffer[..len]);
            remaining -= len as u64;
        }
        self.stream_backup_member(backup, member, max_bytes, output, offset, hash)
    }

    fn stream_backup_member(
        &self,
        backup: &ResourceRef,
        member: BackupMember,
        max_bytes: u64,
        output: &mut impl Write,
        mut offset: u64,
        mut hash: Sha256,
    ) -> ApiResult<BackupMemberReceipt> {
        let mut identity = None;
        let invalid = || ApiError::new("RecoveryRequired", "invalid backup export chunk sequence");
        loop {
            let requested = json!({"reference":backup,"member":member,"offset":offset,"length":MAX_ARTIFACT_CHUNK});
            let backoff_limit = Duration::from_secs(3);
            let mut deadline = Instant::now() + backoff_limit;
            let mut verified = 0;
            let raw = loop {
                match self.call("Backup.Export", requested.clone()) {
                    Err(error) if error.retryable => {
                        if let Some((bytes, total)) = verification_progress(&error) {
                            if total > max_bytes {
                                return Err(ApiError::new(
                                    "QuotaExceeded",
                                    "backup member exceeds client byte limit",
                                )
                                .into());
                            }
                            if bytes > verified {
                                verified = bytes;
                                deadline = Instant::now() + backoff_limit;
                                continue;
                            }
                        }
                        if Instant::now() >= deadline {
                            return Err(error);
                        }
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
