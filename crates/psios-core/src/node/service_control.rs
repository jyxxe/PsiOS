//! Durable evidence of the exact authorized call into a systemd manager.
use super::*;
use crate::db::{find, resolve, row};
use rusqlite::{Connection, params};
mod settlement;
pub(crate) use settlement::{record_settlement, releases_inspection};

pub(crate) const FORMAT: u32 = agent::inference::FORMAT + 1;
const EPOCH: &str = "host-service-control-epoch";
const INTENT: &str = "HostServiceControlIntent";
const CHECKPOINT: &str = "HostServiceControlCheckpoint";
const CONFIRMATION: &str = "HostServiceControlConfirmation";

#[derive(Clone, Copy)]
pub(super) enum Control {
    Start,
    Stop,
    Restart,
}

impl Control {
    pub(super) fn from_action(action: &Action) -> Option<Self> {
        match action {
            Action::ServiceStart(_) => Some(Self::Start),
            Action::ServiceStop(_) => Some(Self::Stop),
            Action::ServiceRestart(_) => Some(Self::Restart),
            _ => None,
        }
    }

    fn from_tool(tool: &Value) -> Option<Self> {
        match tool.as_str()? {
            "service.start" => Some(Self::Start),
            "service.stop" => Some(Self::Stop),
            RESTART_TOOL => Some(Self::Restart),
            _ => None,
        }
    }

    pub(super) fn verb(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "try-restart",
        }
    }

    pub(super) fn desired(self) -> &'static str {
        match self {
            Self::Start | Self::Restart => "active",
            Self::Stop => "inactive",
        }
    }
}

pub(crate) fn is_control(action: &Action) -> bool {
    Control::from_action(action).is_some()
}

pub(crate) fn is_tool(tool: &str) -> bool {
    matches!(tool, "service.start" | "service.stop" | RESTART_TOOL)
}

pub(super) fn manager_uid(manager: &Manager) -> u32 {
    match manager {
        Manager::User => rustix::process::geteuid().as_raw(),
        Manager::System => 0,
    }
}

pub(super) fn boot_id() -> Result<String> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    let id = boot.trim();
    require(
        valid_boot_id(id),
        "DependencyUnavailable",
        "Linux boot identity unavailable",
    )?;
    Ok(id.to_owned())
}

fn valid_boot_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|uuid| !uuid.is_nil() && uuid.to_string() == id)
}

pub(crate) fn migrate(db: &Db<'_>) -> Result<()> {
    db.create(
        "HostServiceControlEpoch",
        EPOCH,
        policy::SYSTEM,
        json!({}),
        json!({"format":FORMAT}),
    )?;
    Ok(())
}

fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT seq FROM current WHERE id=?1 AND kind='HostServiceControlEpoch'
         AND rev=1 AND owner=?2 AND body->>'format'=?3",
        params![EPOCH, policy::SYSTEM, FORMAT],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| err("RecoveryRequired", "systemd control epoch is missing"))
}

fn intent_id(operation: &str) -> String {
    format!("host-service-control-intent-{operation}")
}

fn checkpoint_id(operation: &str) -> String {
    format!("host-service-control-checkpoint-{operation}")
}

fn confirmation_id(operation: &str) -> String {
    format!("host-service-control-confirmation-{operation}")
}

fn version(conn: &Connection, id: &str, revision: i64) -> Result<Doc> {
    conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![id, revision],
        row,
    )
    .optional()?
    .ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd control historical revision is missing",
        )
    })
}

fn position(conn: &Connection, subject: &Doc) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=?2",
        params![subject.id, subject.rev],
        |r| r.get(0),
    )?)
}

fn original_effect(conn: &Connection, op: &Doc) -> Result<(Doc, i64)> {
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let original = version(conn, &effect.id, 1)?;
    let seq = conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=1",
        [&effect.id],
        |r| r.get(0),
    )?;
    require(
        original.kind == "Effect"
            && original.owner == op.owner
            && original.body["operation_id"] == op.id
            && Control::from_tool(&original.body["action"]).is_some(),
        "RecoveryRequired",
        "systemd effect does not bind its original Operation",
    )?;
    Ok((original, seq))
}

