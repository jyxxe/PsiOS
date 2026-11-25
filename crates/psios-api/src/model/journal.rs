//! A private, single-worker outbox for a completed local provider attempt.
use super::{Delivery, SessionModel, Turn};
use crate::{ApiError, ApiResult, Client, ResourceRef, parse};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, open, openat, renameat_with, unlinkat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
    time::Duration,
};

const PENDING: &str = "psios-model-pending.json";
const STAGING: &str = ".psios-model-pending.tmp";
const LIMIT: u64 = 72 * 1024;

fn storage_error(_: impl std::fmt::Display) -> Box<ApiError> {
    ApiError::new("StorageUnavailable", "provider decision journal I/O failed").into()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    format: u32,
    authority: String,
    provider: String,
    claim_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    queue_model: Option<ResourceRef>,
    request: Delivery,
}

impl Pending {
    fn new(
        identity: (String, String),
        claim_key: &str,
        queue_model: Option<&ResourceRef>,
        request: Delivery,
    ) -> Self {
        Self {
            format: request.format(),
            authority: identity.0,
            provider: identity.1,
            claim_key: claim_key.to_owned(),
            queue_model: queue_model.cloned(),
            request,
        }
    }

    fn verify_origin(
        &self,
        identity: &(String, String),
        model: Option<&ResourceRef>,
    ) -> ApiResult<()> {
        if self.authority != identity.0
            || self.provider != identity.1
            || self.queue_model.as_ref() != model
            || matches!((&self.request, model),
                (Delivery::Failure(report), Some(model)) if &report.model_ref != model)
        {
            return Err(ApiError::new(
                "Conflict",
                "journal contains a delivery for another authority, provider or model queue",
            )
            .into());
        }
        Ok(())
    }
}

struct Journal(File);
impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl Journal {
    fn open(path: &Path) -> ApiResult<Self> {
        let dir = File::from(
            open(
                path,
                OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(storage_error)?,
        );
        let meta = dir.metadata().map_err(storage_error)?;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(ApiError::new(
                "PermissionDenied",
                "expected a private, owned provider directory",
            )
            .into());
        }
        dir.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                ApiError::new("Conflict", "provider journal already in use").into()
            }
            std::fs::TryLockError::Error(e) => storage_error(e),
        })?;
        // A staging record that never reached the atomic rename was never sent.
        match unlinkat(&dir, STAGING, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(storage_error(e)),
        }
        Ok(Self(dir))
    }

    fn pending(&self) -> ApiResult<Option<Pending>> {
        let fd = match openat(
            &self.0,
            PENDING,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(storage_error(e)),
        };
        let f = File::from(fd);
        let m = f.metadata().map_err(storage_error)?;
        if !m.is_file()
            || m.nlink() != 1
            || m.mode() & 0o077 != 0
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.len() > LIMIT
        {
            return Err(ApiError::new(
                "RecoveryRequired",
                "invalid retained provider decision file",
            )
            .into());
        }
        let mut bytes = Vec::new();
        f.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        let invalid = || ApiError::new("RecoveryRequired", "invalid retained provider decision");
        if bytes.len() as u64 > LIMIT {
            return Err(invalid().into());
        }
        let record: Pending =
            serde_json::from_value(parse(&bytes).map_err(|_| invalid())?).map_err(|_| invalid())?;
        if record.format != record.request.format() {
            return Err(invalid().into());
        }
        // A previous fsync of the renamed directory entry may have returned an
        // error. Establish its durability before allowing any retransmission.
        self.0.sync_all().map_err(storage_error)?;
        Ok(Some(record))
    }

    fn retain(&self, record: &Pending) -> ApiResult<()> {
        let mut bytes = vec![0; LIMIT as usize];
        let mut buffer = std::io::Cursor::new(bytes.as_mut_slice());
        serde_json::to_writer(&mut buffer, record).map_err(|_| {
            ApiError::new("QuotaExceeded", "decision exceeds local retention limit")
        })?;
        let size = buffer.position() as usize;
        let mut f = File::from(
            openat(
                &self.0,
                STAGING,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(storage_error)?,
        );
        f.write_all(&bytes[..size])
            .and_then(|_| f.sync_all())
            .map_err(storage_error)?;
        renameat_with(&self.0, STAGING, &self.0, PENDING, RenameFlags::NOREPLACE)
            .map_err(storage_error)?;
        self.0.sync_all().map_err(storage_error)
    }

    fn remove(&self) -> ApiResult<()> {
        unlinkat(&self.0, PENDING, AtFlags::empty()).map_err(storage_error)?;
        self.0.sync_all().map_err(storage_error)
    }
}

