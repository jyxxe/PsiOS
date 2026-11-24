//! Local, immutable DAG definitions and durable Task-broker admission.
mod condition;
mod run;

use crate::{
    ById, Result, broker,
    db::{Db, Doc},
    decode, err,
    host::Action,
    label, policy, require,
};
use chrono::{DateTime, SecondsFormat, Utc};
use condition::Predicate;
use psios_api::{canonical, sha256};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub use run::{cancel, tick};
pub const ACTION: &str = "workflow.manage";
const MAX_STEPS: usize = 16;
const DEFINITION: &str = "WorkflowDefinition";
const RUN: &str = "WorkflowRun";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Step {
    id: String,
    tool: String,
    input: Value,
    after: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<Predicate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Define {
    key: String,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    key: String,
    definition_ref: Value,
    delegation_id: String,
    deadline_at: String,
}

#[derive(Deserialize, Serialize)]
struct FrozenStep {
    step: Step,
    tool_ref: Value,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PreparedStep {
    id: String,
    step_key: String,
    after: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<Predicate>,
    tool_ref: Value,
    request: Value,
}

fn stable_id(kind: &str, input: &Value) -> String {
    let digest = sha256(&canonical(input));
    format!("{kind}-{}", &digest[7..])
}

fn run_view(d: &Doc) -> Value {
    json!({"reference": d.reference(), "definition_ref": d.spec["definition_ref"],
           "deadline_at": d.spec["deadline_at"], "state": d.body})
}

fn definition(d: &Doc) -> Value {
    json!({"reference": d.reference(), "definition": d.spec})
}

fn plan(db: &Db<'_>, steps: Vec<Step>) -> Result<Vec<FrozenStep>> {
    require(
        !steps.is_empty() && steps.len() <= MAX_STEPS,
        "InvalidArgument",
        "workflow requires 1 to 16 steps",
    )?;
    let ids: BTreeSet<_> = steps.iter().map(|s| s.id.as_str()).collect();
    require(
        ids.len() == steps.len(),
        "InvalidArgument",
        "duplicate workflow step ID",
    )?;
    for step in &steps {
        label(&step.id)?;
        let parents: BTreeSet<_> = step.after.iter().map(String::as_str).collect();
        require(
            parents.len() == step.after.len()
                && parents.iter().all(|p| ids.contains(p) && *p != step.id),
            "InvalidArgument",
            "workflow step dependencies must be distinct existing steps",
        )?;
        if let Some(predicate) = &step.when {
            predicate.validate(&parents)?;
        }
    }
    let mut visited = BTreeSet::new();
    while visited.len() < steps.len() {
        let ready = steps.iter().find(|s| {
            !visited.contains(s.id.as_str()) && s.after.iter().all(|p| visited.contains(p.as_str()))
        });
        let ready =
            ready.ok_or_else(|| err("InvalidArgument", "workflow dependencies contain a cycle"))?;
        visited.insert(ready.id.as_str());
    }
    steps
        .into_iter()
        .map(|step| {
            Action::parse(&step.tool, &step.input)?;
            let tool_ref = db
                .of(&format!("tool-{}", step.tool), "Capability")?
                .reference();
            Ok(FrozenStep { step, tool_ref })
        })
        .collect()
}

pub fn define(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Define = decode(params)?;
    label(&request.key)?;
    policy::can(db, actor, ACTION)?;
    let id = stable_id(
        "workflow-definition",
        &json!([db.meta("authority")?, actor, request.key]),
    );
    let bytes = canonical(&json!(request.steps));
    require(
        bytes.len() <= 128 * 1024,
        "InvalidArgument",
        "workflow definition exceeds 128 KiB",
    )?;
    let creation = sha256(&bytes);
    if let Some(existing) = db.find(&id)? {
        require(
            existing.kind == DEFINITION
                && existing.owner == actor
                && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "workflow definition key already bound",
        )?;
        return Ok(definition(&existing));
    }
    db.cfg.check_admission_space()?;
    let version = if request.steps.iter().any(|step| step.when.is_some()) {
        2
    } else {
        1
    };
    let steps = plan(db, request.steps)?;
    let mut spec = json!({"dsl_version": version, "definition_version": "r1", "steps": steps});
    spec["definition_digest"] = json!(sha256(&canonical(&spec)));
    spec["creation_digest"] = json!(creation);
    let d = db.create(
        DEFINITION,
        &id,
        actor,
        spec,
        json!({"lifecycle": "Registered"}),
    )?;
    Ok(definition(&d))
}

pub fn get(db: &Db<'_>, actor: &str, params: &Value, kind: &str) -> Result<Value> {
    let request: ById = decode(params)?;
    policy::can(db, actor, "task.read")?;
    let d = db.of(&request.id, kind)?;
    policy::owned(actor, &d)?;
    Ok(if kind == DEFINITION {
        definition(&d)
    } else {
        run_view(&d)
    })
}

fn instant(value: &str) -> Result<DateTime<Utc>> {
    let time = DateTime::parse_from_rfc3339(value).map_err(|_| {
        err(
            "InvalidArgument",
            "workflow deadline requires an RFC 3339 offset",
        )
    })?;
    require(
        time.timestamp_subsec_nanos() < 1_000_000_000,
        "InvalidArgument",
        "workflow deadline requires a POSIX time",
    )?;
    Ok(time.to_utc())
}

pub fn start(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: Start = decode(params)?;
    label(&request.key)?;
    policy::active(db, actor)?;
    let deadline = instant(&request.deadline_at)?;
    let deadline_at = deadline.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let id = stable_id(
        "workflow-run",
        &json!([db.meta("authority")?, actor, request.key]),
    );
    let creation = sha256(&canonical(&json!([
        request.definition_ref,
        request.delegation_id,
        deadline_at
    ])));
    if let Some(existing) = db.find(&id)? {
        policy::can(db, actor, "task.read")?;
        require(
            existing.kind == RUN
                && existing.owner == actor
                && existing.spec["creation_digest"] == creation,
            "IdempotencyConflict",
            "workflow run key already bound",
        )?;
        return Ok(run_view(&existing));
    }
    let delegation = policy::delegation(db, actor, &request.delegation_id, ACTION)?;
    let grant = db.of(delegation.body["grant_id"].as_str().unwrap(), "Grant")?;
    let expires = instant(delegation.body["valid_until"].as_str().unwrap())?
        .min(instant(grant.spec["valid_until"].as_str().unwrap())?);
    require(
        Utc::now() < deadline && deadline <= expires,
        "InvalidArgument",
        "workflow deadline must be within authorization",
    )?;
    let d = db.resolve(&request.definition_ref)?;
    policy::owned(actor, &d)?;
    require(
        d.kind == DEFINITION && matches!(d.spec["dsl_version"].as_u64(), Some(1 | 2)),
        "UnsupportedCapability",
        "unsupported workflow definition",
    )?;
    let frozen: Vec<FrozenStep> = decode(&d.spec["steps"])?;
    let conditional = frozen.iter().any(|step| step.step.when.is_some());
    require(
        conditional == (d.spec["dsl_version"] == 2),
        "RecoveryRequired",
        "workflow definition version disagrees with conditional steps",
    )?;
    if conditional {
        policy::delegation(db, actor, &request.delegation_id, "task.read")?;
    }
    db.cfg.check_admission_space()?;
    let active: u32 =
        db.c.query_row("SELECT count(*) FROM active_workflows", [], |row| {
            row.get(0)
        })?;
    require(
        active < db.cfg.queue_limit,
        "QueueFull",
        "active workflow capacity reached",
    )?;
    let available: u32 = db.c.query_row(
        "SELECT maximum-reserved-spent FROM budgets WHERE id=?1",
        [delegation.body["budget_ref"]["resource_id"]
            .as_str()
            .unwrap()],
        |row| row.get(0),
    )?;
    require(
        usize::try_from(available).unwrap() >= frozen.len(),
        "QuotaExceeded",
        "insufficient units for workflow steps",
    )?;

    let steps = frozen
        .into_iter()
        .map(|FrozenStep { step, tool_ref }| {
            check_tool(db, &step.tool, &tool_ref)?;
            let action = Action::parse(&step.tool, &step.input)?;
            let target = action.target(db, actor)?;
            action.audience(db, actor, &delegation.reference(), &target)?;
            let step_key = stable_id("workflow-step", &json!([id, step.id]));
            let key = stable_id("workflow-dispatch", &json!([step_key, "1"]));
            let task = broker::prepare_workflow(
                db,
                actor,
                &json!({
                    "tool": step.tool, "input": step.input, "key": key,
                    "delegation_id": request.delegation_id, "deadline_at": deadline_at,
                }),
                &id,
                &step_key,
            )?;
            Ok(PreparedStep {
                id: step.id,
                step_key,
                after: step.after,
                when: step.when,
                tool_ref,
                request: task,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let projections: Vec<_> = steps
        .iter()
        .map(|step| {
            json!({"step_id": step.id, "step_key": step.step_key, "lifecycle": "Pending",
                   "task_receipt": null, "task_ref": null})
        })
        .collect();
    let run = db.create(
        RUN,
        &id,
        actor,
        json!({"definition_ref": d.reference(), "creation_digest": creation, "steps": steps,
               "dsl_version": d.spec["dsl_version"],
               "delegation_id": request.delegation_id, "deadline_at": deadline_at}),
        json!({"lifecycle": "Ready", "outcome_knowledge": "NotStarted", "reason": null,
               "active_step": null, "stop_reason": null, "steps": projections}),
    )?;
    let deadline_ms = deadline.timestamp_millis()
        + i64::from(!deadline.timestamp_subsec_nanos().is_multiple_of(1_000_000));
    db.c.execute(
        "INSERT INTO active_workflows(run,deadline_ms,ready_ms) VALUES(?1,?2,?3)",
        params![id, deadline_ms, Utc::now().timestamp_millis()],
    )?;
    Ok(run_view(&run))
}

fn check_tool(db: &Db<'_>, tool: &str, pinned: &Value) -> Result<()> {
    let version = db.of(&format!("tool-{tool}"), "Capability")?.reference();
    require(
        version == *pinned,
        "PreconditionFailed",
        "workflow tool revision changed",
    )
}
