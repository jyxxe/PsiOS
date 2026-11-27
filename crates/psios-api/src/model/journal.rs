//! A private, single-worker outbox for a completed local provider attempt.
use super::{DecisionRequest, Delivery, SessionModel, Turn, claim_parameters};
use crate::{ApiError, ApiResult, Client, ResourceRef, canonical, parse, sha256};
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
const LATE_DECISION_FORMAT: u32 = 4;
const DECISION_REJECTION_FORMAT: u32 = 5;

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    late_decision_model_ref: Option<ResourceRef>,
    request: Delivery,
}

impl Pending {
    fn new(
        identity: (String, String),
        claim_key: &str,
        queue_model: Option<&ResourceRef>,
        request: Delivery,
        late_decision_model_ref: Option<ResourceRef>,
    ) -> Self {
        let format = if matches!(&request, Delivery::Decision(d) if d.decision_rejection_policy.is_some())
        {
            DECISION_REJECTION_FORMAT
        } else if late_decision_model_ref.is_some() {
            LATE_DECISION_FORMAT
        } else {
            request.format()
        };
        Self {
            format,
            authority: identity.0,
            provider: identity.1,
            claim_key: claim_key.to_owned(),
            queue_model: queue_model.cloned(),
            late_decision_model_ref,
            request,
        }
    }

