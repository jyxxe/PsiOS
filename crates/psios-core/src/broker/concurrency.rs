//! Additional dispatch limits shared by all work using a root authorization budget.
use crate::{Result, db::Db};
use rusqlite::{OptionalExtension, params};

pub(crate) const FORMAT: u32 = 40;
pub(crate) const SCHEMA: &str = include_str!("concurrency.sql");
pub(crate) const MAX_TASKS_PER_BUDGET: u32 = 16;

pub(crate) fn register(db: &Db<'_>, budget: &str, maximum: u32) -> Result<()> {
    db.c.execute(
        "INSERT INTO task_concurrency VALUES(?1,?2)",
        params![budget, maximum],
    )?;
    Ok(())
}

pub(crate) struct Usage {
    pub maximum: u32,
    pub occupied: u32,
    pub unknown: u32,
}

pub(crate) fn usage(db: &Db<'_>, budget: &str) -> Result<Option<Usage>> {
    Ok(db
        .c
        .query_row(
            "SELECT c.maximum,
           (SELECT count(*) FROM occupied_task_slots WHERE budget=c.budget),
           (SELECT count(*) FROM occupied_task_slots WHERE budget=c.budget AND phase='Unknown')
         FROM task_concurrency c WHERE c.budget=?1",
            [budget],
            |row| {
                Ok(Usage {
                    maximum: row.get(0)?,
                    occupied: row.get(1)?,
                    unknown: row.get(2)?,
                })
            },
        )
        .optional()?)
}
