//! Prepare exact result reclamation and publish one durable batch receipt.
pub(crate) mod verify;

use super::{Candidate, apply_candidate, prepare_candidate, validate_reason};
use crate::{
    Result,
    db::{Db, Doc},
    decode, err, label, now, policy, require,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{ResourceRef, VERSION, canonical, sha256};
use ring::hmac;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::project::hierarchy::FORMAT + 1;
pub(super) const KIND: &str = "TaskResultEvictionBatch";
const MAX_TASKS: usize = 16;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN: usize = 16 * 1024;
const LIFETIME_SECONDS: i64 = 120;
const MAX_DELAY_SECONDS: u32 = 86_400;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    task_refs: Vec<ResourceRef>,
    reason: String,
    max_content_bytes: Option<u64>,
    #[serde(default)]
    delay_seconds: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    plan_token: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanItem {
    pub(super) task_ref: ResourceRef,
    pub(super) content_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    schema_version: String,
    pub(super) authority_id: String,
    pub(super) recovery_incarnation: String,
    pub(super) authorization_epoch: String,
    pub(super) owner: String,
    pub(super) boot_id: String,
    pub(super) time_namespace: String,
    pub(super) issued_boot_ms: i64,
    pub(super) deadline_boot_ms: i64,
    valid_until: String,
    reason: String,
    max_content_bytes: u64,
    pub(super) content_bytes: u64,
    pub(super) items: Vec<PlanItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) delay_seconds: Option<u32>,
}

fn validate_delay(delay: Option<u32>) -> Result<()> {
    require(
        delay.is_none_or(|seconds| (1..=MAX_DELAY_SECONDS).contains(&seconds)),
        "InvalidArgument",
        "result eviction delay must be 1 to 86400 seconds",
    )
}

impl Plan {
    pub(super) fn validate(&self) -> Result<()> {
        validate_reason(&self.reason)?;
        validate_delay(self.delay_seconds)?;
        require(
            self.schema_version == VERSION
                && self.owner == policy::OWNER
                && self.authorization_epoch.parse::<u64>().is_ok()
                && uuid::Uuid::parse_str(&self.boot_id).is_ok()
                && !self.time_namespace.is_empty()
                && self.issued_boot_ms >= 0
                && self.issued_boot_ms.checked_add(LIFETIME_SECONDS * 1000)
                    == Some(self.deadline_boot_ms)
                && chrono::DateTime::parse_from_rfc3339(&self.valid_until).is_ok(),
            "InvalidArgument",
            "invalid eviction plan authority or deadline",
        )?;
        require(
            (1..=MAX_TASKS).contains(&self.items.len())
                && (1..=MAX_BYTES).contains(&self.max_content_bytes)
                && self.content_bytes <= self.max_content_bytes
                && self
                    .items
                    .windows(2)
                    .all(|pair| pair[0].task_ref.resource_id < pair[1].task_ref.resource_id)
                && self
                    .items
                    .iter()
                    .try_fold(0u64, |bytes, item| bytes.checked_add(item.content_bytes))
                    == Some(self.content_bytes),
            "InvalidArgument",
            "invalid eviction plan task scope or byte count",
        )?;
        Ok(())
    }

    pub(super) fn live(&self, db: &Db<'_>) -> Result<()> {
        require(
            self.authorization_epoch == db.meta("epoch")?,
            "Conflict",
            "authorization changed since the eviction plan was prepared",
        )?;
        let clock = policy::deadline::Clock::sample()?;
        require(
            clock.boot.boot_id == self.boot_id
                && clock.time_ns == self.time_namespace
                && (self.issued_boot_ms..self.deadline_boot_ms).contains(&clock.boot.millis)
                && !crate::expired(&self.valid_until)?,
            "DeadlineExceeded",
            "eviction plan expired or belongs to another Linux clock",
        )
    }
}

pub(super) fn digest(plan: &Plan) -> Result<String> {
    Ok(sha256(&canonical(&serde_json::to_value(plan)?)))
}

pub(super) fn receipt_id(digest: &str) -> String {
    format!("result-eviction-batch-{}", &digest[7..])
}

fn signing_key(db: &Db<'_>) -> hmac::Key {
    let root = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
    let domain = hmac::sign(&root, b"psios/task-result-eviction-batch/1");
    hmac::Key::new(hmac::HMAC_SHA256, domain.as_ref())
}

fn seal(db: &Db<'_>, plan: &Plan) -> Result<String> {
    let bytes = canonical(&serde_json::to_value(plan)?);
    let tag = hmac::sign(&signing_key(db), &bytes);
    let token = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(bytes),
        URL_SAFE_NO_PAD.encode(tag)
    );
    require(
        token.len() <= MAX_TOKEN,
        "QuotaExceeded",
        "eviction plan exceeds its size limit",
    )?;
    Ok(token)
}

