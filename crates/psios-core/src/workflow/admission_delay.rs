//! Reorder ready Workflow runs after ordinary Broker admission pressure.
use super::RUN;
use crate::{
    Result,
    db::{Db, Doc, find},
    err, policy, require,
};
use chrono::Utc;
use rusqlite::{Connection, params};

pub(crate) const FORMAT: u32 = crate::artifact::ingress::authority::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("admission_delay.sql");
pub(super) const BACKOFF_MS: i64 = 1_000;

fn pending_step(run: &Doc, step: &str) -> bool {
    run.kind == RUN
        && matches!(
            run.body["lifecycle"].as_str(),
            Some("Ready" | "Running" | "WaitingApproval")
        )
        && run.body["stop_reason"].is_null()
        && run.body["steps"].as_array().is_some_and(|steps| {
            steps.iter().any(|current| {
                current["step_id"] == step
                    && current["lifecycle"] == "Pending"
                    && current["task_ref"].is_null()
                    && current["task_receipt"].is_null()
            })
        })
}

pub(super) fn defer(db: &Db<'_>, id: &str, original_rev: &str, step: &str) -> Result<()> {
    let run = db.of(id, RUN)?;
    run.cas(original_rev)?;
    require(
        pending_step(&run, step),
        "RecoveryRequired",
        "deferred workflow step is no longer the original pending admission",
    )?;
    let clock = policy::deadline::Clock::sample()?;
    // Move behind every currently ready run. A fixed cooldown alone could
    // continuously cycle through many older blocked runs before a later one.
    // The ordering does not affect deadlines or the identity of the step.
    require(
        db.c.execute(
            "UPDATE active_workflows SET ready_ms=max(?2,
                  (SELECT max(ready_ms) FROM active_workflows WHERE ready_ms IS NOT NULL))+1
              WHERE run=?1 AND stopping=0 AND result_blocked=0 AND ready_ms IS NOT NULL",
            params![id, Utc::now().timestamp_millis()],
        )? == 1,
        "RecoveryRequired",
        "deferred workflow has no ready scheduler entry",
    )?;
    db.c.execute(
        "INSERT INTO workflow_admission_deferrals(run,run_rev,step,boot_id,time_ns,deferred_boot_ms)
         VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(run) DO UPDATE SET run_rev=excluded.run_rev,
         step=excluded.step,boot_id=excluded.boot_id,time_ns=excluded.time_ns,
         deferred_boot_ms=excluded.deferred_boot_ms",
        params![id, run.rev, step, clock.boot.boot_id, clock.time_ns, clock.boot.millis],
    )?;
    Ok(())
}

pub(super) fn clear(db: &Db<'_>, run: &Doc) -> Result<()> {
    db.c.execute(
        "DELETE FROM workflow_admission_deferrals WHERE run=?1",
        [&run.id],
    )?;
    Ok(())
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let mut query = conn.prepare(
        "SELECT d.run,d.run_rev,d.step,d.boot_id,d.time_ns,
                w.ready_ms IS NOT NULL AND w.stopping=0 AND w.result_blocked=0
           FROM workflow_admission_deferrals d LEFT JOIN active_workflows w ON w.run=d.run",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let revision: i64 = row.get(1)?;
        let step: String = row.get(2)?;
        let boot_id: String = row.get(3)?;
        let time_ns: String = row.get(4)?;
        let ready: Option<bool> = row.get(5)?;
        let run = find(conn, &id)?
            .ok_or_else(|| err("RecoveryRequired", "deferred workflow has no run"))?;
        require(
            ready == Some(true)
                && run.rev == revision
                && pending_step(&run, &step)
                && uuid::Uuid::parse_str(&boot_id).is_ok_and(|boot| boot.to_string() == boot_id)
                && (time_ns == "unsupported"
                    || time_ns
                        .parse::<u64>()
                        .is_ok_and(|ns| ns != 0 && ns.to_string() == time_ns)),
            "RecoveryRequired",
            "workflow admission delay disagrees with its pending step or clock scope",
        )?;
    }
    Ok(())
}
