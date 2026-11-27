//! Timing for the provider transport belongs to the lease, outside the model context.
use crate::{ApiResult, Client, ResourceRef, invalid, sha256};
use chrono::{DateTime, Utc};
use rustix::{
    fs::{Mode, OFlags, open},
    time::{ClockId, clock_gettime},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, read_link},
    io::{ErrorKind, Read},
    time::{Duration, Instant},
};

/// Authority-issued execution window. Its boottime value is scoped to one Linux clock domain.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLeaseTiming {
    pub clock_domain: String,
    pub deadline_boottime_ms: i64,
    pub window_ms: i64,
}

/// Exact identity needed to check an in-flight provider invocation.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelLeaseStatusRequest {
    pub session_id: String,
    pub model_ref: ResourceRef,
    pub lease_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelLeaseStatus {
    Active,
    Ended,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LeaseStatusResponse {
    status: ModelLeaseStatus,
}

impl Client {
    /// Check only the liveness of a lease belonging to the authenticated provider.
    pub fn model_lease_status(
        &self,
        request: &ModelLeaseStatusRequest,
    ) -> ApiResult<ModelLeaseStatus> {
        read_status(self.call("Model.LeaseStatus", json!(request))?)
    }
}

fn read_status(response: Value) -> ApiResult<ModelLeaseStatus> {
    serde_json::from_value::<LeaseStatusResponse>(response)
        .map(|response| response.status)
        .map_err(|_| invalid("invalid model lease status response"))
}

impl ModelLeaseTiming {
    pub fn from_authority_clock(
        boot_id: &str,
        time_namespace: &str,
        deadline: i64,
        window: i64,
    ) -> Self {
        Self {
            clock_domain: clock_domain(boot_id, time_namespace),
            deadline_boottime_ms: deadline,
            window_ms: window,
        }
    }
}

/// Exact claim and time available to a provider adapter. Keep these fields outside model input.
pub struct ModelInvocation<'a> {
    pub lease_id: &'a str,
    pub turn: u32,
    pub expires_at: DateTime<Utc>,
    authority: &'a Client,
    target: ModelLeaseStatusRequest,
    deadline_boottime_ms: Option<i64>,
    received: Instant,
    available: Duration,
}

impl<'a> ModelInvocation<'a> {
    pub(super) fn from_claim(
        authority: &'a Client,
        lease_id: &'a str,
        claim: &Value,
    ) -> ApiResult<Self> {
        let received = Instant::now();
        let expires_at = claim["expires_at"]
            .as_str()
            .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
            .ok_or_else(|| invalid("model lease has no valid expiration"))?
            .to_utc();
        let turn = claim["turn"]
            .as_u64()
            .and_then(|raw| u32::try_from(raw).ok())
            .filter(|turn| *turn > 0)
            .ok_or_else(|| invalid("model lease has no valid turn"))?;
        let mut available = wall_remaining(expires_at);
        let mut deadline_boottime_ms = None;
        if let Some(raw) = claim.get("lease_timing") {
            let timing: ModelLeaseTiming = serde_json::from_value(raw.clone())
                .map_err(|_| invalid("invalid model lease timing"))?;
            if !(1..=300_000).contains(&timing.window_ms)
                || timing.deadline_boottime_ms < timing.window_ms
                || timing.clock_domain.len() != 71
                || !timing.clock_domain.starts_with("sha256:")
                || !timing.clock_domain[7..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid("invalid model lease timing"));
            }
            available = available.min(Duration::from_millis(timing.window_ms as u64));
            if local_clock_domain().as_ref() == Some(&timing.clock_domain) {
                deadline_boottime_ms = Some(timing.deadline_boottime_ms);
            }
        }
        let target = ModelLeaseStatusRequest {
            session_id: claim["context"]["session_id"]
                .as_str()
                .ok_or_else(|| invalid("model lease has no session reference"))?
                .to_owned(),
            model_ref: serde_json::from_value(claim["context"]["model_ref"].clone())
                .map_err(|_| invalid("model lease has no model reference"))?,
            lease_id: lease_id.to_owned(),
        };
        Ok(Self {
            lease_id,
            turn,
            expires_at,
            authority,
            target,
            deadline_boottime_ms,
            received,
            available,
        })
    }

    /// Verify the lease against live session and provider authorization.
    pub fn status(&self) -> ApiResult<ModelLeaseStatus> {
        self.authority.model_lease_status(&self.target)
    }

    pub(super) fn model_ref(&self) -> &ResourceRef {
        &self.target.model_ref
    }

    #[cfg(feature = "local-inference")]
    pub(super) async fn wait_ended(&self) {
        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let status = self
                .authority
                .call_local(
                    "Model.LeaseStatus",
                    json!(self.target),
                    Duration::from_millis(200),
                )
                .await
                .and_then(read_status);
            match status {
                Ok(ModelLeaseStatus::Ended) => return,
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "PermissionDenied"
                            | "NotFound"
                            | "AuthenticationRequired"
                            | "RecoveryRequired"
                            | "Fenced"
                            | "PreconditionFailed"
                    ) =>
                {
                    return;
                }
                _ => {}
            }
        }
    }

    /// The current local execution allowance, accounting for time already spent before dispatch.
    pub fn remaining_time(&self) -> Duration {
        let mut remaining = self
            .available
            .saturating_sub(self.received.elapsed())
            .min(wall_remaining(self.expires_at));
        if let Some(deadline) = self.deadline_boottime_ms {
            remaining = remaining.min(Duration::from_millis(
                deadline.saturating_sub(boottime_ms()).max(0) as u64,
            ));
        }
        remaining
    }
}

