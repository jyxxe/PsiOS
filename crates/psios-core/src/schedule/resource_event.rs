use super::*;
use crate::db::row;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Rule {
    source_ref: Value,
    condition: Condition,
    expires_at: String,
    max_triggers: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Condition {
    SpecChanged,
    LifecycleEntered { lifecycle: String },
}

impl Condition {
    fn matches(&self, previous: &Doc, next: &Doc) -> bool {
        match self {
            Self::SpecChanged => next.generation > previous.generation,
            Self::LifecycleEntered { lifecycle } => {
                previous.body["lifecycle"] != *lifecycle && next.body["lifecycle"] == *lifecycle
            }
        }
    }
}

fn event_permissions(db: &Db<'_>, actor: &str, delegation: &str) -> Result<()> {
    for action in ["resource.read", "event.read", "task.read"] {
        policy::delegation(db, actor, delegation, action)?;
    }
    Ok(())
}

pub(super) fn create(db: &Db<'_>, actor: &str, p: &Create, supplied: &Rule) -> Result<Value> {
    require(
        (1..=32).contains(&supplied.max_triggers),
        "InvalidArgument",
        "event schedule trigger limit must be 1 to 32",
    )?;
    if let Condition::LifecycleEntered { lifecycle } = &supplied.condition {
        label(lifecycle)?;
    }
    let expires = instant(&supplied.expires_at)?;
    let rule = Rule {
        expires_at: timestamp(expires),
        ..supplied.clone()
    };
    let id = schedule_id(db, actor, &p.key)?;
    let creation = sha256(&canonical(
        &json!([p.tool,p.input,p.delegation_id,{"event":rule}]),
    ));
    if let Some(schedule) = existing(db, actor, &id, &creation)? {
        return Ok(schedule);
    }

    let delegation = policy::delegation(db, actor, &p.delegation_id, ACTION)?;
    event_permissions(db, actor, &p.delegation_id)?;
    require(
        Utc::now() < expires && expires <= scope_end(db, &delegation)?,
        "InvalidArgument",
        "event schedule requires expiration within its authorization lifetime",
    )?;
    let source = db.resolve(&rule.source_ref)?;
    require(
        policy::visible(actor, &source),
        "PermissionDenied",
        "event source is outside the actor's authorized scope",
    )?;
    require(
        db.get(&source.id)?.reference() == rule.source_ref,
        "PreconditionFailed",
        "event source revision changed",
    )?;
    let template = prepare_template(
        db,
        actor,
        p,
        &delegation,
        &id,
        &format!("event-template-{id}"),
        &rule.expires_at,
    )?;
    let tool = db.of(&format!("tool-{}", p.tool), "Capability")?;
    let schedule = db.create(
        "Schedule",
        &id,
        actor,
        json!({"creation_digest":creation,"event":rule,"request":template,"tool_ref":tool.reference()}),
        json!({
            "lifecycle":"Pending", "source_ref":source.reference(), "expires_at":rule.expires_at,
            "trigger_refs":[], "pending_trigger_ref":null, "max_triggers":rule.max_triggers,
            "last_task_id":null,"reason":null
        }),
    )?;
    policy::deadline::Clock::sample()?.register_until(db, &id, &rule.expires_at, None)?;
    db.c.execute(
        "INSERT INTO pending_event_schedules VALUES(?1,?2,?3,?4)",
        params![id, source.id, source.rev, due_ms(expires)],
    )?;
    Ok(reference(&schedule))
}

struct Work {
    schedule: String,
    source: String,
    source_rev: i64,
    expired: bool,
    next_seq: Option<i64>,
}

fn next_work(db: &Db<'_>) -> Result<Option<Work>> {
    let pending: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM pending_event_schedules)",
        [],
        |r| r.get(0),
    )?;
    if !pending {
        return Ok(None);
    }
    let clock = policy::deadline::Clock::sample()?;
    Ok(db.c.query_row(
        "WITH candidates AS (
           SELECT p.*,
                  (p.deadline_ms<=?1 OR coalesce(elapsed.time_ns!=?3 OR elapsed.deadline_boot_ms<=?4,0)) AS expired,
                  (SELECT v.seq FROM versions v WHERE v.id=p.source AND v.rev>p.source_rev ORDER BY v.rev LIMIT 1) AS next_seq
           FROM pending_event_schedules p LEFT JOIN authorization_deadlines elapsed
                ON elapsed.resource=p.schedule AND elapsed.boot_id=?2
         )
         SELECT schedule,source,source_rev,expired,next_seq FROM candidates
         WHERE expired OR next_seq IS NOT NULL ORDER BY expired DESC,next_seq,schedule LIMIT 1",
        params![Utc::now().timestamp_millis(), clock.boot.boot_id, clock.time_ns, clock.boot.millis],
        |r| Ok(Work {
            schedule: r.get(0)?, source: r.get(1)?, source_rev: r.get(2)?,
            expired: r.get(3)?, next_seq: r.get(4)?,
        }),
    ).optional()?)
}

