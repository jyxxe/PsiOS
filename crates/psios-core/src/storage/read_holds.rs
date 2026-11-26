//! Detached Volume readers may coexist while changes and attachments remain exclusive.
use crate::{Result, db::Db, host::Action, require};
use rusqlite::params;

pub(crate) const FORMAT: u32 = super::file_integrity::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("read_holds.sql");

pub(crate) fn reader(action: &Action) -> bool {
    matches!(
        action,
        Action::Read(_)
            | Action::Files(_)
            | Action::Chunk(_)
            | Action::VolumeUsage(_)
            | Action::VerifyVolumeFile(_)
    )
}

pub(super) fn acquire(db: &Db<'_>, volume: &str, task: &str) -> Result<()> {
    db.c.execute(
        "INSERT INTO volume_read_holds(volume,task) VALUES(?1,?2)",
        params![volume, task],
    )?;
    Ok(())
}

pub(super) fn ensure_no_readers(db: &Db<'_>, volume: &str) -> Result<()> {
    let reading: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume_read_holds WHERE volume=?1)",
        [volume],
        |row| row.get(0),
    )?;
    require(!reading, "Conflict", "volume held by an active reader")
}

pub(crate) fn release(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute("DELETE FROM volume_read_holds WHERE task=?1", [task])?;
    Ok(())
}