fn binding(operation: &Doc, effect: &Doc, run: &Doc, task: &Doc, approval_use: &Doc) -> Value {
    json!({"operation_ref":operation.reference(),"plan_ref":operation.body["plan_ref"],
        "effect_ref":effect.reference(),"permit_ref":effect.spec["permit_ref"],"approval_use_ref":approval_use.reference(),
        "task_ref":task.reference(),
        "run_ref":run.reference(),"service_ref":effect.body["resource_ref"],"tool":effect.body["action"]})
}

struct Authority<'a> {
    task: &'a Doc,
    issued: &'a Doc,
    effect: &'a Doc,
    run: &'a Doc,
    permit: &'a Doc,
    plan: &'a Doc,
    use_: &'a Doc,
}

fn approved(conn: &Connection, authority: Authority<'_>) -> Result<()> {
    let Authority {
        task,
        issued,
        effect,
        run,
        permit,
        plan,
        use_,
    } = authority;
    let decision = resolve(conn, &permit.body["decision_ref"])?;
    let request = resolve(conn, &decision.body["request_ref"])?;
    let used = version(conn, &use_.id, 1)?;
    let approval = version(conn, &request.id, 2)?;
    let verification = resolve(conn, &decision.body["verification_evidence_ref"])?;
    let proof = resolve(conn, &permit.body["auth_proof"]["record_ref"])?;
    let indexed: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM approval_bindings WHERE request=?1 AND task=?2 AND decision=?3 AND operation=?4)",
        params![request.id,task.id,decision.id,issued.id], |r|r.get(0),
    )?;
    let audience = json!([effect.body["resource_ref"]]);
    require(
        indexed
            && task.kind == "Task"
            && task.owner == issued.owner
            && issued.spec["task_id"] == task.id
            && task.body["operation_refs"] == json!([issued.reference()])
            && task.body["current_run_id"] == run.id
            && task.body["lifecycle"] == "Running"
            && task.body["outcome_knowledge"] == "InFlight"
            && run.body["task_id"] == task.id
            && permit.body["authorization_ref"] == task.spec["delegation_ref"]
            && permit.body["audience_resource_refs"] == audience
            && permit.body["preconditions"] == plan.body["preconditions"]
            && permit.body["recovery_incarnation"] == run.body["recovery_incarnation"]
            && proof.kind == "PermitBinding"
            && proof.rev == 1
            && proof.owner == issued.owner
            && proof.body["operation_id"] == issued.id
            && proof.body["effect_id"] == effect.id
            && proof.body["permit_id"] == permit.id
            && proof.body["audience_resource_refs"] == audience
            && proof.body["claim_id"] == run.body["claim_id"]
            && proof.body["claim_epoch"] == run.body["claim_epoch"]
            && proof.body["plan_digest"] == plan.body["plan_digest"]
            && decision.kind == "ApprovalDecision"
            && decision.rev == 1
            && decision.owner == issued.owner
            && decision.body["decision"] == "Approved"
            && decision.body["use_limit"] == "SingleOperation"
            && decision.body["approver_principal_id"] == policy::OWNER
            && decision.body["plan_digest"] == plan.body["plan_digest"]
            && decision.body["preconditions"] == plan.body["preconditions"]
            && verification.kind == "ApprovalEvidence"
            && verification.owner == issued.owner
            && verification.body["authenticated_approver"] == policy::OWNER
            && verification.body["plan_digest"] == plan.body["plan_digest"]
            && request.kind == "ApprovalRequest"
            && request.rev == 1
            && request.owner == issued.owner
            && request.spec["task_id"] == task.id
            && request.body["status"] == "Pending"
            && request.body["plan_ref"] == plan.reference()
            && request.body["plan_digest"] == plan.body["plan_digest"]
            && approval.kind == request.kind
            && approval.spec == request.spec
            && approval.owner == request.owner
            && approval.body["status"] == "Approved"
            && approval.body["decision_ref"] == decision.reference()
            && use_.kind == "ApprovalUse"
            && use_.rev == 2
            && use_.owner == issued.owner
            && use_.id == format!("use-{}", decision.id)
            && use_.body["decision_ref"] == decision.reference()
            && use_.body["operation_id"] == issued.id
            && use_.body["plan_digest"] == plan.body["plan_digest"]
            && use_.body["binding_status"] == "Bound"
            && use_.body["validity_status"] == "Valid"
            && used.kind == use_.kind
            && used.owner == use_.owner
            && used.body["decision_ref"] == decision.reference()
            && used.body["plan_digest"] == plan.body["plan_digest"]
            && used.body["binding_status"] == "Unbound"
            && used.body["validity_status"] == "Valid"
            && position(conn, &decision)? < position(conn, use_)?
            && position(conn, use_)? < position(conn, permit)?
            && position(conn, permit)? < position(conn, effect)?
            && position(conn, effect)? < position(conn, issued)?
            && position(conn, issued)? < position(conn, run)?
            && position(conn, run)? < position(conn, task)?,
        "RecoveryRequired",
        "systemd dispatch disagrees with its original approval or authenticated permit",
    )
}