fn wall_remaining(expires_at: DateTime<Utc>) -> Duration {
    (expires_at - Utc::now()).to_std().unwrap_or_default()
}

fn boottime_ms() -> i64 {
    let clock = clock_gettime(ClockId::Boottime);
    clock
        .tv_sec
        .saturating_mul(1000)
        .saturating_add(clock.tv_nsec / 1_000_000)
}

fn clock_domain(boot_id: &str, time_namespace: &str) -> String {
    sha256(format!("{boot_id}\0{time_namespace}").as_bytes())
}

fn local_clock_domain() -> Option<String> {
    let boot = open(
        "/proc/sys/kernel/random/boot_id",
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .ok()?;
    let mut raw = String::new();
    File::from(boot).take(38).read_to_string(&mut raw).ok()?;
    let boot_id = raw.strip_suffix('\n').unwrap_or(&raw);
    if !uuid::Uuid::parse_str(boot_id)
        .is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == boot_id)
    {
        return None;
    }
    let namespace = match read_link("/proc/thread-self/ns/time") {
        Ok(path) => path
            .to_str()?
            .strip_prefix("time:[")?
            .strip_suffix(']')?
            .to_owned(),
        Err(error) if error.kind() == ErrorKind::NotFound => "unsupported".into(),
        Err(_) => return None,
    };
    Some(clock_domain(boot_id, &namespace))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matching_linux_clock_uses_the_original_cutoff_even_with_a_later_wall_expiration() {
        let timing = ModelLeaseTiming {
            clock_domain: local_clock_domain().expect("read the local Linux clock domain"),
            deadline_boottime_ms: boottime_ms(),
            window_ms: 1,
        };
        let claim = json!({"turn":1,"expires_at":(Utc::now()+chrono::TimeDelta::seconds(5)).to_rfc3339(),
            "lease_timing":timing,"context":{"session_id":"session-test",
            "model_ref":{"resource_id":"model-test","version":"r1","digest":sha256(b"test")}}});
        let authority = Client::new("/unused", "");
        let invocation =
            ModelInvocation::from_claim(&authority, "model-call-test", &claim).unwrap();
        assert!(invocation.deadline_boottime_ms.is_some());
        assert!(invocation.remaining_time().is_zero());
    }
}