struct Trigger {
    id: String,
    previous_rev: i64,
    source_rev: i64,
    source_ref: Value,
    event: Value,
}

struct Attempt {
    schedule: String,
    version: String,
    trigger: Trigger,
}

pub(super) fn tick(store: &mut Store) -> Result<()> {
    let correlation = id("resource-event-tick");
    let mut rejected = None;
    match store.transaction(&correlation, |db| fire(db, &mut rejected)) {
        Ok(()) => Ok(()),
        Err(error) if deferred(&error) && error.0.code != "QueueFull" => Ok(()),
        Err(error) => {
            let Some(attempt) = rejected else {
                return Err(error);
            };
            store.transaction(&correlation, |db| {
                let schedule = db.of(&attempt.schedule, "Schedule")?;
                schedule.cas(&attempt.version)?;
                record_attempt_failure(db, &schedule, &attempt.trigger, &error)
            })
        }
    }
}

fn record_attempt_failure(
    db: &Db<'_>,
    schedule: &Doc,
    trigger: &Trigger,
    error: &crate::Error,
) -> Result<()> {
    match error.0.code.as_str() {
        "QueueFull" => remember_pending(db, schedule, trigger).map(|_| ()),
        "DeadlineExceeded" => {
            let schedule = remember_pending(db, schedule, trigger)?;
            settle(db, &schedule, "Expired", Some("DeadlineExceeded"), None).map(|_| ())
        }
        reason => finish_trigger(
            db,
            schedule,
            &decode(&schedule.spec["event"])?,
            trigger,
            "Rejected",
            Some(reason),
            None,
        ),
    }
}

fn advance_source(db: &Db<'_>, schedule: &Doc, previous: i64, next: i64) -> Result<()> {
    require(
        db.c.execute(
            "UPDATE pending_event_schedules SET source_rev=?3 WHERE schedule=?1 AND source_rev=?2",
            params![schedule.id, previous, next],
        )? == 1,
        "RecoveryRequired",
        "event schedule cursor changed during settlement",
    )
}