fn holds(conn: &Connection, service: &Doc, op: &Doc, outstanding: bool) -> Result<()> {
    let actual: (i64, i64) = conn.query_row(
        "SELECT count(*) FILTER (WHERE volume=?1),count(*) FROM holds WHERE holder=?2",
        params![service.id, op.id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let expected = if outstanding { (1, 1) } else { (0, 0) };
    require(
        actual == expected && service.kind == "HostService",
        "RecoveryRequired",
        "systemd operation does not have its expected exclusive service hold",
    )
}

fn held(db: &Db<'_>, service: &Doc, op: &Doc) -> Result<()> {
    holds(db.c, service, op, true)
}

pub(super) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.target, &work.op)?;
    let permit = db.resolve(&work.effect.spec["permit_ref"])?;
    let decision = db.resolve(&permit.body["decision_ref"])?;
    let use_ = db.of(&format!("use-{}", decision.id), "ApprovalUse")?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        binding(&work.op, &work.effect, &work.run, &work.task, &use_),
    )?;
    Ok(())
}

fn intent(conn: &Connection, op: &Doc) -> Result<Option<Doc>> {
    let (effect, seq) = original_effect(conn, op)?;
    let intent = find(conn, &intent_id(&op.id))?;
    if seq < epoch(conn)? {
        require(
            intent.is_none(),
            "RecoveryRequired",
            "legacy systemd control has a newer dispatch intent",
        )?;
        return Ok(None);
    }
    let intent = intent.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd control dispatch intent is missing",
        )
    })?;
    let issued = resolve(conn, &intent.body["operation_ref"])?;
    let run = resolve(conn, &intent.body["run_ref"])?;
    let task = resolve(conn, &intent.body["task_ref"])?;
    let use_ = resolve(conn, &intent.body["approval_use_ref"])?;
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let mut expected = binding(&issued, &effect, &run, &task, &use_);
    if let Some(reference) = intent.body.get("checkpoint_ref") {
        expected["checkpoint_ref"] = reference.clone();
    }
    require(
        intent.kind == INTENT
            && intent.owner == op.owner
            && ((intent.rev == 1 && intent.body.get("checkpoint_ref").is_none())
                || (intent.rev == 2 && intent.body["checkpoint_ref"].is_object()))
            && intent.spec["operation_id"] == op.id
            && intent.body == expected
            && issued.kind == "Operation"
            && issued.id == op.id
            && issued.owner == op.owner
            && issued.body["lifecycle"] == "Running"
            && issued.body["outcome_knowledge"] == "InFlight"
            && issued.body["effect_refs"][0] == effect.reference()
            && issued.body["plan_ref"] == plan.reference()
            && effect.body["phase"] == "PermitBound"
            && effect.body["outcome_knowledge"] == "InFlight"
            && run.kind == "Run"
            && run.owner == op.owner
            && run.body["operation_refs"][0] == issued.reference()
            && run.body["outcome_knowledge"] == "InFlight"
            && permit.kind == "ExecutionPermit"
            && permit.owner == op.owner
            && permit.body["operation_id"] == op.id
            && permit.body["effect_id"] == effect.id
            && permit.body["plan_digest"] == plan.body["plan_digest"]
            && permit.body["decision_ref"].is_object()
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && plan.body["allowed_effects"][0]["action"] == effect.body["action"]
            && plan.body["allowed_effects"][0]["resource_ref"] == effect.body["resource_ref"],
        "RecoveryRequired",
        "systemd control intent disagrees with the approved dispatch",
    )?;
    approved(
        conn,
        Authority {
            task: &task,
            issued: &issued,
            effect: &effect,
            run: &run,
            permit: &permit,
            plan: &plan,
            use_: &use_,
        },
    )?;
    Ok(Some(intent))
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = intent(db.c, op)? else {
        return Ok(false);
    };
    let target = db.resolve(&intent.body["service_ref"])?;
    holds(db.c, &target, op, true)?;
    let current = db.resolve(&op.body["effect_refs"][0])?;
    if let Some(saved) = checkpoint_link(db.c, op, &intent)? {
        require(
            current.reference() == saved.body["effect_ref"],
            "RecoveryRequired",
            "systemd started effect is not current",
        )?;
        return Ok(false);
    }
    require(
        current.reference() == intent.body["effect_ref"],
        "RecoveryRequired",
        "systemd pending effect is not current",
    )?;
    Ok(true)
}

