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
const KIND: &str = "TaskResultEvictionBatch";
const MAX_TASKS: usize = 16;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TOKEN: usize = 16 * 1024;
const LIFETIME_SECONDS: i64 = 120;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    task_refs: Vec<ResourceRef>,
    reason: String,
    max_content_bytes: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    plan_token: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlanItem {
    task_ref: ResourceRef,
    content_bytes: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    schema_version: String,
    authority_id: String,
    recovery_incarnation: String,
    authorization_epoch: String,
    owner: String,
    boot_id: String,
    time_namespace: String,
    issued_boot_ms: i64,
    deadline_boot_ms: i64,
    valid_until: String,
    reason: String,
    max_content_bytes: u64,
    content_bytes: u64,
    items: Vec<PlanItem>,
}

impl Plan {
    fn validate(&self) -> Result<()> {
        validate_reason(&self.reason)?;
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

    fn live(&self, db: &Db<'_>) -> Result<()> {
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

fn digest(plan: &Plan) -> Result<String> {
    Ok(sha256(&canonical(&serde_json::to_value(plan)?)))
}

fn receipt_id(digest: &str) -> String {
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

fn open(db: &Db<'_>, actor: &str, token: &str) -> Result<Plan> {
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
    };
    plan.validate()?;
    let token = seal(db, &plan)?;
    db.audit(actor, "Task.PrepareResultEvictionBatch", None, "Prepared")?;
    Ok(
        json!({"plan_token":token,"items":plan.items,"content_bytes":plan.content_bytes,
        "valid_until":plan.valid_until}),
    )
}

fn view(record: &Doc) -> Value {
    json!({"reference":record.reference(),"batch":record.body})
}

pub(crate) fn apply(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let request: Apply = decode(params)?;
    let plan = open(db, actor, &request.plan_token)?;
    let plan_value = serde_json::to_value(&plan)?;
    let plan_digest = digest(&plan)?;
    let id = receipt_id(&plan_digest);
    if let Some(record) = db.find(&id)? {
        require(
            record.kind == KIND
                && record.owner == actor
                && record.rev == 1
                && record.spec["plan"] == plan_value
                && record.body["plan_digest"] == plan_digest,
            "RecoveryRequired",
            "stored batch receipt does not match its prepared plan",
        )?;
        return Ok(view(&record));
    }
    plan.live(db)?;
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
    plan.live(db)?;
    let mut eviction_refs = Vec::with_capacity(candidates.len());
    for next in candidates {
        let receipt = apply_candidate(db, actor, &plan.reason, next)?;
        let reference: ResourceRef = decode(&receipt["reference"])?;
        eviction_refs.push(reference);
    }
    let record = db.create(KIND, &id, actor, json!({"plan":plan_value}), json!({
        "plan_digest":plan_digest,"reason":plan.reason,"eviction_refs":eviction_refs,
        "content_bytes_removed":plan.content_bytes.to_string(),"evicted_by":actor,"evicted_at":now()
    }))?;
    db.audit(actor, "Task.ApplyResultEvictionBatch", Some(&id), "Evicted")?;
    Ok(view(&record))
}
