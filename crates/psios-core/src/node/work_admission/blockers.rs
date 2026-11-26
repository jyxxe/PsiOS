//! Resolve bounded blocker rows against one authority transaction.
use crate::{
    Result,
    db::{Db, Doc},
    decode, err, require,
};
use psios_api::{BlockerPage, ExclusiveHoldBlocker, TaskBlocker, VolumeReadBlocker};
use rusqlite::{OptionalExtension, Row};
use serde_json::{Value, json};

struct QueueEntry {
    task: String,
    operation: Option<String>,
    phase: String,
    recovery_admission: bool,
}

fn invalid(message: &str) -> crate::Error {
    err("RecoveryRequired", message)
}

fn doc(db: &Db<'_>, id: &str) -> Result<Doc> {
    db.find(id)?
        .ok_or_else(|| invalid("maintenance blocker references an unavailable resource"))
}

fn resolve(db: &Db<'_>, reference: &Value) -> Result<Doc> {
    db.resolve(reference).map_err(|error| {
        if matches!(
            error.0.code.as_str(),
            "InvalidArgument" | "PreconditionFailed" | "NotFound"
        ) {
            invalid("maintenance blocker has an invalid resource reference")
        } else {
            error
        }
    })
}

fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| invalid("maintenance blocker has incomplete durable state"))
}

fn page<R, T>(
    mut rows: impl Iterator<Item = rusqlite::Result<R>>,
    limit: u32,
    mut project: impl FnMut(R) -> Result<T>,
) -> Result<BlockerPage<T>> {
    let mut items = Vec::new();
    for row in rows.by_ref().take(limit as usize) {
        items.push(project(row?)?);
    }
    let truncated = rows.next().transpose()?.is_some();
    Ok(BlockerPage { items, truncated })
}

impl QueueEntry {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            task: row.get(0)?,
            operation: row.get(1)?,
            phase: row.get(2)?,
            recovery_admission: row.get(3)?,
        })
    }

    fn get(db: &Db<'_>, task: &str) -> Result<Self> {
        db.c.query_row(
            "SELECT q.task,q.operation,q.phase,r.task IS NOT NULL
             FROM queue q LEFT JOIN recovery_admissions r ON r.task=q.task WHERE q.task=?1",
            [task],
            Self::read,
        )
        .optional()?
        .ok_or_else(|| invalid("maintenance holder has no durable queue entry"))
    }

    fn operation(&self, db: &Db<'_>) -> Result<Doc> {
        doc(
            db,
            self.operation
                .as_deref()
                .ok_or_else(|| invalid("maintenance queue entry lacks an operation"))?,
        )
    }

    fn task(&self, db: &Db<'_>, operation: &Doc) -> Result<Doc> {
        let task = doc(db, &self.task)?;
        require(
            task.kind == "Task"
                && operation.kind == "Operation"
                && self.operation.as_deref() == Some(operation.id.as_str())
                && operation.spec["task_id"] == task.id
                && operation.owner == task.owner
                && task.body["operation_refs"] == json!([operation.reference()]),
            "RecoveryRequired",
            "maintenance queue, task and operation references disagree",
        )?;
        Ok(task)
    }

    fn check_hold_phase(&self, active: &[&str]) -> Result<()> {
        require(
            active.contains(&self.phase.as_str()),
            "RecoveryRequired",
            "maintenance holder is outside an active queue phase",
        )
    }
}

pub(super) fn tasks(db: &Db<'_>, limit: u32) -> Result<BlockerPage<TaskBlocker>> {
    let mut statement = db.c.prepare_cached(
        "SELECT q.task,q.operation,q.phase,r.task IS NOT NULL
         FROM queue q INDEXED BY queue_admitted
         LEFT JOIN recovery_admissions r ON r.task=q.task WHERE q.phase<>'Done'
         ORDER BY CASE q.phase
           WHEN 'Unknown' THEN 0 WHEN 'WaitingInput' THEN 1
           WHEN 'Dispatched' THEN 2 WHEN 'Claimed' THEN 3
           WHEN 'WaitingApproval' THEN 4 WHEN 'WaitingResource' THEN 5
           WHEN 'Queued' THEN 6 ELSE 7 END, q.task
         LIMIT ?1",
    )?;
    let rows = statement.query_map([i64::from(limit) + 1], QueueEntry::read)?;
    page(rows, limit, |entry| {
        entry.check_hold_phase(&[
            "Unknown",
            "WaitingInput",
            "Dispatched",
            "Claimed",
            "WaitingApproval",
            "WaitingResource",
            "Queued",
        ])?;
        let operation = entry.operation(db)?;
        let task = entry.task(db, &operation)?;
        let knowledge = text(&operation.body, "outcome_knowledge")?;
        require(
            (entry.phase == "Unknown") == (knowledge == "Unknown"),
            "RecoveryRequired",
            "maintenance queue and operation outcome disagree",
        )?;
        Ok(TaskBlocker {
            phase: entry.phase,
            task_ref: decode(&task.reference())?,
            operation_ref: decode(&operation.reference())?,
            operation_lifecycle: text(&operation.body, "lifecycle")?.into(),
            outcome_knowledge: knowledge.into(),
            recovery_admission: entry.recovery_admission,
        })
    })
}

