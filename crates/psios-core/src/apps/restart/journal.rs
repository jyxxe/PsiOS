//! Validate both the original restart journal and the versioned invocation fence.
use super::*;
use crate::db::{find, resolve, row};
use rusqlite::{Connection, OptionalExtension};

const EPOCH: &str = "oci-restart-custody-epoch";

pub(super) fn migrate(db: &Db<'_>) -> Result<()> {
    db.create(
        "OCIRestartCustodyEpoch",
        EPOCH,
        crate::policy::SYSTEM,
        json!({}),
        json!({"format":CUSTODY_FORMAT}),
    )?;
    Ok(())
}

pub(super) fn epoch(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT seq FROM current WHERE id=?1 AND kind='OCIRestartCustodyEpoch' AND rev=1 AND owner=?2 AND body->>'format'=?3",
        params![EPOCH,crate::policy::SYSTEM,CUSTODY_FORMAT], |r|r.get(0),
    ).optional()?.ok_or_else(|| err("RecoveryRequired", "OCI restart custody epoch is missing"))
}

pub(super) fn needed(conn: &Connection, id: &Value) -> Result<Doc> {
    let id = id
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI restart resource identity missing"))?;
    find(conn, id)?.ok_or_else(|| err("RecoveryRequired", "OCI restart resource is missing"))
}

fn historical(conn: &Connection, id: &str, rev: i64) -> Result<Doc> {
    conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![id, rev],
        row,
    )
    .optional()?
    .ok_or_else(|| {
        err(
            "RecoveryRequired",
            "OCI restart historical revision is missing",
        )
    })
}

pub(super) struct Custody {
    pub(super) app: Doc,
    pub(super) instance: Doc,
    pub(super) volume: Option<Doc>,
    attachment: Option<Doc>,
    request: Request,
}

