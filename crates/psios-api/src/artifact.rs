use crate::{ApiError, ApiResult, Client, MAX_ARTIFACT_CHUNK, ResourceRef};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};

#[derive(Debug, Serialize)]
pub struct ArtifactRepairReceipt {
    pub reference: ResourceRef,
    pub size_bytes: String,
    pub chunks_repaired: u64,
}

#[derive(Debug, Serialize)]
pub struct ArtifactUploadReceipt {
    pub stage_id: String,
    pub reference: ResourceRef,
    pub size_bytes: u64,
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
    /// Register a byte stream and resume at the authority's committed offset.
    /// Retrying with the same key and unchanged source retrieves a committed publication.
    pub fn upload_artifact(
        &self,
        key: &str,
        declared_media_type: &str,
        ttl_seconds: u32,
        input: &mut (impl Read + Seek),
    ) -> ApiResult<ArtifactUploadReceipt> {
        self.upload_artifact_inner(key, declared_media_type, ttl_seconds, None, input)
    }

    /// Stage and publish bytes under this exact upload delegation.
    pub fn upload_artifact_with_delegation(
        &self,
        key: &str,
        declared_media_type: &str,
        ttl_seconds: u32,
        delegation: &ResourceRef,
        input: &mut (impl Read + Seek),
    ) -> ApiResult<ArtifactUploadReceipt> {
        self.upload_artifact_inner(
            key,
            declared_media_type,
            ttl_seconds,
            Some(delegation),
            input,
        )
    }