fn original_target(db: &Db<'_>, task: &Doc, operation: &Doc) -> Result<Doc> {
    let plan = resolve(db, &operation.body["plan_ref"])?;
    let effect = &plan.body["allowed_effects"][0];
    require(
        plan.kind == "OperationPlan"
            && plan.owner == task.owner
            && effect["action"].is_string()
            && effect["action"] == task.spec["task_type"],
        "RecoveryRequired",
        "maintenance operation is disconnected from its original plan",
    )?;
    resolve(db, &effect["resource_ref"])
}

fn operation_holding(db: &Db<'_>, resource: &Doc, operation: &Doc) -> Result<Doc> {
    let entry = QueueEntry::get(db, text(&operation.spec, "task_id")?)?;
    entry.check_hold_phase(&["Dispatched", "WaitingInput", "Unknown"])?;
    let task = entry.task(db, operation)?;
    let primary = original_target(db, &task, operation)?;
    let direct = primary.id == resource.id && primary.kind == resource.kind;
    // Some application health/log operations hold a detached data volume
    // transiently while their plan's only target is the Application.
    let mounted = resource.kind == "Volume"
        && primary.kind == "Application"
        && primary.spec["volume_id"] == resource.id
        && resource.body["data_incarnation"].is_string()
        && primary.spec["data_incarnation"] == resource.body["data_incarnation"];
    require(
        matches!(
            resource.kind.as_str(),
            "Volume" | "Backup" | "Application" | "HostService" | "NetworkLink"
        ) && (direct || mounted),
        "RecoveryRequired",
        "maintenance operation does not hold this resource",
    )?;
    Ok(task)
}

fn attachment(db: &Db<'_>, volume: &Doc, app: &Doc) -> Result<Doc> {
    let references = volume.body["attachment_refs"]
        .as_array()
        .filter(|references| references.len() == 1)
        .ok_or_else(|| invalid("held volume has no exact application attachment"))?;
    let record = resolve(db, &references[0])?;
    require(
        volume.kind == "Volume"
            && volume.body["lifecycle"] == "Available"
            && app.body["lifecycle"] == "Running"
            && app.spec["volume_id"] == volume.id
            && volume.body["data_incarnation"].is_string()
            && app.spec["data_incarnation"] == volume.body["data_incarnation"]
            && record.kind == "VolumeAttachment"
            && record.owner == app.owner
            && record.body["attachment_id"] == record.id
            && record.body["volume_id"] == volume.id
            && record.body["workload_id"] == app.id
            && record.body["data_incarnation"] == volume.body["data_incarnation"]
            && record.body["node_id"] == db.meta("authority")?
            && record.body["lifecycle"] == "Attached"
            && record.body["requested_access"] == "ReadWrite"
            && record.body["effective_access"] == "ReadWrite"
            && doc(db, &record.id)?.reference() == record.reference(),
        "RecoveryRequired",
        "maintenance volume attachment does not match its current application hold",
    )?;
    Ok(record)
}

pub(super) fn exclusive_holds(
    db: &Db<'_>,
    limit: u32,
) -> Result<BlockerPage<ExclusiveHoldBlocker>> {
    let mut statement =
        db.c.prepare_cached("SELECT volume,holder FROM holds ORDER BY volume LIMIT ?1")?;
    let rows = statement.query_map([i64::from(limit) + 1], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    page(rows, limit, |(resource, holder)| {
        let resource = doc(db, &resource)?;
        let holder = doc(db, &holder)?;
        let (task_ref, attachment_ref) = match holder.kind.as_str() {
            "Operation" => {
                let task = operation_holding(db, &resource, &holder)?;
                (Some(decode(&task.reference())?), None)
            }
            "Application" => {
                let attachment = attachment(db, &resource, &holder)?;
                (None, Some(decode(&attachment.reference())?))
            }
            _ => {
                return Err(invalid(
                    "maintenance resource hold has an unsupported holder",
                ));
            }
        };
        let resource_ref = decode(&resource.reference())?;
        let holder_ref = decode(&holder.reference())?;
        Ok(ExclusiveHoldBlocker {
            resource_kind: resource.kind,
            resource_ref,
            holder_kind: holder.kind,
            holder_ref,
            task_ref,
            attachment_ref,
        })
    })
}

pub(super) fn volume_read_holds(db: &Db<'_>, limit: u32) -> Result<BlockerPage<VolumeReadBlocker>> {
    let mut statement = db.c.prepare_cached(
        "SELECT volume,task FROM volume_read_holds ORDER BY volume,task LIMIT ?1",
    )?;
    let rows = statement.query_map([i64::from(limit) + 1], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    page(rows, limit, |(volume, task)| {
        let volume = doc(db, &volume)?;
        let entry = QueueEntry::get(db, &task)?;
        entry.check_hold_phase(&["Dispatched"])?;
        let operation = entry.operation(db)?;
        let task = entry.task(db, &operation)?;
        let primary = original_target(db, &task, &operation)?;
        require(
            matches!(
                task.spec["task_type"].as_str(),
                Some(
                    "volume.read"
                        | "volume.files"
                        | "volume.read_chunk"
                        | "volume.usage"
                        | "volume.verify_file"
                )
            ) && volume.kind == "Volume"
                && primary.kind == "Volume"
                && primary.id == volume.id
                && volume.body["data_incarnation"].is_string()
                && primary.body["data_incarnation"] == volume.body["data_incarnation"],
            "RecoveryRequired",
            "maintenance reader does not match the original volume",
        )?;
        Ok(VolumeReadBlocker {
            volume_ref: decode(&volume.reference())?,
            task_ref: decode(&task.reference())?,
            operation_ref: decode(&operation.reference())?,
        })
    })
}
