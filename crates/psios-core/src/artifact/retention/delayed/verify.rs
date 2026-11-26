//! Check both sides of the schedule queue and the exact aggregate receipt.
use super::{
    KIND, MAX_PENDING, Pending, Phase, State, batch, check_projection, invalid, verify_record,
};
use crate::{
    Result,
    db::{Doc, find, resolve, row},
    policy, require,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::collections::BTreeMap;

struct Revision {
    seq: i64,
    doc: Doc,
    correlation: String,
    incarnation: String,
}

fn revision(row_: &Row<'_>) -> rusqlite::Result<Revision> {
    Ok(Revision {
        seq: row_.get("seq")?,
        doc: row(row_)?,
        correlation: row_.get("correlation")?,
        incarnation: row_.get("incarnation")?,
    })
}

fn queue(conn: &Connection) -> Result<BTreeMap<String, Pending>> {
    let mut pending = BTreeMap::new();
    let mut statement = conn.prepare(
        "SELECT schedule,scope_digest,due_boot_ms FROM pending_result_eviction_schedules LIMIT ?1",
    )?;
    for value in statement.query_map([MAX_PENDING + 1], Pending::row)? {
        let value = value?;
        require(
            pending.len() < MAX_PENDING as usize,
            "RecoveryRequired",
            "eviction schedule queue exceeds its durable limit",
        )?;
        pending.insert(value.schedule.clone(), value);
    }
    Ok(pending)
}

pub(crate) fn verify_pending(conn: &Connection) -> Result<()> {
    for pending in queue(conn)?.into_values() {
        let record = find(conn, &pending.schedule)?.ok_or_else(invalid)?;
        let (plan, state) = verify_record(&record)?;
        check_projection(&record, &plan, &state, Some(pending))?;
    }
    Ok(())
}

fn admission(conn: &Connection, original: &Revision, plan: &batch::Plan) -> Result<()> {
    let mut latest = conn.prepare_cached(
        "SELECT * FROM versions WHERE id=?1 AND seq<?2 ORDER BY rev DESC LIMIT 1",
    )?;
    let owner = latest
        .query_row(params![policy::OWNER, original.seq], row)
        .optional()?
        .ok_or_else(invalid)?;
    require(
        owner.kind == "Principal"
            && owner.body["lifecycle"] == "Active"
            && plan.recovery_incarnation == original.incarnation,
        "RecoveryRequired",
        "result eviction schedule lost its admission authority",
    )?;
    for item in &plan.items {
        let task = latest
            .query_row(params![item.task_ref.resource_id, original.seq], row)
            .optional()?
            .ok_or_else(invalid)?;
        require(
            task.kind == "Task"
                && task.reference() == serde_json::to_value(&item.task_ref)?
                && matches!(
                    task.body["lifecycle"].as_str(),
                    Some("Succeeded" | "Cancelled")
                )
                && task.body["outcome_knowledge"] == "Known"
                && task.body["result"]["availability"] == "Available",
            "RecoveryRequired",
            "result eviction schedule lost its exact admission Task revision",
        )?;
        let tool = super::super::read_only_tool(conn, &task).map_err(|_| invalid())?;
        require(
            policy::readonly(tool),
            "RecoveryRequired",
            "result eviction schedule was prepared for a mutating Task",
        )?;
    }
    Ok(())
}

fn terminal(
    conn: &Connection,
    original: &Revision,
    closed: &Revision,
    plan: &batch::Plan,
    state: &State,
) -> Result<()> {
    let before = &original.doc;
    let after = &closed.doc;
    let exact = state.scheduled_ref.as_ref().ok_or_else(invalid)?;
    require(
        original.seq < closed.seq
            && before.reference() == serde_json::to_value(exact)?
            && before.spec == after.spec
            && before.created == after.created,
        "RecoveryRequired",
        "result eviction schedule changed its admission during settlement",
    )?;
    let initial = State::new(plan, state.admitted_at.clone(), state.admitted_boot_ms)?;
    require(
        before.body == serde_json::to_value(initial)?,
        "RecoveryRequired",
        "result eviction schedule changed its durable due time",
    )?;
    if state.lifecycle != Phase::Evicted {
        // Lost authorization can settle in a new recovery incarnation; task
        // eligibility cannot be examined durably outside the admitted scope.
        require(
            state.lifecycle != Phase::Rejected || closed.incarnation == original.incarnation,
            "RecoveryRequired",
            "result eviction rejection crossed evaluation incarnations",
        )?;
        return Ok(());
    }
    let batch_ref = serde_json::to_value(state.batch_ref.as_ref().ok_or_else(invalid)?)?;
    let batch = conn
        .query_row(
            "SELECT * FROM versions WHERE seq=?1 AND kind=?2",
            params![closed.seq - 1, batch::KIND],
            revision,
        )
        .optional()?
        .ok_or_else(invalid)?;
    require(
        resolve(conn, &batch_ref).is_ok_and(|doc| doc.reference() == batch.doc.reference())
            && batch.doc.reference() == batch_ref
            && batch.doc.owner == before.owner
            && batch.doc.rev == 1
            && batch.doc.id == batch::receipt_id(&state.plan_digest)
            && batch.doc.spec == before.spec
            && batch.doc.body["schedule_ref"] == before.reference()
            && batch.correlation == closed.correlation
            && batch.incarnation == closed.incarnation
            && batch.incarnation == original.incarnation,
        "RecoveryRequired",
        "result eviction schedule has no exact same-transaction batch receipt",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let actual = queue(conn)?;
    let authority: String =
        conn.query_row("SELECT value FROM meta WHERE key='authority'", [], |row| {
            row.get(0)
        })?;
    let mut active = BTreeMap::<String, (Revision, Pending)>::new();
    let mut statement = conn.prepare("SELECT * FROM versions WHERE kind=?1 ORDER BY seq")?;
    for record in statement.query_map([KIND], revision)? {
        let record = record?;
        let (plan, state) = verify_record(&record.doc)?;
        require(
            plan.authority_id == authority,
            "RecoveryRequired",
            "result eviction schedule belongs to a foreign authority",
        )?;
        let id = record.doc.id.clone();
        if state.lifecycle == Phase::Scheduled {
            require(
                active.len() < MAX_PENDING as usize && !active.contains_key(&id),
                "RecoveryRequired",
                "historical result eviction schedule exceeded its pending limit",
            )?;
            admission(conn, &record, &plan)?;
            let pending = Pending::new(&id, &plan, &state);
            active.insert(id, (record, pending));
        } else {
            let (original, _) = active.remove(&id).ok_or_else(invalid)?;
            terminal(conn, &original, &record, &plan, &state)?;
        }
    }
    let expected: BTreeMap<_, _> = active
        .into_iter()
        .map(|(id, (_, pending))| (id, pending))
        .collect();
    require(
        actual == expected,
        "RecoveryRequired",
        "pending result eviction queue disagrees with exact schedule history",
    )
}