pub(crate) fn worker_started(db: &Db<'_>, work: &Work, task: &Doc) -> Result<bool> {
    let op = db.of(&work.op.id, "Operation")?;
    let run = db.of(&work.run.id, "Run")?;
    let effect = db.resolve(&op.body["effect_refs"][0])?;
    let started = !not_dispatched(db, &op)?;
    require(
        op.spec == work.op.spec
            && op.body["plan_ref"] == work.op.body["plan_ref"]
            && op.body["lifecycle"] == "Running"
            && op.body["outcome_knowledge"] == "InFlight"
            && effect.id == work.effect.id
            && effect.spec == work.effect.spec
            && effect.rev == work.effect.rev + i64::from(started)
            && db.of(&effect.id, "Effect")?.reference() == effect.reference()
            && run.body["operation_refs"][0] == op.reference()
            && task.body["operation_refs"][0] == op.reference(),
        "Fenced",
        "systemd worker receipt differs from its current effect revision",
    )?;
    Ok(started)
}

fn matches_service(service: &Doc, observation: &Value) -> bool {
    let uid = observation["manager_uid"].as_u64();
    service.kind == "HostService"
        && service.spec["allow_control"] == true
        && observation["unit"] == service.spec["unit"]
        && observation["manager"] == service.spec["manager"]
        && (match observation["manager"].as_str() {
            Some("system") => uid == Some(0),
            Some("user") => uid.is_some_and(|uid| u32::try_from(uid).is_ok()),
            _ => false,
        })
        && observation["boot_id"].as_str().is_some_and(valid_boot_id)
        && observation["definition_digest"].is_string()
        && observation["definition_digest"] == service.spec["definition_digest"]
        && observation["load_state"] == "loaded"
        && observation["needs_reload"] == false
}

fn same_manager(before: &Value, after: &Value) -> bool {
    [
        "unit",
        "manager",
        "manager_uid",
        "boot_id",
        "definition_digest",
    ]
    .iter()
    .all(|field| before[*field] == after[*field])
        && before.get(service_manager::STARTED).is_none_or(|value| {
            service_manager::timestamp(after).is_some() && *value == after[service_manager::STARTED]
        })
}

pub(super) fn confirms(control: Control, service: &Doc, before: &Value, after: &Value) -> bool {
    matches_service(service, before)
        && matches_service(service, after)
        && same_manager(before, after)
        && after["active_state"] == control.desired()
        && (after["active_state"] != "active"
            || after["invocation_id"]
                .as_str()
                .is_some_and(valid_invocation))
        && (!matches!(control, Control::Restart)
            || after["invocation_id"] != before["invocation_id"])
}

pub(super) fn expected_invocation(action: &Action, before: &Value) -> bool {
    match action {
        Action::ServiceRestart(request) => {
            before["active_state"] == "active"
                && before["boot_id"] == request.expected_boot_id
                && before["invocation_id"] == request.expected_invocation_id
        }
        _ => true,
    }
}

