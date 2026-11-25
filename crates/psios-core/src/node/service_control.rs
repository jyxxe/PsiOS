//! Durable evidence of the exact authorized call into a systemd manager.
use super::*;
use crate::db::{find, resolve, row};
use rusqlite::{Connection, params};

pub(crate) const FORMAT: u32 = agent::inference::FORMAT + 1;
const EPOCH: &str = "host-service-control-epoch";
const INTENT: &str = "HostServiceControlIntent";
const CHECKPOINT: &str = "HostServiceControlCheckpoint";

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

fn original_effect(conn: &Connection, op: &Doc) -> Result<(Doc, i64)> {
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let original = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&effect.id],
        row,
    )?;
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

fn binding(operation: &Doc, effect: &Doc, run: &Doc) -> Value {
    json!({"operation_ref":operation.reference(),"plan_ref":operation.body["plan_ref"],
        "effect_ref":effect.reference(),"permit_ref":effect.spec["permit_ref"],
        "run_ref":run.reference(),"service_ref":effect.body["resource_ref"],"tool":effect.body["action"]})
}

fn held(db: &Db<'_>, service: &Doc, op: &Doc) -> Result<()> {
    let present: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![service.id, op.id],
        |r| r.get(0),
    )?;
    require(
        present && service.kind == "HostService",
        "RecoveryRequired",
        "systemd control no longer owns its service hold",
    )
}

pub(super) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.target, &work.op)?;
    db.create(
        INTENT,
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        binding(&work.op, &work.effect, &work.run),
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
    let permit = resolve(conn, &effect.spec["permit_ref"])?;
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let mut expected = binding(&issued, &effect, &run);
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
            && plan.body["allowed_effects"][0]["resource_ref"] == effect.body["resource_ref"],
        "RecoveryRequired",
        "systemd control intent disagrees with the approved dispatch",
    )?;
    Ok(Some(intent))
}

pub(crate) fn not_dispatched(db: &Db<'_>, op: &Doc) -> Result<bool> {
    let Some(intent) = intent(db.c, op)? else {
        return Ok(false);
    };
    let present = db.find(&checkpoint_id(&op.id))?;
    require(
        present.as_ref().map(Doc::reference) == intent.body.get("checkpoint_ref").cloned(),
        "RecoveryRequired",
        "systemd dispatch has a missing or unbound checkpoint",
    )?;
    Ok(present.is_none())
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
            && intent.body == binding(&work.op, &work.effect, &work.run)
            && target.reference() == intent.body["service_ref"]
            && matches_service(target, before)
            && before["manager_uid"] == manager_uid(&manager)
            && before["boot_id"] == boot_id()?
            && expected_invocation(&work.action, before),
        "Fenced",
        "systemd invocation differs from its approved dispatch",
    )?;
    let checkpoint = db.create(
        CHECKPOINT,
        &checkpoint_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"intent_ref":intent.reference(),"verb":control.verb(),"before":before}),
    )?;
    let mut called = intent.body.clone();
    called["checkpoint_ref"] = checkpoint.reference();
    db.status(&intent, called)?;
    Ok(())
}

fn checkpoint(
    conn: &Connection,
    op: &Doc,
    intent: &Doc,
    service: &Doc,
    control: Control,
) -> Result<Option<Doc>> {
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
    let tool = intent.body["tool"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "systemd tool is missing"))?;
    let plan = resolve(conn, &intent.body["plan_ref"])?;
    let input = resolve(conn, &plan.body["purpose_ref"])?;
    let action = Action::parse(tool, crate::broker::dependencies::tool_input(&input)?)?;
    require(
        checkpoint.kind == CHECKPOINT
            && checkpoint.rev == 1
            && checkpoint.owner == op.owner
            && checkpoint.spec["operation_id"] == op.id
            && original.kind == INTENT
            && original.id == intent.id
            && original.rev == 1
            && original.owner == intent.owner
            && original.spec == intent.spec
            && original.body == pending
            && checkpoint.body["verb"] == control.verb()
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
    let mut statement = conn.prepare(
        "SELECT body->>'operation_id' FROM versions WHERE kind='Effect' AND rev=1 AND seq>?1
          AND body->>'action' IN ('service.start','service.stop','service.restart')",
    )?;
    let mut intents = 0u64;
    let mut checkpoints = 0u64;
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
        require(
            (op.body["outcome_knowledge"] != "Unknown" || saved.is_some())
                && (op.body["lifecycle"] != "Succeeded" || saved.is_some())
                && (op.body["outcome_knowledge"] != "NotStarted" || saved.is_none()),
            "RecoveryRequired",
            "systemd checkpoint disagrees with the recorded outcome",
        )?;
    }
    let counts: (u64,u64) = conn.query_row(
        "SELECT count(*) FILTER (WHERE kind=?1),count(*) FILTER (WHERE kind=?2) FROM current WHERE kind IN (?1,?2)",
        [INTENT,CHECKPOINT], |r| Ok((r.get(0)?,r.get(1)?)),
    )?;
    require(
        counts == (intents, checkpoints),
        "RecoveryRequired",
        "systemd dispatch evidence contains unbound records",
    )
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
        assert!(confirms(Control::Start, &service, &initial, &initial));
        restarted["active_state"] = json!("inactive");
        assert!(confirms(Control::Stop, &service, &initial, &restarted));
    }
}
