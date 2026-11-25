use crate::{ApiError, ApiResult, Client, MAX_ARTIFACT_CHUNK, ResourceRef};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};

#[derive(Debug, Serialize)]
pub struct ArtifactRepairReceipt {
    pub reference: ResourceRef,
    pub size_bytes: String,
    pub chunks_repaired: u64,
}

fn source_error(error: Error) -> Box<ApiError> {
    if error.kind() == ErrorKind::UnexpectedEof {
        ApiError::new(
            "PreconditionFailed",
            "artifact source is shorter than expected",
        )
        .into()
    } else {
        ApiError::new(
            "StorageUnavailable",
            format!("cannot read artifact source: {}", error.kind()),
        )
        .into()
    }
}

impl Client {
    /// Verify the entire source before repairing available chunks at the pinned reference.
    /// Each accepted repair is durable and safe to repeat with the original bytes.
    pub fn repair_artifact(
        &self,
        reference: &ResourceRef,
        input: &mut (impl Read + Seek),
        max_bytes: u64,
    ) -> ApiResult<ArtifactRepairReceipt> {
        let pinned = json!(reference);
        let metadata = self.call("Artifact.Get", json!({"reference":pinned}))?;
        if metadata["reference"] != pinned {
            return Err(
                ApiError::new("RecoveryRequired", "artifact metadata reference changed").into(),
            );
        }
        if metadata["availability"] == "Evicted" {
            return Err(
                ApiError::new("ResultEvicted", "artifact was intentionally evicted").into(),
            );
        }
        let chunk = metadata["repair_chunk_bytes"]
            .as_u64()
            .filter(|size| (1..=MAX_ARTIFACT_CHUNK as u64).contains(size))
            .ok_or_else(|| {
                ApiError::new("UnsupportedCapability", "artifact does not support repair")
            })?;
        let size: u64 = metadata["resource"]["spec"]["size_bytes"]
            .as_str()
            .and_then(|size| size.parse().ok())
            .filter(|size| *size != 0)
            .ok_or_else(|| {
                ApiError::new("RecoveryRequired", "artifact metadata has no valid size")
            })?;
        if size > max_bytes {
            return Err(
                ApiError::new("QuotaExceeded", "artifact exceeds repair byte limit").into(),
            );
        }

        let origin = input.stream_position().map_err(source_error)?;
        let mut buffer = vec![0; chunk as usize];
        let mut digest = Sha256::new();
        for offset in (0..size).step_by(chunk as usize) {
            let length = (size - offset).min(chunk) as usize;
            input
                .read_exact(&mut buffer[..length])
                .map_err(source_error)?;
            digest.update(&buffer[..length]);
        }
        if input.read(&mut [0]).map_err(source_error)? != 0
            || format!("sha256:{:x}", digest.finalize()) != reference.digest
        {
            return Err(ApiError::new(
                "PreconditionFailed",
                "artifact source does not match the pinned content",
            )
            .into());
        }

        input.seek(SeekFrom::Start(origin)).map_err(source_error)?;
        let mut chunks_repaired = 0;
        for offset in (0..size).step_by(chunk as usize) {
            let length = (size - offset).min(chunk) as usize;
            input
                .read_exact(&mut buffer[..length])
                .map_err(source_error)?;
            let reply = self.call(
                "Artifact.RepairChunk",
                json!({"reference":pinned,"offset":offset,"data_base64":STANDARD.encode(&buffer[..length])}),
            )?;
            if reply["reference"] != pinned || reply["offset"] != offset {
                return Err(ApiError::new(
                    "RecoveryRequired",
                    "artifact repair acknowledgement changed",
                )
                .into());
            }
            let repaired = reply["repaired"].as_bool().ok_or_else(|| {
                ApiError::new(
                    "RecoveryRequired",
                    "invalid artifact repair acknowledgement",
                )
            })?;
            chunks_repaired += u64::from(repaired);
        }
        Ok(ArtifactRepairReceipt {
            reference: reference.clone(),
            size_bytes: size.to_string(),
            chunks_repaired,
        })
    }
}