fn fire(db: &Db<'_>, rejected: &mut Option<Attempt>) -> Result<()> {
    let Some(work) = next_work(db)? else {
        return Ok(());
    };
    let schedule = db.of(&work.schedule, "Schedule")?;
    let rule: Rule = decode(&schedule.spec["event"])?;
    require(
        schedule.body["lifecycle"] == "Pending" && rule.source_ref["resource_id"] == work.source,
        "RecoveryRequired",
        "event schedule index disagrees with its rule",
    )?;
    if work.expired {
        settle(db, &schedule, "Expired", Some("DeadlineExceeded"), None)?;
        return Ok(());
    }
    if !node::enabled(db)? || node::work_admission::draining(db)? {
        return Ok(());
    }
    let template = &schedule.spec["request"];
    let delegation = template["delegation_ref"]["resource_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "event schedule delegation missing"))?;
    let authorized = policy::delegation(db, &schedule.owner, delegation, ACTION)
        .and_then(|_| event_permissions(db, &schedule.owner, delegation));
    if let Err(error) = authorized {
        if matches!(
            error.0.code.as_str(),
            "PermissionDenied" | "DeadlineExceeded"
        ) {
            settle(db, &schedule, "Rejected", Some(&error.0.code), None)?;
            return Ok(());
        }
        return Err(error);
    }
    let tool = template["task_type"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "event schedule tool identity missing"))?;
    if db.of(&format!("tool-{tool}"), "Capability")?.reference() != schedule.spec["tool_ref"] {
        settle(db, &schedule, "Rejected", Some("CapabilityChanged"), None)?;
        return Ok(());
    }
    let seq = work.next_seq.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "event schedule lacks its selected revision",
        )
    })?;
    let previous = db.c.query_row(
        "SELECT * FROM versions WHERE id=?1 AND rev=?2",
        params![work.source, work.source_rev],
        row,
    )?;
    let (source, event_id, incarnation) =
        db.c.query_row("SELECT * FROM versions WHERE seq=?1", [seq], |r| {
            Ok((
                row(r)?,
                r.get::<_, String>("event_id")?,
                r.get::<_, String>("incarnation")?,
            ))
        })?;
    require(
        source.id == previous.id && source.rev == previous.rev + 1 && source.kind == previous.kind,
        "RecoveryRequired",
        "event schedule source history is discontinuous",
    )?;
    if !policy::visible(&schedule.owner, &source) || !policy::visible(&schedule.owner, &previous) {
        settle(db, &schedule, "Rejected", Some("PermissionDenied"), None)?;
        return Ok(());
    }
    if !rule.condition.matches(&previous, &source) {
        return advance_source(db, &schedule, previous.rev, source.rev);
    }
    let digest = sha256(&canonical(&json!([
        "psios/schedule/event/1",
        schedule.id,
        schedule.spec["creation_digest"],
        event_id
    ])));
    let source_ref = source.reference();
    let trigger = Trigger {
        id: format!("trigger-{}", &digest[7..]),
        previous_rev: previous.rev,
        source_rev: source.rev,
        event: json!({
            "event_id":event_id,"source_seq":seq.to_string(),"source_authority_id":db.meta("authority")?,
            "recovery_incarnation":incarnation,"resource_ref":source_ref,"occurred_at":source.stamp,
            "event_type":format!("psios.{}.updated.v1",source.kind.to_ascii_lowercase())
        }),
        source_ref,
    };
    if super::previous_task_active(db, &schedule)? {
        return finish_trigger(
            db,
            &schedule,
            &rule,
            &trigger,
            "Skipped",
            Some("PreviousTaskActive"),
            None,
        );
    }
    let request = broker::retime_scheduled(template, &trigger.id, &rule.expires_at)?;
    let receipt = match broker::submit_scheduled(db, &schedule.owner, &request, &schedule.id) {
        Ok(receipt) => receipt,
        Err(error) => {
            if matches!(
                error.0.code.as_str(),
                "QueueFull"
                    | "PermissionDenied"
                    | "PreconditionFailed"
                    | "Conflict"
                    | "NotFound"
                    | "QuotaExceeded"
                    | "UnsupportedCapability"
                    | "DeadlineExceeded"
            ) {
                *rejected = Some(Attempt {
                    schedule: schedule.id.clone(),
                    version: schedule.version(),
                    trigger,
                });
            }
            return Err(error);
        }
    };
    finish_trigger(
        db,
        &schedule,
        &rule,
        &trigger,
        "Triggered",
        None,
        Some(receipt),
    )
}

fn pending_trigger(db: &Db<'_>, schedule: &Doc) -> Result<Option<Doc>> {
    let reference = &schedule.body["pending_trigger_ref"];
    if reference.is_null() {
        return Ok(None);
    }
    let pending = db.resolve(reference)?;
    require(
        pending.kind == "ScheduleTrigger"
            && pending.owner == schedule.owner
            && pending.spec["schedule_id"] == schedule.id
            && pending.spec["creation_digest"] == schedule.spec["creation_digest"]
            && pending.body["lifecycle"] == "Pending",
        "RecoveryRequired",
        "pending event trigger disagrees with its schedule",
    )?;
    Ok(Some(pending))
}