    fn upload_artifact_inner(
        &self,
        key: &str,
        declared_media_type: &str,
        ttl_seconds: u32,
        delegation: Option<&ResourceRef>,
        input: &mut (impl Read + Seek),
    ) -> ApiResult<ArtifactUploadReceipt> {
        let invalid = || {
            ApiError::new(
                "RecoveryRequired",
                "invalid artifact upload acknowledgement",
            )
        };
        let profile = self.call("Artifact.UploadProfile", json!({}))?;
        if delegation.is_some() && profile["exact_delegation"] != true {
            return Err(ApiError::new(
                "UnsupportedCapability",
                "authority does not support exact upload delegation",
            )
            .into());
        }
        let delegated_ref = delegation.map(|value| json!(value));
        let scope_matches = |value: &Value| {
            delegated_ref
                .as_ref()
                .is_none_or(|reference| value["delegation_ref"] == *reference)
        };
        let chunk = profile["chunk_bytes"]
            .as_u64()
            .filter(|chunk| (1..=MAX_ARTIFACT_CHUNK as u64).contains(chunk))
            .ok_or_else(invalid)?;
        let max = profile["max_bytes"].as_u64().ok_or_else(invalid)?;
        let origin = input.stream_position().map_err(source_error)?;
        let end = input.seek(SeekFrom::End(0)).map_err(source_error)?;
        let size = end.saturating_sub(origin);
        if size == 0 || size > max {
            return Err(
                ApiError::new("QuotaExceeded", "artifact size outside ingress profile").into(),
            );
        }
        input.seek(SeekFrom::Start(origin)).map_err(source_error)?;
        let mut buffer = vec![0; chunk as usize];
        let mut digest = Sha256::new();
        for at in (0..size).step_by(chunk as usize) {
            let length = (size - at).min(chunk) as usize;
            input
                .read_exact(&mut buffer[..length])
                .map_err(source_error)?;
            digest.update(&buffer[..length]);
        }
        if input.read(&mut [0]).map_err(source_error)? != 0 {
            return Err(ApiError::new("PreconditionFailed", "artifact source size changed").into());
        }
        let digest = format!("sha256:{:x}", digest.finalize());
        let mut request = json!({
            "key":key,"declared_media_type":declared_media_type,"ttl_seconds":ttl_seconds,
            "size_bytes":size,"digest":digest,
        });
        if let Some(reference) = &delegated_ref {
            request["delegation_ref"] = reference.clone();
        }
        let stage = self.call("Artifact.Stage", request)?;
        let stage_id = stage["stage_id"].as_str().ok_or_else(invalid)?.to_owned();
        if stage["digest"] != digest || stage["size_bytes"] != size || !scope_matches(&stage) {
            return Err(invalid().into());
        }
        let terminal = match stage["state"].as_str() {
            Some("Published") => stage,
            Some("Open") => {
                let received = stage["received_bytes"]
                    .as_u64()
                    .filter(|received| {
                        *received == size || (*received < size && received.is_multiple_of(chunk))
                    })
                    .ok_or_else(invalid)?;
                input
                    .seek(SeekFrom::Start(origin + received))
                    .map_err(source_error)?;
                for offset in (received..size).step_by(chunk as usize) {
                    let length = (size - offset).min(chunk) as usize;
                    input
                        .read_exact(&mut buffer[..length])
                        .map_err(source_error)?;
                    let ack = self.call("Artifact.Stage.PutChunk", json!({
                        "stage_id":stage_id,"offset":offset,"data_base64":STANDARD.encode(&buffer[..length]),
                    }))?;
                    if ack["stage_id"] != stage_id
                        || ack["state"] != "Open"
                        || !scope_matches(&ack)
                        || !ack["received_bytes"].as_u64().is_some_and(|received| {
                            (offset + length as u64..=size).contains(&received)
                        })
                    {
                        return Err(invalid().into());
                    }
                }
                self.call("Artifact.Publish", json!({"stage_id":stage_id}))?
            }
            Some("Expired") => {
                return Err(ApiError::new("DeadlineExceeded", "upload key expired").into());
            }
            Some("Withdrawn") => {
                let withdrawn = ApiError::new("ResultEvicted", "published upload was withdrawn");
                return Err(withdrawn.into());
            }
            Some("Discarded") => {
                if stage["close_reason"] == "AuthorityRevoked" {
                    return Err(ApiError::new(
                        "PermissionDenied",
                        "upload delegation is no longer active",
                    )
                    .into());
                }
                return Err(ApiError::new("PreconditionFailed", "upload key was discarded").into());
            }
            _ => return Err(invalid().into()),
        };
        let reference: ResourceRef =
            serde_json::from_value(terminal["artifact_ref"].clone()).map_err(|_| invalid())?;
        if terminal["state"] != "Published"
            || terminal["stage_id"] != stage_id
            || terminal["size_bytes"] != size
            || terminal["digest"] != digest
            || !scope_matches(&terminal)
            || reference.digest != digest
        {
            return Err(invalid().into());
        }
        Ok(ArtifactUploadReceipt {
            stage_id,
            reference,
            size_bytes: size,
        })
    }

    /// Verify a settled Task's pinned results and return its current availability view.
    pub fn verify_task_results(
        &self,
        id: &str,
        expected_resource_version: &str,
        max_bytes: Option<u64>,
    ) -> ApiResult<serde_json::Value> {
        self.call(
            "Task.VerifyResults",
            json!({"id":id,"expected_resource_version":expected_resource_version,"max_bytes":max_bytes}),
        )
    }

    /// Abandon the verified unavailable results of one exact settled read-only Task.
    pub fn abandon_task_results(
        &self,
        id: &str,
        expected_resource_version: &str,
        reason: &str,
    ) -> ApiResult<serde_json::Value> {
        self.call(
            "Task.AbandonResults",
            json!({"id":id,"expected_resource_version":expected_resource_version,"reason":reason}),
        )
    }

    /// Release a damaged local upload by its immutable published reference.
    pub fn abandon_uploaded_artifact(
        &self,
        reference: &ResourceRef,
        reason: &str,
    ) -> ApiResult<serde_json::Value> {
        self.call(
            "Artifact.AbandonUpload",
            json!({"reference":reference,"reason":reason}),
        )
    }

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