impl Client {
    fn provider_identity(&self) -> ApiResult<(String, String)> {
        let who = self.call("Principal.WhoAmI", json!({}))?;
        let identity = |field: &str| {
            who[field]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| crate::invalid("missing model authority or provider identity"))
        };
        Ok((identity("authority_id")?, identity("principal_id")?))
    }

    fn deliver_record(&self, journal: &Journal, record: &Pending) -> ApiResult<Value> {
        let answer = self.send_delivery(&record.request);
        // The authority checks previously settled receipts before checking the
        // lease fence. Fenced therefore also resolves this exact pending request.
        if answer.is_ok() || answer.as_ref().is_err_and(|e| e.code == "Fenced") {
            journal.remove()?;
        }
        answer
    }

    fn deliver_queued_record(
        &self,
        journal: &Journal,
        record: &Pending,
        model: &ResourceRef,
    ) -> ApiResult<Value> {
        match self.send_delivery(&record.request) {
            Ok(receipt) => {
                journal.remove()?;
                Ok(receipt)
            }
            Err(e) if e.code == "Fenced" => {
                journal.remove()?;
                Ok(json!({"status":"Fenced"}))
            }
            Err(e)
                if !e.retryable
                    && matches!(
                        e.code.as_str(),
                        "PermissionDenied"
                            | "PreconditionFailed"
                            | "QuotaExceeded"
                            | "InvalidArgument"
                            | "Conflict"
                            | "ApprovalInvalidated"
                            | "SchemaVersionUnsupported"
                    ) =>
            {
                let disposition = self.call("Model.ClaimStatus", json!({
                    "model_ref":model, "key":record.claim_key, "lease_id":record.request.lease_id()
                }))?;
                match disposition["status"].as_str() {
                    Some("Settled" | "Fenced") => {
                        journal.remove()?;
                        Ok(disposition)
                    }
                    Some("Pending") => Err(ApiError::new(
                        "OutcomeUnknown",
                        format!("retained model delivery is pending after {}", e.code),
                    )
                    .into()),
                    _ => Err(crate::invalid("invalid queued lease disposition")),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Retain a model decision or failure report before transmission. Reusing the same private
    /// directory, authority, principal, session and key resumes its exact request.
    pub fn model_turn_durable(
        &self,
        session: &str,
        key: &str,
        provider: &(impl SessionModel + ?Sized),
        dir: &Path,
    ) -> ApiResult<Value> {
        let journal = Journal::open(dir)?;
        let identity = self.provider_identity()?;
        let record = if let Some(pending) = journal.pending()? {
            pending.verify_origin(&identity, None)?;
            if pending.request.session_id() != session || pending.claim_key != key {
                return Err(ApiError::new(
                    "Conflict",
                    "journal contains a decision for another session or key",
                )
                .into());
            }
            pending
        } else {
            match self.decide_turn(session, key, provider)? {
                Turn::Completed(receipt) => return Ok(receipt),
                Turn::Deliver(request) => {
                    let record = Pending::new(identity, key, None, request);
                    journal.retain(&record)?;
                    record
                }
            }
        };
        self.deliver_record(&journal, &record)
    }

    /// Recover a retained delivery before claiming the next opt-in session for
    /// this exact model. Each worker requires its own private decision directory.
    pub fn model_next_durable(
        &self,
        model: &ResourceRef,
        key: &str,
        provider: &(impl SessionModel + ?Sized),
        dir: &Path,
        wait: Duration,
    ) -> ApiResult<Option<Value>> {
        let journal = Journal::open(dir)?;
        let identity = self.provider_identity()?;
        if let Some(pending) = journal.pending()? {
            pending.verify_origin(&identity, Some(model))?;
            return self
                .deliver_queued_record(&journal, &pending, model)
                .map(Some);
        }
        let mut params = json!({"model_ref": model, "key": key});
        let method = if wait.is_zero() {
            "Model.ClaimNext"
        } else {
            let millis = wait.as_millis();
            if !(1..=10_000).contains(&millis) {
                return Err(crate::invalid("model wait must be 1 to 10000 milliseconds"));
            }
            params["timeout_ms"] = json!(millis);
            "Model.WaitNext"
        };
        let claim = match self.call(method, params.clone()) {
            Err(e) if e.code == "DependencyUnavailable" => self.call(method, params)?,
            other => other?,
        };
        if claim["available"] == false {
            return Ok(None);
        }
        let session = claim["session_id"]
            .as_str()
            .ok_or_else(|| crate::invalid("missing queued model session"))?
            .to_owned();
        match Self::decide_claim(&session, claim, provider, Some(model))? {
            Turn::Completed(receipt) => Ok(Some(receipt)),
            Turn::Deliver(request) => {
                let record = Pending::new(identity, key, Some(model), request);
                journal.retain(&record)?;
                self.deliver_queued_record(&journal, &record, model)
                    .map(Some)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_request_formats_keep_their_original_wire_shape() {
        let decision = json!({"session_id":"session-fixture","lease_id":"lease-fixture",
            "decision":{"action":"finish","summary":"inventory ready"}});
        let mut metered = decision.clone();
        metered["usage"] = json!({"input_tokens":3,"output_tokens":5});
        let failed = json!({"session_id":"session-fixture","lease_id":"lease-fixture",
            "model_ref":{"resource_id":"model-fixture","version":"r1","digest":"digest"},
            "outcome":"NotStarted"});
        for (format, request, method) in [
            (1, decision, "Model.Respond"),
            (2, metered, "Model.Respond"),
            (3, failed, "Model.ReportFailure"),
        ] {
            let original = json!({"format":format,"request":request,"authority":"local-fixture",
                "provider":"provider-fixture","claim_key":"fixture"});
            let restored: Pending = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(restored.request.format(), format);
            assert_eq!(restored.request.method(), method);
            assert_eq!(json!(restored), original);
        }
    }
}