fn require_same_event(pending: &Doc, event: &Trigger) -> Result<()> {
    require(
        pending.id == event.id && pending.body["source_event"] == event.event,
        "RecoveryRequired",
        "pending event trigger no longer matches the source cursor",
    )
}

fn remember_pending(db: &Db<'_>, schedule: &Doc, event: &Trigger) -> Result<Doc> {
    if let Some(pending) = pending_trigger(db, schedule)? {
        require_same_event(&pending, event)?;
        return Ok(schedule.clone());
    }
    let pending = create_trigger(db, schedule, event, "Pending", None, None)?;
    let mut body = schedule.body.clone();
    body["pending_trigger_ref"] = pending.reference();
    db.status(schedule, body)
}

fn create_trigger(
    db: &Db<'_>,
    schedule: &Doc,
    trigger: &Trigger,
    phase: &str,
    reason: Option<&str>,
    receipt: Option<&Value>,
) -> Result<Doc> {
    db.create(
        "ScheduleTrigger",
        &trigger.id,
        &schedule.owner,
        json!({"schedule_id":schedule.id,"schedule_version":"r1","creation_digest":schedule.spec["creation_digest"]}),
        json!({
            "trigger_id":trigger.id,"schedule_id":schedule.id,"lifecycle":phase,
            "source_event":trigger.event,"reason":reason,"task_receipt":receipt,
            "settled_at":(phase != "Pending").then(now)
        }),
    )
}

fn close_trigger(
    db: &Db<'_>,
    trigger: &Doc,
    phase: &str,
    reason: Option<&str>,
    receipt: Option<&Value>,
) -> Result<Doc> {
    let mut body = trigger.body.clone();
    body["lifecycle"] = json!(phase);
    body["reason"] = json!(reason);
    body["task_receipt"] = json!(receipt);
    body["settled_at"] = json!(now());
    db.status(trigger, body)
}

fn append_trigger(body: &mut Value, limit: u32, trigger: &Doc) -> Result<bool> {
    let history = body["trigger_refs"]
        .as_array_mut()
        .filter(|history| history.len() < limit as usize)
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "event schedule trigger history is out of bounds",
            )
        })?;
    history.push(trigger.reference());
    let exhausted = history.len() == limit as usize;
    body["pending_trigger_ref"] = Value::Null;
    Ok(exhausted)
}

pub(super) fn settle_pending(
    db: &Db<'_>,
    schedule: &Doc,
    phase: &str,
    reason: Option<&str>,
    body: &mut Value,
) -> Result<()> {
    if let Some(pending) = pending_trigger(db, schedule)? {
        let rule: Rule = decode(&schedule.spec["event"])?;
        let closed = close_trigger(db, &pending, phase, reason, None)?;
        append_trigger(body, rule.max_triggers, &closed)?;
        db.audit(
            &schedule.owner,
            "Schedule.EventTrigger",
            Some(&pending.id),
            phase,
        )?;
    }
    Ok(())
}