pub(super) fn open(db: &Db<'_>, actor: &str, token: &str) -> Result<Plan> {
    let bad = || err("InvalidArgument", "invalid or foreign eviction plan token");
    require(
        token.len() <= MAX_TOKEN,
        "InvalidArgument",
        "eviction plan token exceeds its size limit",
    )?;
    let (body, tag) = token.split_once('.').ok_or_else(bad)?;
    let bytes = URL_SAFE_NO_PAD.decode(body).map_err(|_| bad())?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| bad())?;
    hmac::verify(&signing_key(db), &bytes, &tag).map_err(|_| bad())?;
    let plan: Plan = serde_json::from_slice(&bytes).map_err(|_| bad())?;
    plan.validate()?;
    require(
        plan.owner == actor
            && plan.authority_id == db.meta("authority")?
            && plan.recovery_incarnation == db.incarnation,
        "InvalidArgument",
        "invalid or foreign eviction plan token",
    )?;
    Ok(plan)
}

fn candidate(db: &Db<'_>, reference: &ResourceRef) -> Result<Candidate> {
    let task = db.of(&reference.resource_id, "Task")?;
    require(
        task.reference() == serde_json::to_value(reference)?,
        "Conflict",
        "task result revision changed after selection",
    )?;
    prepare_candidate(db, task)
}

pub(crate) fn prepare(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let request: Prepare = decode(params)?;
    validate_reason(&request.reason)?;
    validate_delay(request.delay_seconds)?;
    let max_content_bytes = request.max_content_bytes.unwrap_or(DEFAULT_BYTES);
    let mut task_refs = request.task_refs;
    require(
        (1..=MAX_TASKS).contains(&task_refs.len()) && (1..=MAX_BYTES).contains(&max_content_bytes),
        "InvalidArgument",
        "eviction plans require 1 to 16 tasks and a byte limit of 1 to 67108864",
    )?;
    task_refs.sort_unstable_by(|left, right| left.resource_id.cmp(&right.resource_id));
    require(
        task_refs
            .windows(2)
            .all(|pair| pair[0].resource_id != pair[1].resource_id),
        "InvalidArgument",
        "eviction plan tasks must be distinct",
    )?;
    let mut items = Vec::with_capacity(task_refs.len());
    let mut content_bytes = 0u64;
    for task_ref in task_refs {
        label(&task_ref.resource_id)?;
        let next = candidate(db, &task_ref)?;
        let bytes = u64::try_from(next.bytes()).map_err(|_| {
            err(
                "QuotaExceeded",
                "eviction plan exceeds the selected byte limit",
            )
        })?;
        content_bytes = content_bytes
            .checked_add(bytes)
            .filter(|total| *total <= max_content_bytes)
            .ok_or_else(|| {
                err(
                    "QuotaExceeded",
                    "eviction plan exceeds the selected byte limit",
                )
            })?;
        items.push(PlanItem {
            task_ref,
            content_bytes: bytes,
        });
    }
    let clock = policy::deadline::Clock::sample()?;
    let plan = Plan {
        schema_version: VERSION.into(),
        authority_id: db.meta("authority")?,
        recovery_incarnation: db.incarnation.clone(),
        authorization_epoch: db.meta("epoch")?,
        owner: actor.into(),
        boot_id: clock.boot.boot_id,
        time_namespace: clock.time_ns,
        issued_boot_ms: clock.boot.millis,
        deadline_boot_ms: clock.boot.millis.saturating_add(LIFETIME_SECONDS * 1000),
        valid_until: crate::after(LIFETIME_SECONDS),
        reason: request.reason,
        max_content_bytes,
        content_bytes,
        items,
        delay_seconds: request.delay_seconds,
    };
    plan.validate()?;
    let token = seal(db, &plan)?;
    db.audit(actor, "Task.PrepareResultEvictionBatch", None, "Prepared")?;
    let mut response = json!({
        "plan_token":token, "items":plan.items, "content_bytes":plan.content_bytes,
        "valid_until":plan.valid_until
    });
    if let Some(delay) = plan.delay_seconds {
        response["delay_seconds"] = json!(delay);
    }
    Ok(response)
}

fn view(record: &Doc) -> Value {
    json!({"reference":record.reference(),"batch":record.body})
}