pub(crate) fn record(db: &Db<'_>, work: &Work, target: &Doc, before: &Value) -> Result<()> {
    held(db, target, &work.op)?;
    let intent = intent(db.c, &work.op)?
        .ok_or_else(|| err("Fenced", "systemd dispatch requires a current intent"))?;
    let control = Control::from_action(&work.action)
        .ok_or_else(|| err("Fenced", "not a managed systemd control"))?;
    let manager: Manager = decode(&target.spec["manager"])?;
    require(
        intent.rev == 1
            && intent.body["effect_ref"] == work.effect.reference()
            && intent.body["operation_ref"] == work.op.reference()
            && intent.body["run_ref"] == work.run.reference()
            && intent.body["task_ref"] == work.task.reference()
            && db.of(&work.op.id, "Operation")?.reference() == work.op.reference()
            && db.of(&work.effect.id, "Effect")?.reference() == work.effect.reference()
            && db.of(&work.run.id, "Run")?.reference() == work.run.reference()
            && db.of(&work.task.id, "Task")?.reference() == work.task.reference()
            && target.reference() == intent.body["service_ref"]
            && matches_service(target, before)
            && service_manager::timestamp(before).is_some()
            && before["manager_uid"] == manager_uid(&manager)
            && before["boot_id"] == boot_id()?
            && expected_invocation(&work.action, before),
        "Fenced",
        "systemd invocation differs from its approved dispatch",
    )?;
    let mut effect = work.effect.body.clone();
    effect["phase"] = json!("EffectStarted");
    let effect = db.status(&work.effect, effect)?;
    let mut operation = work.op.body.clone();
    operation["effect_refs"] = json!([effect.reference()]);
    operation["updated_at"] = json!(now());
    let operation = db.status(&work.op, operation)?;
    for subject in [&work.run, &work.task] {
        let mut state = subject.body.clone();
        state["operation_refs"] = json!([operation.reference()]);
        db.status(subject, state)?;
    }
    let checkpoint = db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"intent_ref":intent.reference(),"verb":control.verb(),"before":before,
            "effect_ref":effect.reference(),"operation_ref":operation.reference()}),
    )?;
    let mut called = intent.body.clone();
    called["checkpoint_ref"] = checkpoint.reference();
    db.status(&intent, called)?;
    Ok(())
}

fn checkpoint_link(conn: &Connection, op: &Doc, intent: &Doc) -> Result<Option<Doc>> {
    let saved = find(conn, &checkpoint_id(&op.id))?;
    require(
        saved.as_ref().map(Doc::reference) == intent.body.get("checkpoint_ref").cloned(),
        "RecoveryRequired",
        "systemd dispatch has a missing or unbound checkpoint",
    )?;
    let Some(checkpoint) = saved else {
        return Ok(None);
    };
    let original = resolve(conn, &checkpoint.body["intent_ref"])?;
    let mut pending = intent.body.clone();
    pending.as_object_mut().unwrap().remove("checkpoint_ref");
    let initial = resolve(conn, &pending["effect_ref"])?;
    let effect = resolve(conn, &checkpoint.body["effect_ref"])?;
    let dispatch = resolve(conn, &checkpoint.body["operation_ref"])?;
    let prepared = resolve(conn, &original.body["operation_ref"])?;
    let mut started = initial.body.clone();
    started["phase"] = json!("EffectStarted");
    let before = &checkpoint.body["before"];
    let startup_bound = match service_manager::cutover(conn)? {
        Some(cutover) if position(conn, &initial)? > cutover => {
            service_manager::timestamp(before).is_some()
        }
        _ => before.get(service_manager::STARTED).is_none(),
    };
    require(
        startup_bound
            && checkpoint.kind == CHECKPOINT
            && checkpoint.rev == 1
            && checkpoint.owner == op.owner
            && checkpoint.spec["operation_id"] == op.id
            && original.kind == INTENT
            && original.id == intent.id
            && original.rev == 1
            && original.owner == intent.owner
            && original.spec == intent.spec
            && original.body == pending
            && effect.kind == "Effect"
            && effect.id == initial.id
            && effect.rev == initial.rev + 1
            && effect.spec == initial.spec
            && effect.owner == initial.owner
            && effect.body == started
            && dispatch.kind == "Operation"
            && dispatch.id == op.id
            && dispatch.rev == prepared.rev + 1
            && dispatch.spec == op.spec
            && dispatch.owner == op.owner
            && dispatch.body["plan_ref"] == prepared.body["plan_ref"]
            && dispatch.body["lifecycle"] == "Running"
            && dispatch.body["outcome_knowledge"] == "InFlight"
            && dispatch.body["effect_refs"] == json!([effect.reference()]),
        "RecoveryRequired",
        "systemd checkpoint disagrees with its dispatch intent and started effect",
    )?;
    Ok(Some(checkpoint))
}