fn valid_container(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl Custody {
    fn load(conn: &Connection, plan: &Doc) -> Result<Self> {
        let app = resolve(conn, &plan.body["allowed_effects"][0]["resource_ref"])?;
        let instance = resolve(conn, &app.body["instance_ref"])?;
        let input = resolve(conn, &plan.body["purpose_ref"])?;
        let request: Request = crate::decode(crate::broker::dependencies::tool_input(&input)?)?;
        request.validate()?;
        require(
            app.kind == "Application"
                && app.body["lifecycle"] == "Running"
                && request.id == app.id
                && request.expected_resource_version == app.version()
                && instance.kind == "RuntimeInstance"
                && instance.owner == app.owner
                && instance.spec["application_id"] == app.id
                && instance.spec["deployment_ref"] == app.spec["deployment_ref"]
                && instance.spec["container_id"] == app.body["container_id"]
                && instance.spec["engine_id"] == app.body["engine_id"]
                && instance.spec["start_operation_id"] == app.body["start_operation_id"]
                && app.body["container_id"]
                    .as_str()
                    .is_some_and(valid_container)
                && app.body["engine_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
                && app.body["start_operation_id"].is_string(),
            "RecoveryRequired",
            "OCI restart plan has no matching installed invocation",
        )?;
        let subjects: Vec<_> = plan.body["preconditions"]
            .as_array()
            .ok_or_else(|| err("RecoveryRequired", "OCI restart plan has no audience"))?
            .iter()
            .map(|condition| condition["resource_ref"].clone())
            .collect();
        let (volume, attachment) = if let Some(id) = app.spec["volume_id"].as_str() {
            require(
                subjects.len() == 2 && subjects[0] == app.reference(),
                "RecoveryRequired",
                "OCI restart plan lacks its bound volume",
            )?;
            let volume = resolve(conn, &subjects[1])?;
            let attachment = resolve(conn, &volume.body["attachment_refs"][0])?;
            let access = json!(super::super::selected_volume_access(conn, &app)?);
            require(
                volume.id == id
                    && volume.kind == "Volume"
                    && volume.body["data_incarnation"] == app.spec["data_incarnation"]
                    && volume.body["attachment_refs"] == json!([attachment.reference()])
                    && attachment.kind == "VolumeAttachment"
                    && attachment.owner == app.owner
                    && attachment.body["volume_id"] == volume.id
                    && attachment.body["workload_id"] == app.id
                    && attachment.body["data_incarnation"] == volume.body["data_incarnation"]
                    && attachment.body["lifecycle"] == "Attached"
                    && attachment.body["requested_access"] == access
                    && attachment.body["effective_access"] == access,
                "RecoveryRequired",
                "OCI restart plan has no matching volume attachment",
            )?;
            (Some(volume), Some(attachment))
        } else {
            require(
                app.spec["volume_id"].is_null() && subjects == [app.reference()],
                "RecoveryRequired",
                "OCI restart plan has unexpected targets",
            )?;
            (None, None)
        };
        Ok(Self {
            app,
            instance,
            volume,
            attachment,
            request,
        })
    }

    pub(super) fn binding(&self) -> Value {
        json!({"application_ref":self.app.reference(),"instance_ref":self.instance.reference(),
            "volume_ref":self.volume.as_ref().map(Doc::reference),"attachment_ref":self.attachment.as_ref().map(Doc::reference)})
    }

    pub(super) fn holds(&self, conn: &Connection, op: &Doc, outstanding: bool) -> Result<()> {
        let holds: (i64, i64) = conn.query_row(
            "SELECT count(*) FILTER (WHERE volume=?1),count(*) FROM holds WHERE holder=?2",
            params![self.app.id, op.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        require(
            holds == if outstanding { (1, 1) } else { (0, 0) },
            "RecoveryRequired",
            "OCI restart exclusive application hold is inconsistent",
        )?;
        if outstanding {
            require(
                needed(conn, &json!(self.app.id))?.reference() == self.app.reference()
                    && needed(conn, &json!(self.instance.id))?.reference()
                        == self.instance.reference(),
                "RecoveryRequired",
                "OCI restart holds an obsolete application or instance revision",
            )?;
            if let Some(volume) = &self.volume {
                let holder: Option<String> = conn
                    .query_row(
                        "SELECT holder FROM holds WHERE volume=?1",
                        [&volume.id],
                        |r| r.get(0),
                    )
                    .optional()?;
                require(
                    holder.as_deref() == Some(self.app.id.as_str())
                        && needed(conn, &json!(volume.id))?.reference() == volume.reference(),
                    "RecoveryRequired",
                    "OCI restart attached volume hold is inconsistent",
                )?;
            }
            if let Some(attachment) = &self.attachment {
                require(
                    needed(conn, &json!(attachment.id))?.reference() == attachment.reference(),
                    "RecoveryRequired",
                    "OCI restart attachment changed while its operation was unresolved",
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn sample(&self, before: &Value) -> bool {
        self.request.matches(before) && self.running(before)
    }

    pub(super) fn running(&self, sample: &Value) -> bool {
        sample["application_ref"] == self.app.reference()
            && ["container_id", "engine_id", "start_operation_id"]
                .iter()
                .all(|field| sample[*field] == self.app.body[*field])
            && sample["process_running"] == true
            && sample["runtime_state"] == "Running"
            && sample["process_id"]
                .as_u64()
                .is_some_and(|pid| (1..=i32::MAX as u64).contains(&pid))
            && sample["started_at"].as_str().is_some_and(|value| {
                value.len() <= 40 && DateTime::parse_from_rfc3339(value).is_ok()
            })
            && matches!(
                sample["effective_limits"]["source"].as_str(),
                Some("LinuxCgroupV2" | "DockerCgroupV1")
            )
    }
}

pub(super) struct Journal {
    pub(super) modern: bool,
    pub(super) intent: Doc,
    pub(super) checkpoint: Option<Doc>,
    pub(super) task: Doc,
    pub(super) run: Doc,
    pub(super) effect: Doc,
    pub(super) custody: Custody,
}

pub(super) fn load(conn: &Connection, op: &Doc, epoch: i64) -> Result<Journal> {
    let task = needed(conn, &op.spec["task_id"])?;
    let run = needed(conn, &task.body["current_run_id"])?;
    let issued_run = historical(conn, &run.id, 2)?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let (initial, seq): (Doc, i64) = conn.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=1",
        [&effect.id],
        |r| Ok((row(r)?, r.get("seq")?)),
    )?;
    let modern = seq > epoch;
    let issued = resolve(conn, &issued_run.body["operation_refs"][0])?;
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let approval =
        crate::broker::verify_bound_approval(conn, op, &task, &issued_run, &plan, &initial, TOOL)?;
    let custody = Custody::load(conn, &plan)?;
    let intent = needed(conn, &json!(intent_id(&op.id)))?;
    let checkpoint = find(conn, &checkpoint_id(&op.id))?;
    require(
        op.kind == "Operation"
            && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == TOOL
            && task.body["operation_refs"] == json!([op.reference()])
            && task.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && run.kind == "Run"
            && run.owner == op.owner
            && run.body["task_id"] == task.id
            && run.body["operation_refs"] == task.body["operation_refs"]
            && run.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && issued_run.kind == "Run"
            && issued_run.owner == op.owner
            && issued_run.spec == run.spec
            && issued_run.body["task_id"] == task.id
            && issued_run.body["outcome_knowledge"] == "InFlight"
            && issued.kind == "Operation"
            && issued.id == op.id
            && issued.spec == op.spec
            && issued.owner == op.owner
            && issued.body["plan_ref"] == plan.reference()
            && issued.body["effect_refs"] == json!([initial.reference()])
            && issued.body["outcome_knowledge"] == "InFlight"
            && initial.kind == "Effect"
            && initial.owner == op.owner
            && initial.body["action"] == TOOL
            && initial.body["operation_id"] == op.id
            && initial.body["outcome_knowledge"] == "InFlight"
            && initial.body["phase"]
                == if modern {
                    "PermitBound"
                } else {
                    "EffectStarted"
                }
            && initial.body["resource_ref"] == custody.app.reference()
            && effect.kind == initial.kind
            && effect.owner == initial.owner
            && effect.spec == initial.spec
            && effect.body["action"] == TOOL
            && effect.body["operation_id"] == op.id
            && effect.body["resource_ref"] == initial.body["resource_ref"]
            && effect.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && op.body["effect_refs"] == json!([effect.reference()])
            && bound(&intent, INTENT, op)
            && intent.body["application_ref"] == custody.app.reference(),
        "RecoveryRequired",
        "OCI restart journal disagrees with the original effect or application",
    )?;
    if let Some(checkpoint) = &checkpoint {
        require(
            bound(checkpoint, CHECKPOINT, op)
                && checkpoint.rev == 1
                && custody.sample(&checkpoint.body["before"])
                && checkpoint.body["volume_ref"]
                    == json!(custody.volume.as_ref().map(Doc::reference)),
            "RecoveryRequired",
            "OCI restart checkpoint disagrees with approved invocation or volume",
        )?;
    }
    if modern {
        require(
            intent.rev == 1 + i64::from(checkpoint.is_some())
                && intent.body["operation_ref"] == issued.reference()
                && intent.body["run_ref"] == issued_run.reference()
                && intent.body["effect_ref"] == initial.reference()
                && intent.body["approval_use_ref"] == approval.reference()
                && intent.body["binding"] == custody.binding()
                && intent.body.get("checkpoint_ref").cloned()
                    == checkpoint.as_ref().map(Doc::reference),
            "RecoveryRequired",
            "OCI restart intent has a missing or unbound invocation checkpoint",
        )?;
        let started_effect = if let Some(checkpoint) = &checkpoint {
            let pending = resolve(conn, &checkpoint.body["intent_ref"])?;
            let started = resolve(conn, &checkpoint.body["started_effect_ref"])?;
            let started_op = resolve(conn, &checkpoint.body["started_operation_ref"])?;
            let mut pending_body = intent.body.clone();
            pending_body
                .as_object_mut()
                .unwrap()
                .remove("checkpoint_ref");
            let mut expected = initial.body.clone();
            expected["phase"] = json!("EffectStarted");
            require(
                pending.kind == INTENT
                    && pending.id == intent.id
                    && pending.rev == 1
                    && pending.owner == intent.owner
                    && pending.spec == intent.spec
                    && pending.body == pending_body
                    && started.kind == initial.kind
                    && started.id == initial.id
                    && started.rev == initial.rev + 1
                    && started.owner == initial.owner
                    && started.spec == initial.spec
                    && started.body == expected
                    && started_op.kind == issued.kind
                    && started_op.id == issued.id
                    && started_op.rev == issued.rev + 1
                    && started_op.owner == issued.owner
                    && started_op.spec == issued.spec
                    && started_op.body["plan_ref"] == issued.body["plan_ref"]
                    && started_op.body["outcome_knowledge"] == "InFlight"
                    && started_op.body["effect_refs"] == json!([started.reference()]),
                "RecoveryRequired",
                "OCI restart checkpoint has no exact started effect",
            )?;
            Some(started)
        } else {
            None
        };
        let phase = match op.body["outcome_knowledge"].as_str() {
            Some("InFlight") => {
                effect.reference() == started_effect.as_ref().unwrap_or(&initial).reference()
            }
            Some("NotStarted") => {
                started_effect.is_none() && effect.rev == 2 && effect.body["phase"] == "PermitBound"
            }
            Some("Unknown") => {
                started_effect.is_some()
                    && effect.rev == 3
                    && effect.body["phase"] == "EffectStarted"
            }
            Some("Known") => {
                started_effect.is_some()
                    && matches!(
                        (effect.rev, effect.body["phase"].as_str()),
                        (3, Some("EffectConfirmed")) | (4, Some("EffectReconciled"))
                    )
            }
            _ => false,
        };
        require(
            phase,
            "RecoveryRequired",
            "OCI restart effect phase disagrees with the witnessed invocation",
        )?;
    } else {
        require(
            intent.rev == 1 && intent.body.get("checkpoint_ref").is_none(),
            "RecoveryRequired",
            "legacy OCI restart has a newer checkpoint profile",
        )?;
    }
    Ok(Journal {
        modern,
        intent,
        checkpoint,
        task,
        run,
        effect,
        custody,
    })
}

pub(super) fn binding(db: &Db<'_>, work: &Work) -> Result<(Value, Value)> {
    let plan = db.resolve(&work.op.body["plan_ref"])?;
    let custody = Custody::load(db.c, &plan)?;
    custody.holds(db.c, &work.op, true)?;
    let approved = crate::broker::verify_bound_approval(
        db.c,
        &work.op,
        &work.task,
        &work.run,
        &plan,
        &work.effect,
        TOOL,
    )?;
    Ok((custody.binding(), approved.reference()))
}

pub(crate) fn verify_active(conn: &Connection) -> Result<()> {
    let epoch = epoch(conn)?;
    let mut statement = conn.prepare(
        "SELECT q.task,q.operation,q.phase FROM queue q JOIN current t ON t.id=q.task
        WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type'=?1",
    )?;
    for record in statement.query_map([TOOL], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (task, op, phase) = record?;
        let op = needed(conn, &json!(op))?;
        let journal = load(conn, &op, epoch)?;
        require(
            journal.task.id == task
                && op.body["outcome_knowledge"]
                    == if phase == "Dispatched" {
                        "InFlight"
                    } else {
                        "Unknown"
                    },
            "RecoveryRequired",
            "active OCI restart queue disagrees with its operation",
        )?;
        journal.custody.holds(conn, &op, true)?;
    }
    Ok(())
}