fn finish_trigger(
    db: &Db<'_>,
    schedule: &Doc,
    rule: &Rule,
    trigger: &Trigger,
    phase: &str,
    reason: Option<&str>,
    receipt: Option<Value>,
) -> Result<()> {
    let mut body = schedule.body.clone();
    let record = if let Some(pending) = pending_trigger(db, schedule)? {
        require_same_event(&pending, trigger)?;
        close_trigger(db, &pending, phase, reason, receipt.as_ref())?
    } else {
        create_trigger(db, schedule, trigger, phase, reason, receipt.as_ref())?
    };
    advance_source(db, schedule, trigger.previous_rev, trigger.source_rev)?;
    let exhausted = append_trigger(&mut body, rule.max_triggers, &record)?;
    body["source_ref"] = trigger.source_ref.clone();
    if let Some(receipt) = receipt {
        body["last_task_id"] = receipt["task_id"].clone();
    }
    if exhausted {
        remove_pending(db, schedule)?;
        body["lifecycle"] = json!("Completed");
        body["settled_at"] = json!(now());
    }
    db.status(schedule, body)?;
    db.audit(
        &schedule.owner,
        "Schedule.EventTrigger",
        Some(&trigger.id),
        phase,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use psios_api::{Request, VERSION};

    #[test]
    fn crossing_the_admission_deadline_expires_the_selected_event() {
        let root = tempfile::tempdir().unwrap();
        let mut config = Config::local(&root.path().join("state"));
        config.queue_limit = 1;
        Store::init(&config).unwrap();
        let token = std::fs::read_to_string(config.witness_dir.join("owner.token")).unwrap();
        let mut store = Store::open(config).unwrap();
        let call = |store: &mut Store, method: &str, params: Value| {
            store
                .request(
                    rustix::process::geteuid().as_raw(),
                    Request {
                        schema_version: VERSION.into(),
                        request_id: id("test"),
                        method: method.into(),
                        token: token.clone(),
                        params,
                    },
                )
                .unwrap()
        };
        let prepared = call(
            &mut store,
            "Task.Prepare",
            json!({"key":"queue-blocker","tool":"system.status","input":{},"delegation_id":"owner-delegation"}),
        );
        call(&mut store, "Task.Submit", prepared);
        let grant = call(
            &mut store,
            "Grant.Issue",
            json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read","resource.read",
                "event.read","schedule.manage","system.status"],"units":2,"ttl_seconds":300,
                "tool_targets":[policy::PROFILE]}),
        );
        for staged in [false, true] {
            let node = call(&mut store, "Node.Get", json!({}))["reference"].clone();
            let created = call(
                &mut store,
                "Schedule.Create",
                json!({
                    "key":id("schedule"),"delegation_id":grant["delegation_ref"]["resource_id"],
                    "tool":"system.status","input":{},
                    "event":{"source_ref":node,"condition":{"kind":"SpecChanged"},
                             "expires_at":crate::after(120),"max_triggers":1}
                }),
            );
            let cordon = call(
                &mut store,
                "Node.Configure",
                json!({"expected_resource_version":node["version"],"scheduling":"Cordoned"}),
            );
            call(
                &mut store,
                "Node.Configure",
                json!({"expected_resource_version":cordon["version"],"scheduling":"Enabled"}),
            );

            let mut rejected = None;
            let blocked = store.transaction("queue-at-limit", |db| fire(db, &mut rejected));
            assert_eq!(blocked.unwrap_err().0.code, "QueueFull");
            let attempt = rejected.unwrap();
            assert_eq!(created["reference"]["resource_id"], attempt.schedule);
            store
                .transaction("deadline-during-admission", |db| {
                    let mut schedule = db.of(&attempt.schedule, "Schedule")?;
                    if staged {
                        schedule = remember_pending(db, &schedule, &attempt.trigger)?;
                    }
                    record_attempt_failure(
                        db,
                        &schedule,
                        &attempt.trigger,
                        &err("DeadlineExceeded", "task admission deadline passed"),
                    )?;
                    let result = db.of(&schedule.id, "Schedule")?;
                    assert_eq!(result.body["lifecycle"], "Expired");
                    assert!(result.body["pending_trigger_ref"].is_null());
                    let trigger = db.resolve(&result.body["trigger_refs"][0])?;
                    assert_eq!(trigger.id, attempt.trigger.id);
                    assert_eq!(trigger.body["lifecycle"], "Expired");
                    assert_eq!(trigger.body["source_event"]["resource_ref"], cordon);
                    assert!(trigger.body["task_receipt"].is_null());
                    let reserved = db.c.query_row(
                        "SELECT reserved+spent FROM budgets WHERE id=?1",
                        [grant["budget_ref"]["resource_id"].as_str().unwrap()],
                        |row| row.get::<_, u32>(0),
                    )?;
                    assert_eq!(reserved, 0);
                    Ok(())
                })
                .unwrap();
        }
    }
}