pub(crate) fn apply(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let request: Apply = decode(params)?;
    let plan = open(db, actor, &request.plan_token)?;
    require(
        plan.delay_seconds.is_none(),
        "PreconditionFailed",
        "delayed result eviction requires scheduling",
    )?;
    let plan_value = serde_json::to_value(&plan)?;
    let plan_digest = digest(&plan)?;
    let id = receipt_id(&plan_digest);
    if let Some(record) = db.find(&id)? {
        require(
            record.kind == KIND
                && record.owner == actor
                && record.rev == 1
                && record.spec["plan"] == plan_value
                && record.body["plan_digest"] == plan_digest
                && record.body.get("schedule_ref").is_none(),
            "RecoveryRequired",
            "stored batch receipt does not match its prepared plan",
        )?;
        return Ok(view(&record));
    }
    plan.live(db)?;
    let candidates = check_candidates(db, &plan)?;
    plan.live(db)?;
    publish_batch(db, &plan, candidates, None)
}

pub(super) fn check_candidates(db: &Db<'_>, plan: &Plan) -> Result<Vec<Candidate>> {
    let mut candidates = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        let next = candidate(db, &item.task_ref)?;
        require(
            next.bytes() == u128::from(item.content_bytes),
            "RecoveryRequired",
            "candidate bytes changed without a task revision",
        )?;
        candidates.push(next);
    }
    Ok(candidates)
}

/// Applies the prepared batch within the same transaction as schedule settlement.
/// The caller must propagate errors so that partial payload removal rolls back.
pub(super) fn apply_delayed(
    db: &Db<'_>,
    plan: &Plan,
    candidates: Vec<Candidate>,
    schedule: &Doc,
) -> Result<Value> {
    plan.validate()?;
    let delay = plan.delay_seconds.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "scheduled eviction has no approved delay",
        )
    })?;
    let plan_digest = digest(plan)?;
    let schedule_ref = schedule.reference();
    let admitted = schedule.body["admitted_boot_ms"].as_i64();
    let due = schedule.body["due_boot_ms"].as_i64();
    require(
        schedule.kind == super::delayed::KIND
            && schedule.id == super::delayed::schedule_id(&plan_digest)
            && schedule.rev == 1
            && schedule.owner == plan.owner
            && schedule.spec == json!({"plan":plan})
            && schedule.body["plan_digest"] == plan_digest
            && schedule.body["lifecycle"] == "Scheduled"
            && db.get(&schedule.id)?.reference() == schedule_ref
            && admitted.is_some_and(|accepted| {
                (plan.issued_boot_ms..plan.deadline_boot_ms).contains(&accepted)
                    && accepted
                        .checked_add(i64::from(delay) * 1000)
                        .is_some_and(|deadline| Some(deadline) == due)
            }),
        "RecoveryRequired",
        "scheduled eviction does not bind the exact accepted plan",
    )?;
    let clock = policy::deadline::Clock::sample()?;
    require(
        plan.authority_id == db.meta("authority")?
            && plan.recovery_incarnation == db.incarnation
            && plan.authorization_epoch == db.meta("epoch")?
            && plan.boot_id == clock.boot.boot_id
            && plan.time_namespace == clock.time_ns
            && due.is_some_and(|deadline| clock.boot.millis >= deadline),
        "PreconditionFailed",
        "scheduled eviction no longer has live and due authority",
    )?;
    publish_batch(db, plan, candidates, Some(schedule_ref))
}

fn publish_batch(
    db: &Db<'_>,
    plan: &Plan,
    candidates: Vec<Candidate>,
    schedule_ref: Option<Value>,
) -> Result<Value> {
    require(
        candidates.len() == plan.items.len()
            && candidates.iter().zip(&plan.items).all(|(candidate, item)| {
                candidate.task.reference() == json!(item.task_ref)
                    && candidate.bytes() == u128::from(item.content_bytes)
            }),
        "RecoveryRequired",
        "eviction candidates no longer match the approved batch",
    )?;
    let mut eviction_refs = Vec::with_capacity(candidates.len());
    for next in candidates {
        let receipt = apply_candidate(db, &plan.owner, &plan.reason, next)?;
        let reference: ResourceRef = decode(&receipt["reference"])?;
        eviction_refs.push(reference);
    }
    let plan_digest = digest(plan)?;
    let id = receipt_id(&plan_digest);
    let mut body = json!({
        "plan_digest":plan_digest,"reason":plan.reason,"eviction_refs":eviction_refs,
        "content_bytes_removed":plan.content_bytes.to_string(),"evicted_by":plan.owner,"evicted_at":now()
    });
    let method = if let Some(reference) = schedule_ref {
        body["schedule_ref"] = reference;
        "Task.ExecuteScheduledResultEvictionBatch"
    } else {
        "Task.ApplyResultEvictionBatch"
    };
    let record = db.create(KIND, &id, &plan.owner, json!({"plan":plan}), body)?;
    db.audit(&plan.owner, method, Some(&id), "Evicted")?;
    Ok(view(&record))
}