fn checkpoint(
    conn: &Connection,
    op: &Doc,
    intent: &Doc,
    service: &Doc,
    control: Control,
) -> Result<Option<Doc>> {
    let Some(checkpoint) = checkpoint_link(conn, op, intent)? else {
        return Ok(None);
    };
    let tool = intent.body["tool"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "systemd tool is missing"))?;
    let plan = resolve(conn, &intent.body["plan_ref"])?;
    let input = resolve(conn, &plan.body["purpose_ref"])?;
    let action = Action::parse(tool, crate::broker::dependencies::tool_input(&input)?)?;
    require(
        checkpoint.body["verb"] == control.verb()
            && service.reference() == intent.body["service_ref"]
            && matches_service(service, &checkpoint.body["before"])
            && expected_invocation(&action, &checkpoint.body["before"]),
        "RecoveryRequired",
        "systemd checkpoint disagrees with its dispatch intent",
    )?;
    Ok(Some(checkpoint))
}

pub(crate) fn confirmed(
    db: &Db<'_>,
    op: &Doc,
    target: &Doc,
    action: &Action,
    observation: &Value,
) -> Result<Option<Doc>> {
    let Some(intent) = intent(db.c, op)? else {
        return Ok(None);
    };
    held(db, target, op)?;
    let control = Control::from_action(action)
        .ok_or_else(|| err("RecoveryRequired", "not a managed systemd control"))?;
    let checkpoint = checkpoint(db.c, op, &intent, target, control)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "systemd operation has no committed checkpoint",
        )
    })?;
    let manager: Manager = decode(&target.spec["manager"])?;
    require(
        confirms(control, target, &checkpoint.body["before"], observation)
            && observation["manager_uid"] == manager_uid(&manager)
            && observation["boot_id"] == boot_id()?,
        "OutcomeUnknown",
        "observation does not confirm the checkpointed systemd manager, boot and postcondition",
    )?;
    Ok(Some(checkpoint))
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    service_manager::cutover(conn)?;
    let mut statement = conn.prepare(
        "SELECT body->>'operation_id' FROM versions WHERE kind='Effect' AND rev=1 AND seq>?1
          AND body->>'action' IN ('service.start','service.stop','service.restart')",
    )?;
    let mut intents = 0i64;
    let mut checkpoints = 0i64;
    let mut confirmations = 0i64;
    for id in statement.query_map([epoch(conn)?], |r| r.get::<_, String>(0))? {
        let op = find(conn, &id?)?
            .ok_or_else(|| err("RecoveryRequired", "systemd operation is missing"))?;
        let intent = intent(conn, &op)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "systemd control dispatch intent is missing",
            )
        })?;
        intents += 1;
        let target = resolve(conn, &intent.body["service_ref"])?;
        let control = Control::from_tool(&intent.body["tool"])
            .ok_or_else(|| err("RecoveryRequired", "systemd control verb is invalid"))?;
        let saved = checkpoint(conn, &op, &intent, &target, control)?;
        if saved.is_some() {
            checkpoints += 1;
        }
        let context = settlement::verify_state(conn, &op, &intent, &target, saved.as_ref())?;
        confirmations += i64::from(settlement::verify_confirmation(conn, &context)?);
    }
    let counts: (i64,i64,i64) = conn.query_row(
        "SELECT count(*) FILTER (WHERE kind=?1),count(*) FILTER (WHERE kind=?2),count(*) FILTER (WHERE kind=?3)
         FROM current WHERE kind IN (?1,?2,?3)",
        [INTENT,CHECKPOINT,CONFIRMATION], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    )?;
    require(
        counts == (intents, checkpoints, confirmations),
        "RecoveryRequired",
        "systemd dispatch evidence contains unbound records",
    )
}

