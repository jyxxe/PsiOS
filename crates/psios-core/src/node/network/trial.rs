//! Custody for one locally approved NetworkManager profile trial.
mod activation_receipt;
pub(crate) mod rollback_verify;

pub(crate) use super::managed::RollbackReceipt;
use super::managed::{self, Activation, Checkpoint, Observation, Snapshot};
use crate::{
    Result,
    db::{Db, Doc, find, resolve},
    decode, err, expired, host,
    host::{Action, Target, Work},
    policy::{self, deadline::Clock},
    require,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{canonical, id, sha256};
use ring::hmac;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::node::service_manager::FORMAT + 1;
pub(crate) const ROLLBACK_FORMAT: u32 = crate::project::priority::FORMAT + 1;
pub(crate) const TOOL: &str = "network.trial";
pub(crate) const INSPECT: &str = "network.inspect_trial";
pub(crate) const WAITING: &str = "WaitingInput";
const JOURNAL: &str = "NetworkTrialJournal";
const ROLLBACK_RECEIPT: &str = "NetworkTrialRollbackReceipt";
const BOOT_MARGIN_MS: i64 = 3_000;

#[derive(Deserialize)]
struct Enrollment {
    interface: String,
    mac: String,
    original_uuid: String,
    candidate_uuid: String,
    confirmation_seconds: u32,
}

impl Enrollment {
    fn read(link: &Doc) -> Result<Self> {
        let enrollment: Self = decode(&link.spec)?;
        require(
            link.kind == "NetworkLink"
                && link.owner == policy::SYSTEM
                && link.body["lifecycle"] == "Registered"
                && enrollment.original_uuid != enrollment.candidate_uuid
                && (5..=120).contains(&enrollment.confirmation_seconds),
            "PreconditionFailed",
            "network link is outside the enrolled trial contract",
        )?;
        Ok(enrollment)
    }
}

pub(crate) fn target(db: &Db<'_>, actor: &str, target: &Target) -> Result<Doc> {
    policy::admin(actor)?;
    let link = db.of(&target.id, "NetworkLink")?;
    link.cas(&target.expected_resource_version)?;
    Enrollment::read(&link)?;
    super::enrollment::require_configured(db, &link)?;
    Ok(link)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    PermitBound,
    CreateIssued,
    CheckpointOwned,
    ActivateIssued,
    Activated,
    AwaitingConfirmation,
    ConfirmIssued,
    RollbackIssued,
    Committed,
    RolledBack,
    Aborted,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Deadline {
    boot_id: String,
    time_namespace: String,
    boot_millis: i64,
    expires_at: String,
}

impl Deadline {
    fn capture(started: i64, clock: Clock, timeout: u32, permit_expires: &str) -> Result<Self> {
        let utc = chrono::Utc::now() + chrono::Duration::seconds(i64::from(timeout));
        let permit = chrono::DateTime::parse_from_rfc3339(permit_expires)
            .map_err(|_| err("RecoveryRequired", "invalid trial permit deadline"))?
            .to_utc();
        require(
            started <= clock.boot.millis
                && clock.boot.millis.saturating_sub(started) < BOOT_MARGIN_MS,
            "DeadlineExceeded",
            "network checkpoint dispatch clock sample is stale",
        )?;
        Ok(Self {
            boot_id: clock.boot.boot_id,
            time_namespace: clock.time_ns,
            boot_millis: started.saturating_add(i64::from(timeout) * 1000),
            expires_at: utc
                .min(permit)
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        })
    }

    fn expired(&self, margin_ms: i64) -> Result<bool> {
        let clock = Clock::sample()?;
        Ok(clock.boot.boot_id != self.boot_id
            || clock.time_ns != self.time_namespace
            || clock.boot.millis.saturating_add(margin_ms) >= self.boot_millis
            || expired(&self.expires_at)?)
    }

    fn live(&self) -> Result<()> {
        require(
            !self.expired(BOOT_MARGIN_MS)?,
            "DeadlineExceeded",
            "network trial confirmation window closed",
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    phase: Phase,
    link_ref: Value,
    operation_ref: Value,
    task_ref: Value,
    run_ref: Value,
    effect_ref: Value,
    epoch: String,
    incarnation: String,
    deadline: Option<Deadline>,
    started_boottime_ms: Option<i64>,
    snapshot: Option<Snapshot>,
    checkpoint: Option<Checkpoint>,
    activation: Option<Activation>,
    confirmation_attempt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback_issue: Option<RollbackIssue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollback_receipt_ref: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackIssue {
    attempt: String,
    operation_ref: Value,
    previous_journal_ref: Value,
}

fn rollback_receipt_id(operation: &str) -> String {
    format!("network-trial-rollback-{operation}")
}

struct Custody {
    document: Doc,
    journal: Journal,
}

impl Custody {
    fn load(db: &Db<'_>, op: &Doc) -> Result<Self> {
        Self::read(db.c, op)
    }

    fn read(conn: &Connection, op: &Doc) -> Result<Self> {
        let document = needed(conn, &journal_id(&op.id))?;
        let journal: Journal = decode(&document.body)?;
        let old_op = resolve(conn, &journal.operation_ref)?;
        let old_task = resolve(conn, &journal.task_ref)?;
        let old_run = resolve(conn, &journal.run_ref)?;
        let old_effect = resolve(conn, &journal.effect_ref)?;
        let link = resolve(conn, &journal.link_ref)?;
        let held: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
            params![link.id, op.id],
            |row| row.get(0),
        )?;
        require(
            held && document.kind == JOURNAL
                && document.owner == op.owner
                && document.spec["operation_id"] == op.id
                && document.spec["link_id"] == link.id
                && link.kind == "NetworkLink"
                && old_op.kind == "Operation"
                && old_op.id == op.id
                && old_op.owner == op.owner
                && old_op.spec == op.spec
                && old_op.body["plan_ref"] == op.body["plan_ref"]
                && old_op.body["effect_refs"][0] == journal.effect_ref
                && old_task.kind == "Task"
                && old_task.id == op.spec["task_id"]
                && old_task.owner == op.owner
                && old_task.spec["task_type"] == TOOL
                && old_task.body["current_run_id"] == old_run.id
                && old_task.body["operation_refs"][0] == journal.operation_ref
                && old_run.kind == "Run"
                && old_run.owner == op.owner
                && old_run.body["task_id"] == old_task.id
                && old_run.body["operation_refs"][0] == journal.operation_ref
                && old_effect.kind == "Effect"
                && old_effect.owner == op.owner
                && old_effect.body["operation_id"] == op.id
                && old_effect.body["action"] == TOOL
                && old_effect.body["resource_ref"] == journal.link_ref
                && old_effect.body["phase"] == "PermitBound"
                && op.body["effect_refs"][0]["resource_id"] == old_effect.id,
            "RecoveryRequired",
            "network trial custody does not match its original effect and link hold",
        )?;
        Ok(Self { document, journal })
    }

    fn save(self, db: &Db<'_>) -> Result<()> {
        db.status(&self.document, serde_json::to_value(self.journal)?)?;
        Ok(())
    }

    fn pair(&self) -> Result<(&Snapshot, &Checkpoint)> {
        match (&self.journal.snapshot, &self.journal.checkpoint) {
            (Some(scope), Some(checkpoint)) => Ok((scope, checkpoint)),
            _ => Err(err(
                "OutcomeUnknown",
                "exact NetworkManager checkpoint receipt is unavailable",
            )),
        }
    }

    fn activation(&self) -> Result<&Activation> {
        self.journal.activation.as_ref().ok_or_else(|| {
            err(
                "OutcomeUnknown",
                "exact NetworkManager activation receipt is unavailable",
            )
        })
    }

    fn deadline(&self) -> Result<&Deadline> {
        self.journal
            .deadline
            .as_ref()
            .ok_or_else(|| err("RecoveryRequired", "network trial has no recorded deadline"))
    }

    fn authorize(&self, db: &Db<'_>, op: &Doc, task: &Doc) -> Result<()> {
        require(
            self.journal.epoch == db.meta("epoch")?
                && self.journal.incarnation == db.incarnation
                && task.body["current_run_id"] == self.journal.run_ref["resource_id"]
                && task.body["operation_refs"][0] == op.reference(),
            "Fenced",
            "network trial permit or worker claim is no longer current",
        )?;
        let link = db.resolve(&self.journal.link_ref)?;
        let input = Target {
            id: link.id.clone(),
            expected_resource_version: link.version(),
        };
        let action = Action::NetworkTrial(input);
        let target = action.target(db, &task.owner)?;
        require(
            target.reference() == self.journal.link_ref,
            "Fenced",
            "network link revision changed",
        )?;
        let delegation = &task.spec["delegation_ref"];
        let audience = action.audience(db, &task.owner, delegation, &target)?;
        let run = db.of(
            task.body["current_run_id"].as_str().unwrap_or_default(),
            "Run",
        )?;
        let effect = db.resolve(&self.journal.effect_ref)?;
        host::permit(db, task, &run, op, &effect, &audience)?;
        let permit = db.resolve(&effect.spec["permit_ref"])?;
        let decision = db.resolve(&permit.body["decision_ref"])?;
        require(
            !expired(decision.body["valid_until"].as_str().unwrap_or_default())?,
            "ApprovalInvalidated",
            "network trial approval expired",
        )
    }
}

fn journal_id(operation: &str) -> String {
    format!("network-trial-{operation}")
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    Enrollment::read(&work.target)?;
    require(
        db.c.execute(
            "INSERT OR IGNORE INTO holds(volume,holder) VALUES(?1,?2)",
            params![work.target.id, work.op.id],
        )? == 1,
        "Conflict",
        "network link already has an active or unresolved trial",
    )?;
    let journal = Journal {
        phase: Phase::PermitBound,
        link_ref: work.target.reference(),
        operation_ref: work.op.reference(),
        task_ref: work.task.reference(),
        run_ref: work.run.reference(),
        effect_ref: work.effect.reference(),
        epoch: db.meta("epoch")?,
        incarnation: db.incarnation.clone(),
        deadline: None,
        started_boottime_ms: None,
        snapshot: None,
        checkpoint: None,
        activation: None,
        confirmation_attempt: None,
        rollback_issue: None,
        rollback_receipt_ref: None,
    };
    db.create(
        JOURNAL,
        &journal_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id, "link_id":work.target.id}),
        serde_json::to_value(journal)?,
    )?;
    Ok(())
}

pub(crate) enum Boundary<'a> {
    BeforeCreate(&'a Snapshot, i64),
    Created(&'a Checkpoint),
    BeforeActivate,
    Activated(&'a Activation),
    ActivationReceiptReturned(&'a Activation),
}

pub(crate) fn perform(
    target: &Doc,
    mut checkpoint: impl FnMut(Boundary<'_>) -> Result<()>,
) -> Result<Value> {
    let settings = Enrollment::read(target)?;
    let scope = managed::snapshot(
        &settings.interface,
        [&settings.original_uuid, &settings.candidate_uuid],
    )?;
    require(
        scope.baseline.profile_uuid == settings.original_uuid && scope.link.mac == settings.mac,
        "PreconditionFailed",
        "registered original profile or Ethernet identity is not active",
    )?;
    let nm_checkpoint = managed::checkpoint(&scope, settings.confirmation_seconds, |started| {
        checkpoint(Boundary::BeforeCreate(&scope, started))
    })?;
    checkpoint(Boundary::Created(&nm_checkpoint))?;
    let activation = managed::activate(&scope, &nm_checkpoint, &settings.candidate_uuid, || {
        checkpoint(Boundary::BeforeActivate)
    })?;
    if let Err(error) = checkpoint(Boundary::Activated(&activation)) {
        checkpoint(Boundary::ActivationReceiptReturned(&activation))?;
        return Err(error);
    }
    let observed = managed::await_candidate(&scope, &nm_checkpoint, &activation)?;
    require(
        observed.checkpoint_present && observed.active == activation.active,
        "OutcomeUnknown",
        "network trial candidate is not the active checkpointed connection",
    )?;
    Ok(serde_json::to_value(observed)?)
}

pub(crate) fn record(db: &Db<'_>, work: &Work, boundary: Boundary<'_>) -> Result<()> {
    let op = db.of(&work.op.id, "Operation")?;
    let task = db.of(&work.task.id, "Task")?;
    let mut custody = Custody::load(db, &op)?;
    let phase: String =
        db.c.query_row("SELECT phase FROM queue WHERE task=?1", [&task.id], |r| {
            r.get(0)
        })?;
    require(
        phase == "Dispatched"
            && task.body["current_run_id"] == work.run.id
            && task.body["outcome_knowledge"] == "InFlight"
            && task.body["operation_refs"][0] == op.reference()
            && custody.journal.run_ref == work.run.reference()
            && custody.journal.effect_ref == work.effect.reference(),
        "Fenced",
        "network trial effect no longer owns its worker lease",
    )?;
    let authority = |authority: &Custody| -> Result<()> {
        require(
            task.body["lifecycle"] == "Running",
            "Fenced",
            "network trial was cancelled",
        )?;
        authority.authorize(db, &op, &task)
    };
    custody.journal.phase = match (custody.journal.phase, boundary) {
        (Phase::PermitBound, Boundary::BeforeCreate(scope, started)) => {
            authority(&custody)?;
            let enrollment = Enrollment::read(&work.target)?;
            require(
                scope.link.interface == enrollment.interface
                    && scope.link.mac == enrollment.mac
                    && scope.baseline.profile_uuid == enrollment.original_uuid
                    && scope.profiles[0].uuid == enrollment.original_uuid
                    && scope.profiles[1].uuid == enrollment.candidate_uuid,
                "Fenced",
                "checkpoint scope differs from the enrolled link",
            )?;
            let permit = db.resolve(&work.effect.spec["permit_ref"])?;
            let clock = Clock::sample()?;
            require(
                clock.boot.boot_id == scope.identity.boot_id,
                "Fenced",
                "network snapshot boot changed",
            )?;
            let deadline = Deadline::capture(
                started,
                clock,
                enrollment.confirmation_seconds,
                permit.body["not_after"].as_str().unwrap_or_default(),
            )?;
            deadline.live()?;
            custody.journal.snapshot = Some(scope.clone());
            custody.journal.deadline = Some(deadline);
            custody.journal.started_boottime_ms = Some(started);
            start_effect(db, &op, &task, work)?;
            Phase::CreateIssued
        }
        (Phase::CreateIssued, Boundary::Created(checkpoint)) => {
            let scope = custody
                .journal
                .snapshot
                .as_ref()
                .ok_or_else(|| err("RecoveryRequired", "trial snapshot absent"))?;
            let digest = sha256(&canonical(&serde_json::to_value(scope)?));
            let enrollment = Enrollment::read(&work.target)?;
            require(
                checkpoint.snapshot_digest == digest
                    && checkpoint.rollback_timeout_seconds == enrollment.confirmation_seconds
                    && custody
                        .journal
                        .started_boottime_ms
                        .is_some_and(|start| start <= checkpoint.created_boottime_ms)
                    && checkpoint
                        .created_boottime_ms
                        .checked_add(i64::from(checkpoint.rollback_timeout_seconds) * 1000)
                        == Some(checkpoint.deadline_boottime_ms),
                "Fenced",
                "NetworkManager checkpoint receipt differs from the issued scope",
            )?;
            custody.journal.checkpoint = Some(checkpoint.clone());
            Phase::CheckpointOwned
        }
        (Phase::CheckpointOwned, Boundary::BeforeActivate) => {
            authority(&custody)?;
            custody.deadline()?.live()?;
            Phase::ActivateIssued
        }
        (Phase::ActivateIssued, Boundary::Activated(activation)) => {
            let (_scope, cp) = custody.pair()?;
            require(
                cp.path == activation.checkpoint_path
                    && activation.active.profile_uuid
                        == Enrollment::read(&work.target)?.candidate_uuid,
                "Fenced",
                "NetworkManager activation receipt differs from the issued scope",
            )?;
            custody.journal.activation = Some(activation.clone());
            Phase::Activated
        }
        (Phase::ActivateIssued, Boundary::ActivationReceiptReturned(activation)) => {
            return activation_receipt::record(db, &op, &custody, activation);
        }
        _ => {
            return Err(err(
                "Fenced",
                "network trial invocation boundary already changed",
            ));
        }
    };
    custody.save(db)
}

fn start_effect(db: &Db<'_>, op: &Doc, task: &Doc, work: &Work) -> Result<()> {
    require(
        op.body["effect_refs"][0] == work.effect.reference(),
        "Fenced",
        "network trial effect is already issued",
    )?;
    let mut body = work.effect.body.clone();
    body["phase"] = json!("EffectStarted");
    let effect = db.status(&work.effect, body)?;
    let mut body = op.body.clone();
    body["effect_refs"] = json!([effect.reference()]);
    let op = db.status(op, body)?;
    let mut body = task.body.clone();
    body["operation_refs"] = json!([op.reference()]);
    db.status(task, body)?;
    let run = db.of(&work.run.id, "Run")?;
    let mut body = run.body.clone();
    body["operation_refs"] = json!([op.reference()]);
    db.status(&run, body)?;
    Ok(())
}

pub(crate) fn not_started(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let mut custody = Custody::load(db, op)?;
    if custody.journal.phase != Phase::PermitBound {
        return Ok(false);
    }
    require(
        op.body["effect_refs"][0] == custody.journal.effect_ref,
        "RecoveryRequired",
        "unstarted network trial has a dispatched effect",
    )?;
    custody.journal.phase = Phase::Aborted;
    custody.save(db)?;
    Ok(true)
}

pub(crate) fn park(db: &Db<'_>, work: &Work, receipt: Value) -> Result<()> {
    let op = db.of(&work.op.id, "Operation")?;
    let task = db.of(&work.task.id, "Task")?;
    let run = db.of(&work.run.id, "Run")?;
    let mut custody = Custody::load(db, &op)?;
    let receipt: Observation = decode(&receipt)?;
    require(
        custody.journal.phase == Phase::Activated
            && task.body["lifecycle"] == "Running"
            && run.body["lifecycle"] == "Running"
            && task.body["operation_refs"][0] == op.reference()
            && run.body["operation_refs"][0] == op.reference()
            && custody.journal.run_ref == work.run.reference()
            && receipt.checkpoint_present
            && custody.activation()?.active == receipt.active,
        "Fenced",
        "network activation receipt no longer owns the trial",
    )?;
    custody.deadline()?.live()?;
    custody.authorize(db, &op, &task)?;
    require(
        db.c.execute(
            "UPDATE queue SET phase='WaitingInput' WHERE task=?1 AND phase='Dispatched'",
            [&task.id],
        )? == 1,
        "Fenced",
        "network trial queue ownership changed",
    )?;
    let condition = crate::broker::condition(
        "AwaitingConfirmation",
        "locally authenticate confirmation before the checkpoint window closes",
    );
    let mut body = op.body.clone();
    body["lifecycle"] = json!("WaitingDependency");
    body["updated_at"] = json!(crate::now());
    body["conditions"] = json!([condition]);
    let op = db.status(&op, body)?;
    for subject in [&task, &run] {
        let mut body = subject.body.clone();
        body["lifecycle"] = json!(WAITING);
        body["operation_refs"] = json!([op.reference()]);
        body["conditions"] = json!([condition]);
        db.status(subject, body)?;
    }
    custody.journal.phase = Phase::AwaitingConfirmation;
    custody.save(db)?;
    db.audit(&task.owner, TOOL, Some(&op.id), "AwaitingConfirmation")
}

pub(crate) fn parked_tasks(db: &Db<'_>) -> Result<Vec<String>> {
    Ok(db
        .c
        .prepare(
            "SELECT q.task FROM queue q JOIN current t ON q.task=t.id
         WHERE q.phase='WaitingInput' AND t.spec->>'task_type'=?1",
        )?
        .query_map([TOOL], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn should_expire(db: &Db<'_>, op: &Doc, task: &Doc) -> Result<bool> {
    let custody = Custody::load(db, op)?;
    match custody.journal.phase {
        Phase::AwaitingConfirmation => {
            Ok(custody.deadline()?.expired(0)? || custody.authorize(db, op, task).is_err())
        }
        Phase::ConfirmIssued | Phase::RollbackIssued => custody.deadline()?.expired(0),
        _ => Err(err(
            "RecoveryRequired",
            "parked network trial has an invalid custody phase",
        )),
    }
}

pub(crate) fn terminal_issued(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(matches!(
        Custody::load(db, op)?.journal.phase,
        Phase::ConfirmIssued | Phase::RollbackIssued
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    id: String,
    expected_resource_version: String,
    confirmation_token: String,
}

pub(crate) type ConfirmationReceipt = Observation;

pub(crate) struct Confirmation {
    operation_id: String,
    operation_ref: Value,
    journal_ref: Value,
    attempt: String,
    snapshot: Snapshot,
    checkpoint: Checkpoint,
    activation: Activation,
}

impl Confirmation {
    pub(crate) fn perform(
        &self,
        before_effect: impl FnOnce() -> Result<()>,
    ) -> Result<ConfirmationReceipt> {
        managed::commit(
            &self.snapshot,
            &self.checkpoint,
            &self.activation,
            before_effect,
        )
    }
}

fn awaiting(db: &Db<'_>, actor: &str, id: &str, version: &str) -> Result<(Doc, Custody)> {
    policy::admin(actor)?;
    let op = db.of(id, "Operation")?;
    op.cas(version)?;
    require(
        op.owner == actor
            && op.body["lifecycle"] == "WaitingDependency"
            && op.body["outcome_knowledge"] == "InFlight",
        "PreconditionFailed",
        "network trial is no longer awaiting this terminal decision",
    )?;
    let custody = Custody::load(db, &op)?;
    let task = db.of(op.spec["task_id"].as_str().unwrap_or_default(), "Task")?;
    let phase: String =
        db.c.query_row("SELECT phase FROM queue WHERE task=?1", [&task.id], |row| {
            row.get(0)
        })?;
    require(
        phase == WAITING
            && task.body["lifecycle"] == WAITING
            && custody.journal.phase == Phase::AwaitingConfirmation,
        "PreconditionFailed",
        "network trial is no longer awaiting this confirmation",
    )?;
    custody.deadline()?.live()?;
    custody.authorize(db, &op, &task)?;
    Ok((op, custody))
}

fn token_data(op: &Doc, custody: &Custody) -> Vec<u8> {
    canonical(&json!([
        "psios-network-trial-confirmation-v1",
        op.reference(),
        custody.document.reference(),
        custody.journal.link_ref,
        custody.journal.epoch,
        custody.journal.incarnation,
        custody.journal.deadline,
    ]))
}

pub(crate) fn challenge(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Target = decode(params)?;
    let (op, custody) = awaiting(db, actor, &request.id, &request.expected_resource_version)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, db.keys);
    let signed = hmac::sign(&key, &token_data(&op, &custody));
    Ok(json!({"operation_ref":op.reference(),
        "confirmation_token":URL_SAFE_NO_PAD.encode(signed.as_ref()),
        "expires_at":custody.deadline()?.expires_at}))
}

fn confirmation(db: &Db<'_>, actor: &str, params: &Value) -> Result<(Doc, Custody)> {
    let request: Confirm = decode(params)?;
    let (op, custody) = awaiting(db, actor, &request.id, &request.expected_resource_version)?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, db.keys);
    require(
        request.confirmation_token.len() == 43
            && URL_SAFE_NO_PAD
                .decode(&request.confirmation_token)
                .is_ok_and(|token| hmac::verify(&key, &token_data(&op, &custody), &token).is_ok()),
        "PermissionDenied",
        "network trial confirmation token is invalid",
    )?;
    Ok((op, custody))
}

pub(crate) fn prepare_confirm(db: &Db<'_>, actor: &str, params: &Value) -> Result<Confirmation> {
    let (op, custody) = confirmation(db, actor, params)?;
    let (snapshot, checkpoint) = custody.pair()?;
    Ok(Confirmation {
        operation_id: op.id.clone(),
        operation_ref: op.reference(),
        journal_ref: custody.document.reference(),
        attempt: id("network-confirm"),
        snapshot: snapshot.clone(),
        checkpoint: checkpoint.clone(),
        activation: custody.activation()?.clone(),
    })
}

pub(crate) fn authorize_confirm(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    job: &Confirmation,
) -> Result<()> {
    let (op, mut custody) = confirmation(db, actor, params)?;
    require(
        op.reference() == job.operation_ref && custody.document.reference() == job.journal_ref,
        "Fenced",
        "network confirmation lease changed during live validation",
    )?;
    custody.journal.phase = Phase::ConfirmIssued;
    custody.journal.confirmation_attempt = Some(job.attempt.clone());
    custody.save(db)?;
    db.audit(actor, "NetworkTrial.Confirm", Some(&op.id), "Dispatched")
}

pub(crate) fn confirm_result(
    db: &Db<'_>,
    actor: &str,
    job: &Confirmation,
    result: Result<ConfirmationReceipt>,
) -> Result<(Doc, Option<Value>)> {
    policy::admin(actor)?;
    let op = db.of(&job.operation_id, "Operation")?;
    let custody = Custody::load(db, &op)?;
    require(
        op.reference() == job.operation_ref
            && op.owner == actor
            && op.body["outcome_knowledge"] == "InFlight",
        "Fenced",
        "network confirmation receipt is no longer current",
    )?;
    if custody.journal.phase == Phase::AwaitingConfirmation {
        return match result {
            Err(error) => Err(error),
            Ok(_) => Err(err(
                "Fenced",
                "network confirmation has no committed invocation",
            )),
        };
    }
    require(
        custody.journal.phase == Phase::ConfirmIssued
            && custody.journal.confirmation_attempt.as_deref() == Some(job.attempt.as_str()),
        "Fenced",
        "network confirmation receipt does not own the invocation",
    )?;
    let settled = match result {
        Ok(receipt) if !receipt.checkpoint_present && receipt.active == job.activation.active => {
            Some(json!({"publication":"Committed","link_ref":custody.journal.link_ref}))
        }
        _ => None,
    };
    Ok((op, settled))
}

pub(crate) struct Rollback {
    operation_id: String,
    issue: RollbackIssue,
    snapshot: Snapshot,
    checkpoint: Checkpoint,
    activation: Activation,
}

impl Rollback {
    pub(crate) fn perform(
        &self,
        before_effect: impl FnOnce() -> Result<()>,
    ) -> Result<RollbackReceipt> {
        managed::rollback(
            &self.snapshot,
            &self.checkpoint,
            &self.activation,
            before_effect,
        )
    }
}

pub(crate) fn prepare_rollback(db: &Db<'_>, actor: &str, params: &Value) -> Result<Rollback> {
    let request: Target = decode(params)?;
    let (op, custody) = awaiting(db, actor, &request.id, &request.expected_resource_version)?;
    let (snapshot, checkpoint) = custody.pair()?;
    Ok(Rollback {
        issue: RollbackIssue {
            attempt: id("network-rollback"),
            operation_ref: op.reference(),
            previous_journal_ref: custody.document.reference(),
        },
        operation_id: op.id,
        snapshot: snapshot.clone(),
        checkpoint: checkpoint.clone(),
        activation: custody.activation()?.clone(),
    })
}

pub(crate) fn authorize_rollback(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    job: &Rollback,
) -> Result<()> {
    let request: Target = decode(params)?;
    let (op, mut custody) = awaiting(db, actor, &request.id, &request.expected_resource_version)?;
    require(
        op.reference() == job.issue.operation_ref
            && custody.document.reference() == job.issue.previous_journal_ref,
        "Fenced",
        "network rollback lease changed during live validation",
    )?;
    custody.journal.phase = Phase::RollbackIssued;
    custody.journal.rollback_issue = Some(job.issue.clone());
    custody.save(db)?;
    db.audit(actor, "NetworkTrial.Rollback", Some(&op.id), "Dispatched")
}

pub(crate) fn rollback_result(
    db: &Db<'_>,
    actor: &str,
    job: &Rollback,
    result: Result<RollbackReceipt>,
) -> Result<(Doc, Option<(RollbackReceipt, Value)>)> {
    policy::admin(actor)?;
    let op = db.of(&job.operation_id, "Operation")?;
    require(
        op.reference() == job.issue.operation_ref
            && op.owner == actor
            && op.body["outcome_knowledge"] == "InFlight",
        "Fenced",
        "network rollback receipt is no longer current",
    )?;
    let custody = Custody::load(db, &op)?;
    if custody.journal.phase == Phase::AwaitingConfirmation {
        return match result {
            Err(error) => Err(error),
            Ok(_) => Err(err(
                "Fenced",
                "network rollback has no committed invocation",
            )),
        };
    }
    require(
        custody.journal.phase == Phase::RollbackIssued
            && custody.journal.rollback_issue.as_ref() == Some(&job.issue),
        "Fenced",
        "network rollback receipt does not own the invocation",
    )?;
    let settled = result
        .ok()
        .filter(|receipt| receipt.matches(&job.snapshot))
        .map(|receipt| {
            (
                receipt,
                json!({"publication":"RolledBack","link_ref":custody.journal.link_ref}),
            )
        });
    Ok((op, settled))
}

pub(crate) fn record_rollback(
    db: &Db<'_>,
    op: &Doc,
    job: &Rollback,
    acknowledgement: &RollbackReceipt,
    artifact: &Doc,
) -> Result<()> {
    let mut custody = Custody::load(db, op)?;
    require(
        custody.journal.phase == Phase::RollbackIssued
            && custody.journal.rollback_issue.as_ref() == Some(&job.issue)
            && custody.journal.rollback_receipt_ref.is_none(),
        "Fenced",
        "network rollback no longer occupies its original invocation",
    )?;
    let record = db.create(
        ROLLBACK_RECEIPT,
        &rollback_receipt_id(&op.id),
        &op.owner,
        json!({"operation_id":op.id}),
        json!({"operation_ref":op.reference(),"issued_journal_ref":custody.document.reference(),
            "link_ref":custody.journal.link_ref,"result_artifact_ref":artifact.reference(),
            "attempt":job.issue.attempt,"acknowledgement":acknowledgement}),
    )?;
    custody.journal.phase = Phase::RolledBack;
    custody.journal.rollback_receipt_ref = Some(record.reference());
    custody.save(db)
}

pub(crate) fn mark_settled(db: &Db<'_>, op: &Doc, committed: bool) -> Result<()> {
    let mut custody = Custody::load(db, op)?;
    custody.journal.phase = if committed {
        Phase::Committed
    } else {
        Phase::RolledBack
    };
    custody.save(db)
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
enum Conclusion {
    Committed,
    RolledBack,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Proof {
    operation_ref: Value,
    journal_ref: Value,
    link_ref: Value,
    observed: Observation,
    publication: Conclusion,
}

pub(crate) struct Inspection {
    operation_ref: Value,
    journal_ref: Value,
    link_ref: Value,
    snapshot: Snapshot,
    checkpoint: Checkpoint,
    activation: Option<Activation>,
    activation_receipt_ref: Option<Value>,
    confirm_issued: bool,
}

pub(crate) struct ReconcileSample(Proof);

impl Inspection {
    fn read(&self) -> Result<Proof> {
        let observed =
            managed::observe_recovery(&self.snapshot, &self.checkpoint, self.activation.as_ref())?;
        require(
            !observed.checkpoint_present,
            "OutcomeUnknown",
            "network checkpoint remains active",
        )?;
        let publication = if observed.active.profile_uuid == self.snapshot.baseline.profile_uuid {
            Conclusion::RolledBack
        } else if self.confirm_issued
            && self
                .activation
                .as_ref()
                .is_some_and(|a| a.active == observed.active)
        {
            Conclusion::Committed
        } else {
            return Err(err(
                "OutcomeUnknown",
                "network trial has no uniquely known result",
            ));
        };
        Ok(Proof {
            operation_ref: self.operation_ref.clone(),
            journal_ref: self.journal_ref.clone(),
            link_ref: self.link_ref.clone(),
            observed,
            publication,
        })
    }

    pub(crate) fn artifact(&self) -> Result<Value> {
        let proof = self.read().map_err(|error| {
            if error.0.code == "OutcomeUnknown" {
                // The original effect is still unknown. This read completed
                // without sufficient evidence, so release its recovery slot.
                err("PreconditionFailed", error.0.message)
            } else {
                error
            }
        })?;
        Ok(serde_json::to_value(proof)?)
    }

    pub(crate) fn live(&self) -> Result<ReconcileSample> {
        Ok(ReconcileSample(self.read()?))
    }
}

pub(crate) fn inspect(db: &Db<'_>, op: &Doc) -> Result<Inspection> {
    let custody = Custody::load(db, op)?;
    let link = db.resolve(&custody.journal.link_ref)?;
    require(
        op.body["outcome_knowledge"] == "Unknown"
            && db.of(&link.id, "NetworkLink")?.reference() == link.reference(),
        "PreconditionFailed",
        "inspection requires the unresolved operation and its enrolled link revision",
    )?;
    let (scope, checkpoint) = custody.pair()?;
    let (activation, activation_receipt_ref) = match custody.journal.phase {
        Phase::CheckpointOwned => (None, None),
        // Only the original worker can attest that the activation returned.
        // An issuance alone can still have an orphaned client in flight.
        Phase::ActivateIssued => {
            let (activation, reference) = activation_receipt::get(db, op, &custody)?;
            (Some(activation), Some(reference))
        }
        Phase::Activated
        | Phase::AwaitingConfirmation
        | Phase::ConfirmIssued
        | Phase::RollbackIssued => (Some(custody.activation()?.clone()), None),
        _ => {
            return Err(err(
                "OutcomeUnknown",
                "network activation may still be in flight",
            ));
        }
    };
    Ok(Inspection {
        operation_ref: op.reference(),
        journal_ref: custody.document.reference(),
        link_ref: link.reference(),
        snapshot: scope.clone(),
        checkpoint: checkpoint.clone(),
        activation,
        activation_receipt_ref,
        confirm_issued: custody.journal.phase == Phase::ConfirmIssued,
    })
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    observation: &Doc,
    artifact: Value,
    live: Option<ReconcileSample>,
) -> Result<(bool, Value, Value)> {
    let current = inspect(db, op)?;
    let artifact: Proof = decode(&artifact)?;
    require(
        artifact.operation_ref == current.operation_ref
            && artifact.journal_ref == current.journal_ref
            && artifact.link_ref == current.link_ref
            && live.is_some_and(|live| live.0 == artifact),
        "OutcomeUnknown",
        "fresh network observation does not match the retained trial evidence",
    )?;
    let applied = artifact.publication == Conclusion::Committed;
    mark_settled(db, op, applied)?;
    let mut evidence = json!({"observation_ref":observation.reference(),"journal_ref":artifact.journal_ref,
        "conclusion":if applied {"NetworkTrialCommitted"} else {"NetworkTrialRolledBack"}});
    if let Some(reference) = current.activation_receipt_ref {
        evidence["activation_receipt_ref"] = reference;
    }
    Ok((
        applied,
        json!({"publication":artifact.publication, "link_ref":artifact.link_ref}),
        evidence,
    ))
}

fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "network trial record is missing"))
}

pub(crate) fn verify_active(conn: &Connection, format: u32) -> Result<()> {
    verify_active_records(conn, format)
        .and_then(|()| activation_receipt::verify(conn))
        .map_err(|error| {
            if error.0.code == "RecoveryRequired" {
                error
            } else {
                err(
                    "RecoveryRequired",
                    format!("network trial custody verification: {}", error.0.message),
                )
            }
        })
}

fn verify_active_rollback(conn: &Connection, custody: &Custody, op: &Doc) -> Result<()> {
    let issue = custody.journal.rollback_issue.as_ref().ok_or_else(|| {
        err(
            "RecoveryRequired",
            "active network rollback has no dispatch evidence",
        )
    })?;
    let previous = resolve(conn, &issue.previous_journal_ref)?;
    let armed_op = resolve(conn, &issue.operation_ref)?;
    let mut expected = previous.body.clone();
    require(
        expected.is_object(),
        "RecoveryRequired",
        "network rollback predecessor is unreadable",
    )?;
    expected["phase"] = json!(Phase::RollbackIssued);
    expected["rollback_issue"] = serde_json::to_value(issue)?;
    require(
        !issue.attempt.is_empty()
            && previous.id == custody.document.id
            && previous.owner == op.owner
            && previous.rev.checked_add(1) == Some(custody.document.rev)
            && previous.kind == JOURNAL
            && previous.spec == custody.document.spec
            && previous.body["phase"] == "AwaitingConfirmation"
            && previous.body.get("rollback_issue").is_none()
            && previous.body.get("rollback_receipt_ref").is_none()
            && expected == custody.document.body
            && armed_op.id == op.id
            && armed_op.kind == "Operation"
            && armed_op.owner == op.owner
            && armed_op.spec == op.spec
            && armed_op.body["lifecycle"] == "WaitingDependency"
            && armed_op.body["outcome_knowledge"] == "InFlight"
            && armed_op.body["effect_refs"][0]["resource_id"]
                == op.body["effect_refs"][0]["resource_id"],
        "RecoveryRequired",
        "active network rollback does not bind its parked predecessor",
    )
}

fn verify_active_records(conn: &Connection, format: u32) -> Result<()> {
    let mut selected = conn.prepare(
        "SELECT q.operation FROM queue q JOIN current t ON q.task=t.id
           WHERE q.phase IN ('Dispatched','WaitingInput','Unknown') AND t.spec->>'task_type'=?1
         UNION
         SELECT h.holder FROM holds h CROSS JOIN current link
           WHERE h.volume=link.id AND link.kind='NetworkLink'",
    )?;
    for operation in selected.query_map([TOOL], |row| row.get::<_, String>(0))? {
        let op = needed(conn, &operation?)?;
        let custody = Custody::read(conn, &op)?;
        if custody.journal.phase == Phase::RollbackIssued {
            verify_active_rollback(conn, &custody, &op)?;
        }
        let j = custody.journal;
        let task = needed(conn, j.task_ref["resource_id"].as_str().unwrap_or_default())?;
        let run = needed(conn, j.run_ref["resource_id"].as_str().unwrap_or_default())?;
        let link = needed(conn, j.link_ref["resource_id"].as_str().unwrap_or_default())?;
        let (state, operation): (String, String) = conn.query_row(
            "SELECT phase,operation FROM queue WHERE task=?1",
            [&task.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let issued = j.phase != Phase::PermitBound;
        let effect = resolve(conn, &op.body["effect_refs"][0])?;
        let shape = match j.phase {
            Phase::PermitBound => {
                j.checkpoint.is_none() && j.activation.is_none() && j.deadline.is_none()
            }
            Phase::CreateIssued => {
                j.snapshot.is_some() && j.checkpoint.is_none() && j.activation.is_none()
            }
            Phase::CheckpointOwned | Phase::ActivateIssued => {
                j.checkpoint.is_some() && j.activation.is_none()
            }
            Phase::Activated
            | Phase::AwaitingConfirmation
            | Phase::ConfirmIssued
            | Phase::RollbackIssued => j.activation.is_some(),
            _ => false,
        };
        let lease = match state.as_str() {
            "Dispatched" => {
                op.body["lifecycle"] == "Running"
                    && op.body["outcome_knowledge"] == "InFlight"
                    && !matches!(
                        j.phase,
                        Phase::AwaitingConfirmation | Phase::ConfirmIssued | Phase::RollbackIssued
                    )
            }
            WAITING => {
                op.body["lifecycle"] == "WaitingDependency"
                    && op.body["outcome_knowledge"] == "InFlight"
                    && matches!(
                        j.phase,
                        Phase::AwaitingConfirmation | Phase::ConfirmIssued | Phase::RollbackIssued
                    )
            }
            "Unknown" => op.body["outcome_knowledge"] == "Unknown" && issued,
            _ => false,
        };
        require(
            operation == op.id
                && shape
                && lease
                && task.body["operation_refs"][0] == op.reference()
                && run.body["operation_refs"][0] == op.reference()
                && link.reference() == j.link_ref
                && effect.body["phase"]
                    == if issued {
                        "EffectStarted"
                    } else {
                        "PermitBound"
                    }
                && issued == j.deadline.is_some()
                && issued == j.snapshot.is_some()
                && (j.phase == Phase::ConfirmIssued) == j.confirmation_attempt.is_some()
                && (j.phase == Phase::RollbackIssued) == j.rollback_issue.is_some()
                && j.rollback_receipt_ref.is_none()
                && (j.rollback_issue.is_none() || format >= ROLLBACK_FORMAT),
            "RecoveryRequired",
            "active network trial custody disagrees with its queue or worker",
        )?;
    }
    Ok(())
}