    fn valid_format(&self) -> bool {
        if matches!(&self.request, Delivery::Decision(d) if d.decision_rejection_policy.is_some()) {
            return self.format == DECISION_REJECTION_FORMAT
                && self.late_decision_model_ref.as_ref().is_some_and(|model| {
                    self.queue_model
                        .as_ref()
                        .is_none_or(|queued| queued == model)
                });
        }
        if let Some(model) = &self.late_decision_model_ref {
            self.format == LATE_DECISION_FORMAT
                && matches!(self.request, Delivery::Decision(_))
                && self
                    .queue_model
                    .as_ref()
                    .is_none_or(|queued| queued == model)
        } else {
            self.format == self.request.format()
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
        if !record.valid_format() {
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

fn should_check_lease(error: &ApiError) -> bool {
    !error.retryable
        && matches!(
            error.code.as_str(),
            "PermissionDenied"
                | "PreconditionFailed"
                | "QuotaExceeded"
                | "InvalidArgument"
                | "Conflict"
                | "ApprovalInvalidated"
                | "SchemaVersionUnsupported"
        )
}

fn pending_delivery(error: &ApiError) -> Box<ApiError> {
    ApiError::new(
        "OutcomeUnknown",
        format!("retained model delivery is pending after {}", error.code),
    )
    .into()
}

fn decision_bytes(request: &DecisionRequest) -> Vec<u8> {
    canonical(&json!(request.decision))
}

fn digest_valid(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn session_revision(reference: &Value, session_id: &str) -> Option<u64> {
    let exact: ResourceRef = serde_json::from_value(reference.clone()).ok()?;
    let revision = exact.version.strip_prefix('r')?.parse().ok()?;
    (exact.resource_id == session_id && revision > 0 && digest_valid(&exact.digest))
        .then_some(revision)
}

fn matches_decision(receipt: &Value, request: &DecisionRequest) -> bool {
    let bytes = decision_bytes(request);
    receipt["decision_digest"] == sha256(&bytes) && receipt["output_bytes"] == bytes.len()
}

fn matches_rejection(receipt: &Value, request: &DecisionRequest) -> bool {
    let Some(rejection) = receipt.get("rejection") else {
        return receipt.get("outcome_knowledge").is_none();
    };
    request
        .decision_rejection_policy
        .is_some_and(|policy| rejection == &json!({"policy":policy,"code":"ToolNotDeclared"}))
        && receipt["lifecycle"] == "Failed"
        && receipt["outcome_knowledge"] == "Known"
        && receipt.get("task_id").is_some_and(Value::is_null)
}

fn matches_observation(
    receipt: &Value,
    record: &Pending,
    request: &DecisionRequest,
    model: &ResourceRef,
) -> bool {
    let observation = &receipt["observation"];
    let issued = session_revision(&observation["issued_session_ref"], &request.session_id);
    let unknown = session_revision(&observation["unknown_session_ref"], &request.session_id);
    let usage_digest = request.usage.map(|usage| sha256(&canonical(&json!(usage))));
    receipt["status"] == "Recorded"
        && observation["lease_id"] == request.lease_id
        && observation["model_ref"] == json!(model)
        && observation["provider_principal_id"] == record.provider
        && observation["provider_authority_digest"]
            .as_str()
            .is_some_and(digest_valid)
        && issued
            .zip(unknown)
            .is_some_and(|(before, after)| before < after)
        && observation["observed_at"]
            .as_str()
            .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok())
        && observation["usage_digest"] == json!(usage_digest)
        && matches_decision(observation, request)
}

fn observation_terminal(error: &ApiError) -> bool {
    !error.retryable
        && (error.code == "Fenced"
            || (error.code == "PermissionDenied"
                && error.details.as_ref().is_some_and(|details| {
                    details["reason"] == "OriginalProviderAuthorityUnavailable"
                })))
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
        if let Some(model) = &record.late_decision_model_ref {
            return self.deliver_observable_decision(journal, record, model);
        }
        if let Some(model) = &record.queue_model {
            return self.deliver_legacy_queued(journal, record, model);
        }
        let answer = self.send_delivery(&record.request);
        // The authority checks previously settled receipts before checking the
        // lease fence. Fenced therefore also resolves this exact pending request.
        if answer.is_ok() || answer.as_ref().is_err_and(|e| e.code == "Fenced") {
            journal.remove()?;
        }
        answer
    }

    fn claim_disposition(&self, record: &Pending, model: &ResourceRef) -> ApiResult<Value> {
        self.call(
            "Model.ClaimStatus",
            json!({
                "model_ref":model, "key":record.claim_key, "lease_id":record.request.lease_id()
            }),
        )
    }

    fn deliver_legacy_queued(
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
            Err(e) if should_check_lease(&e) => {
                let disposition = self.claim_disposition(record, model)?;
                match disposition["status"].as_str() {
                    Some("Settled" | "Fenced") => {
                        journal.remove()?;
                        Ok(disposition)
                    }
                    Some("Pending") => Err(pending_delivery(&e)),
                    _ => Err(crate::invalid("invalid queued lease disposition")),
                }
            }
            Err(e) => Err(e),
        }
    }

    fn deliver_observable_decision(
        &self,
        journal: &Journal,
        record: &Pending,
        model: &ResourceRef,
    ) -> ApiResult<Value> {
        let Delivery::Decision(request) = &record.request else {
            return Err(
                ApiError::new("RecoveryRequired", "retained observation lacks a decision").into(),
            );
        };
        match self.send_delivery(&record.request) {
            Ok(receipt) => {
                if !matches_decision(&receipt, request)
                    || !matches_rejection(&receipt, request)
                    || session_revision(&receipt["session_ref"], &request.session_id).is_none()
                {
                    return Err(ApiError::new(
                        "RecoveryRequired",
                        "model response does not confirm the retained decision",
                    )
                    .into());
                }
                journal.remove()?;
                return Ok(receipt);
            }
            Err(error)
                if !error.retryable
                    && matches!(error.code.as_str(), "Fenced" | "PermissionDenied") => {}
            // An older Core rejects the negotiated response field before
            // looking up its receipt. Keep the exact request for a compatible Core.
            Err(error)
                if request.decision_rejection_policy.is_some()
                    && error.code == "InvalidArgument" =>
            {
                return Err(error);
            }
            Err(error) if should_check_lease(&error) => {
                if let Some(queued) = &record.queue_model {
                    // Settled can also be a strict Unknown. It does not
                    // acknowledge this decision or authorize its deletion.
                    match self.claim_disposition(record, queued)?["status"].as_str() {
                        Some("Settled" | "Fenced") => {}
                        Some("Pending") => return Err(pending_delivery(&error)),
                        _ => return Err(crate::invalid("invalid queued lease disposition")),
                    }
                }
            }
            Err(error) => return Err(error),
        }
        match self.call("Model.ObserveLateDecision", request.late_observation()) {
            Ok(receipt) => {
                if !matches_observation(&receipt, record, request, model) {
                    return Err(ApiError::new(
                        "RecoveryRequired",
                        "late decision receipt does not match the retained decision",
                    )
                    .into());
                }
                journal.remove()?;
                Ok(receipt)
            }
            Err(error) if observation_terminal(&error) => {
                journal.remove()?;
                if error.code == "Fenced" && record.queue_model.is_some() {
                    Ok(json!({"status":"Fenced"}))
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
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
                Turn::Deliver {
                    request,
                    late_decision_model_ref,
                } => {
                    let record =
                        Pending::new(identity, key, None, request, late_decision_model_ref);
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
            return self.deliver_record(&journal, &pending).map(Some);
        }
        let mut params = claim_parameters(json!({"model_ref": model, "key": key}), provider);
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
        match self.decide_claim(&session, claim, provider, Some(model))? {
            Turn::Completed(receipt) => Ok(Some(receipt)),
            Turn::Deliver {
                request,
                late_decision_model_ref,
            } => {
                let record =
                    Pending::new(identity, key, Some(model), request, late_decision_model_ref);
                journal.retain(&record)?;
                self.deliver_record(&journal, &record).map(Some)
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
            assert!(restored.valid_format());
            assert_eq!(restored.request.format(), format);
            assert_eq!(restored.request.method(), method);
            assert_eq!(json!(restored), original);
        }
    }

    #[test]
    fn negotiated_journal_matches_exact_late_observation_and_keeps_ambiguous_errors() {
        let reference = |resource_id: &str, version: &str| {
            json!({
                "resource_id":resource_id,"version":version,
                "digest":sha256(version.as_bytes()),
            })
        };
        let model = reference("model-fixture", "r1");
        let request = json!({
            "session_id":"session-fixture","lease_id":"lease-fixture",
            "decision":{"action":"tool","tool":"system.status","input":{}},
            "usage":{"input_tokens":12,"output_tokens":4},
        });
        let pending = json!({
            "format":LATE_DECISION_FORMAT,"authority":"authority-fixture","provider":"provider-fixture",
            "queue_model":model,"late_decision_model_ref":model,"claim_key":"claim-fixture","request":request,
        });
        let saved: Pending = serde_json::from_value(pending.clone()).unwrap();
        assert!(saved.valid_format());
        assert_eq!(json!(saved), pending);
        for (field, wrong) in [
            ("format", json!(2)),
            ("late_decision_model_ref", Value::Null),
            ("queue_model", reference("model-fixture", "r2")),
        ] {
            let mut foreign = pending.clone();
            foreign[field] = wrong;
            let foreign: Pending = serde_json::from_value(foreign).unwrap();
            assert!(!foreign.valid_format());
        }

        let Delivery::Decision(delivery) = &saved.request else {
            unreachable!()
        };
        let decision = canonical(&request["decision"]);
        let exact = json!({"status":"Recorded","observation":{
            "lease_id":request["lease_id"],"model_ref":model,
            "issued_session_ref":reference("session-fixture","r2"),
            "unknown_session_ref":reference("session-fixture","r3"),
            "provider_principal_id":saved.provider,"provider_authority_digest":sha256(b"binding"),
            "decision_digest":sha256(&decision),"output_bytes":decision.len(),
            "observed_at":"2026-11-26T00:00:00.000Z",
            "usage_digest":sha256(&canonical(&request["usage"])),
        }});
        let model = saved.late_decision_model_ref.as_ref().unwrap();
        assert!(matches_observation(&exact, &saved, delivery, model));
        for field in [
            "lease_id",
            "model_ref",
            "issued_session_ref",
            "unknown_session_ref",
            "provider_principal_id",
            "provider_authority_digest",
            "decision_digest",
            "output_bytes",
            "usage_digest",
        ] {
            let mut wrong = exact.clone();
            wrong["observation"][field] = Value::Null;
            assert!(
                !matches_observation(&wrong, &saved, delivery, model),
                "{field}"
            );
        }
        let mut pending = ApiError::new("PreconditionFailed", "lease settlement pending");
        pending.retryable = true;
        pending.retry_condition = "SameRequestAfterBackoff".into();
        assert!(!should_check_lease(&pending));
        assert!(!observation_terminal(&pending));
        assert!(!observation_terminal(&ApiError::new(
            "DependencyUnavailable",
            "receipt lost"
        )));
        let mut denied = ApiError::new("PermissionDenied", "unauthorized");
        assert!(!observation_terminal(&denied));
        denied.details = Some(json!({"reason":"OriginalProviderAuthorityUnavailable"}));
        assert!(observation_terminal(&denied));
        assert!(observation_terminal(&ApiError::new(
            "Fenced",
            "ineligible for observation"
        )));
    }
}