pub(crate) fn verify_active_holds(db: &Db<'_>) -> Result<()> {
    let manager_cutover = service_manager::cutover(db.c)?;
    let mut active = db.c.prepare(
        "SELECT t.id,t.owner,q.operation,q.phase FROM queue q JOIN current t ON t.id=q.task
         WHERE q.phase IN ('Dispatched','Unknown') AND t.kind='Task'
           AND t.spec->>'task_type' IN ('service.start','service.stop','service.restart')",
    )?;
    let mut rows = active.query([])?;
    while let Some(record) = rows.next()? {
        let task: String = record.get(0)?;
        let owner: String = record.get(1)?;
        let id: String = record.get(2)?;
        let phase: String = record.get(3)?;
        let op = db.of(&id, "Operation")?;
        require(
            op.owner == owner
                && op.spec["task_id"] == task
                && op.body["outcome_knowledge"]
                    == if phase == "Dispatched" {
                        "InFlight"
                    } else {
                        "Unknown"
                    },
            "RecoveryRequired",
            "systemd operation disagrees with its active queue entry",
        )?;
        let (effect, seq) = original_effect(db.c, &op)?;
        let service = db.resolve(&effect.body["resource_ref"])?;
        held(db, &service, &op)?;
        if manager_cutover.is_some_and(|cutover| seq > cutover) {
            let dispatch = intent(db.c, &op)?
                .ok_or_else(|| err("RecoveryRequired", "systemd control intent is missing"))?;
            let control = Control::from_tool(&effect.body["action"])
                .ok_or_else(|| err("RecoveryRequired", "systemd control verb is invalid"))?;
            let saved = checkpoint(db.c, &op, &dispatch, &service, control)?;
            require(
                phase != "Unknown" || saved.is_some(),
                "RecoveryRequired",
                "unknown systemd control has no invocation checkpoint",
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_requires_the_exact_manager_boot_and_invocation_transition() {
        let service = Doc {
            id: "service-test.service".into(),
            rev: 1,
            generation: 1,
            kind: "HostService".into(),
            owner: policy::SYSTEM.into(),
            spec: json!({"unit":"test.service","manager":"user","allow_control":true,"definition_digest":"pinned"}),
            body: json!({}),
            created: String::new(),
            stamp: String::new(),
        };
        let initial = json!({"unit":"test.service","manager":"user","manager_uid":1000,
            "load_state":"loaded","active_state":"active","needs_reload":false,
            "definition_digest":"pinned","boot_id":"ff930492-7d24-4217-a1e5-6568b358864f",
            "invocation_id":"ffdd1daeafec4def96db80f6e77b7482"});
        assert!(!confirms(Control::Restart, &service, &initial, &initial));
        let mut restarted = initial.clone();
        restarted["invocation_id"] = json!("7f48a7bc2b21471bbbf77b7812a845bd");
        assert!(confirms(Control::Restart, &service, &initial, &restarted));
        for (field, different) in [
            ("manager_uid", json!(1001)),
            ("manager", json!("system")),
            ("boot_id", json!("8fb4fa96-7fc2-4d25-a1af-28d153411256")),
            ("definition_digest", json!("different")),
            ("active_state", json!("activating")),
            ("invocation_id", json!("00000000000000000000000000000000")),
        ] {
            let mut observation = restarted.clone();
            observation[field] = different;
            assert!(
                !confirms(Control::Restart, &service, &initial, &observation),
                "{field}"
            );
        }
        let mut versioned = initial.clone();
        versioned[service_manager::STARTED] = json!("123456");
        assert!(!confirms(
            Control::Restart,
            &service,
            &versioned,
            &restarted
        ));
        let mut new_manager = restarted.clone();
        new_manager[service_manager::STARTED] = json!("124456");
        assert!(confirms(Control::Restart, &service, &initial, &new_manager));
        assert!(!confirms(
            Control::Restart,
            &service,
            &versioned,
            &new_manager
        ));
        new_manager[service_manager::STARTED] = versioned[service_manager::STARTED].clone();
        assert!(confirms(
            Control::Restart,
            &service,
            &versioned,
            &new_manager
        ));
        assert!(confirms(Control::Start, &service, &initial, &initial));
        restarted["active_state"] = json!("inactive");
        assert!(confirms(Control::Stop, &service, &initial, &restarted));
    }
}
