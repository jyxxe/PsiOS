use super::lease::Clock;
use crate::{Error, Result, db::Db, err, require};
use rusqlite::{OptionalExtension, params};

pub(crate) const FORMAT: u32 = 37;
pub(crate) const SCHEMA: &str = include_str!("model_time.sql");
pub(crate) const MAX_BUDGET_MS: u32 = 30 * 86_400 * 1_000;

pub(super) enum ClaimRefusal {
    CallBudget,
    TimeBudget,
    PendingTime,
}

impl ClaimRefusal {
    pub fn error(self) -> Error {
        match self {
            Self::CallBudget => err("QuotaExceeded", "root model call budget exhausted"),
            Self::TimeBudget => err("QuotaExceeded", "root model lease time budget exhausted"),
            Self::PendingTime => err("QueueFull", "root model lease time is temporarily reserved"),
        }
    }

    pub fn pause_reason(&self) -> Option<&'static str> {
        match self {
            Self::CallBudget => Some("ModelBudgetExhausted"),
            Self::TimeBudget => Some("ModelTimeBudgetExhausted"),
            Self::PendingTime => None,
        }
    }
}

pub(super) struct Window {
    pub millis: i64,
    pub metered: bool,
}

pub(super) fn window(
    db: &Db<'_>,
    budget: &str,
    clock: &Clock,
    desired_ms: i64,
) -> Result<std::result::Result<Window, ClaimRefusal>> {
    let available: Option<(Option<i64>, i64)> = db.c.query_row(
        "SELECT maximum_lease_ms,charged_lease_ms FROM model_budgets WHERE budget=?1 AND used<maximum",
        [budget], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let Some((limit, charged)) = available else {
        return Ok(Err(ClaimRefusal::CallBudget));
    };
    let Some(limit) = limit else {
        return Ok(Ok(Window {
            millis: desired_ms,
            metered: false,
        }));
    };
    let remaining = limit - charged;
    if remaining > 0 {
        return Ok(Ok(Window {
            millis: desired_ms.min(remaining),
            metered: true,
        }));
    }
    // An active call can still release unused time. Expired, cancelled or
    // fenced calls keep their whole allocation and cannot block the queue.
    let refundable: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM model_lease_times a
         JOIN session_turns t ON t.lease=a.lease JOIN current s ON s.id=t.session
         WHERE a.budget=?1 AND a.settled_ms IS NULL AND t.receipt IS NULL
           AND t.boot_id=?2 AND a.time_ns=?3 AND t.deadline_boot_ms>?4
           AND s.body->>'lifecycle'='ModelInFlight' AND s.body->>'current_lease'=a.lease)",
        params![budget, clock.boot.boot_id, clock.time_ns, clock.boot.millis],
        |row| row.get(0),
    )?;
    Ok(Err(if refundable {
        ClaimRefusal::PendingTime
    } else {
        ClaimRefusal::TimeBudget
    }))
}

impl Window {
    pub fn charge(&self, db: &Db<'_>, clock: &Clock, budget: &str, lease: &str) -> Result<()> {
        let reserve = if self.metered { self.millis } else { 0 };
        require(
            db.c.execute(
                "UPDATE model_budgets SET used=used+1,charged_lease_ms=charged_lease_ms+?2 WHERE budget=?1",
                params![budget,reserve],
            )? == 1,
            "RecoveryRequired", "model budget disappeared while allocating a lease",
        )?;
        if self.metered {
            db.c.execute(
                "INSERT INTO model_lease_times(lease,budget,time_ns,reserved_ms) VALUES(?1,?2,?3,?4)",
                params![lease,budget,clock.time_ns,reserve],
            )?;
        }
        Ok(())
    }
}

pub(super) fn settle(db: &Db<'_>, clock: &Clock, lease: &str, budget: &str) -> Result<Option<i64>> {
    let reserved: Option<(String, i64, i64, bool)> =
        db.c.query_row(
            "SELECT a.budget,a.reserved_ms,t.deadline_boot_ms,a.settled_ms IS NOT NULL FROM model_lease_times a
         JOIN session_turns t ON t.lease=a.lease WHERE a.lease=?1",
            [lease],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((charged_budget, maximum, deadline, settled)) = reserved else {
        return Ok(None);
    };
    let issued = deadline.saturating_sub(maximum);
    require(
        charged_budget == budget && !settled && (issued..deadline).contains(&clock.boot.millis),
        "RecoveryRequired",
        "model time reservation disagrees with the current lease",
    )?;
    let elapsed = (clock.boot.millis - issued).max(1);
    let unused = maximum - elapsed;
    let changed = db.c.execute(
        "UPDATE model_budgets SET charged_lease_ms=charged_lease_ms-?2 WHERE budget=?1 AND charged_lease_ms>=?3",
        params![budget,unused,maximum],
    )?;
    require(
        changed == 1,
        "RecoveryRequired",
        "model time charge cannot be settled",
    )?;
    db.c.execute(
        "UPDATE model_lease_times SET settled_ms=?2 WHERE lease=?1",
        params![lease, elapsed],
    )?;
    Ok(Some(elapsed))
}
