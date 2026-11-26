//! Periodically recheck enrolled attachments without using raw hotplug events as evidence.
use super::{KIND, changes_state, identity, sample, state, watched};
use crate::{
    Result,
    db::{Db, Doc},
    require,
};
use rusqlite::params;
use serde_json::Value;

const BATCH_SIZE: i64 = 8;
const PAGE: &str = "SELECT device_id,origin_seq FROM block_device_watch
                    WHERE origin_seq>?1 AND origin_seq<=?2 ORDER BY origin_seq LIMIT ?3";

#[derive(Clone, Copy, Default)]
pub(crate) struct Cursor {
    after: i64,
    through: i64,
}

pub(crate) struct Batch {
    devices: Vec<Doc>,
    node: String,
    incarnation: String,
}

pub(crate) fn next(db: &Db<'_>, cursor: Cursor) -> Result<(Cursor, Batch)> {
    let through = if cursor.through == 0 {
        db.c.query_row(
            "SELECT coalesce(max(origin_seq),0) FROM block_device_watch",
            [],
            |row| row.get(0),
        )?
    } else {
        cursor.through
    };
    // Pin each sweep at the current last eligible enrollment. The primary-key
    // range excludes prior terminal histories before the eight-slot budget.
    let mut origins = db.c.prepare_cached(PAGE)?;
    let mut devices = Vec::new();
    let mut after = through;
    for origin in origins.query_map(params![cursor.after, through, BATCH_SIZE], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?))
    })? {
        let (id, seq) = origin?;
        after = seq;
        let device = db.of(&id, KIND)?;
        require(
            watched(&device.body),
            "RecoveryRequired",
            "block enrollment projection does not refer to an active device",
        )?;
        devices.push(device);
    }
    let next = if after < through {
        Cursor { after, through }
    } else {
        Cursor::default()
    };
    Ok((
        next,
        Batch {
            devices,
            node: db.meta("authority")?,
            incarnation: db.incarnation.clone(),
        },
    ))
}

pub(crate) struct Revisions {
    devices: Vec<(Doc, Value)>,
    incarnation: String,
}

impl Batch {
    pub(crate) fn sample(self) -> Result<Revisions> {
        let mut changed = Vec::new();
        for device in self.devices {
            let expected = identity(&device, &self.node)?;
            // An unavailable or racing kernel observation is not a state change.
            if let Ok(receipt) = sample(&expected)
                && changes_state(&device, &receipt)
            {
                let body = state(&expected, &receipt, device.rev + 1);
                changed.push((device, body));
            }
        }
        Ok(Revisions {
            devices: changed,
            incarnation: self.incarnation,
        })
    }
}

impl Revisions {
    pub(crate) fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    pub(crate) fn publish(self, db: &Db<'_>) -> Result<()> {
        if self.incarnation != db.incarnation {
            return Ok(());
        }
        db.cfg.check_admission_space()?;
        for (previous, body) in self.devices {
            let current = db.of(&previous.id, KIND)?;
            if current.reference() == previous.reference() {
                db.status(&current, body)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, node::block_device::retirement, policy};
    use rusqlite::{Connection, StatementStatus};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn eligible_sweep_seeks_past_terminal_history_with_a_fixed_end() -> Result<()> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(include_str!("../../state.sql"))?;
        conn.execute_batch(include_str!("../../events/snapshots.sql"))?;
        conn.execute("INSERT INTO meta VALUES('authority','node-test')", [])?;
        let config = Config::local(Path::new("unused-block-sweep-test"));
        let db = Db {
            c: &conn,
            cfg: &config,
            keys: &[],
            correlation: "test-sweep",
            incarnation: "incarnation-test".to_string(),
        };
        let present = |id: &str| {
            db.create(
                KIND,
                id,
                policy::OWNER,
                json!({}),
                json!({"lifecycle":"Present"}),
            )
        };
        for index in 0..256 {
            let old = present(&format!("terminal-{index}"))?;
            db.status(&old, json!({"lifecycle":"Replaced"}))?;
        }
        let original = present("pre-migration-live")?;
        db.status(&original, json!({"lifecycle":"Absent"}))?;
        conn.execute_batch(retirement::SCHEMA)?;
        for index in 0..8 {
            present(&format!("eligible-{index}"))?;
        }

        let (cursor, first) = next(&db, Cursor::default())?;
        assert_eq!(first.devices.len(), 8);
        assert_eq!(first.devices[0].id, original.id);
        assert_eq!(first.devices[0].body["lifecycle"], "Absent");
        assert_eq!(first.devices[7].id, "eligible-6");
        let mut statement = conn.prepare(PAGE)?;
        let rows = statement
            .query_map(params![0, cursor.through, BATCH_SIZE], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(rows.len(), 8);
        assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
        assert!(statement.get_status(StatementStatus::VmStep) < 128);

        let arriving = present("after-sweep-cut")?;
        let (rolled, final_page) = next(&db, cursor)?;
        assert_eq!(rolled.through, 0);
        assert_eq!(final_page.devices.len(), 1);
        assert_eq!(final_page.devices[0].id, "eligible-7");

        for device in first.devices.iter().chain(final_page.devices.iter()) {
            db.status(device, json!({"lifecycle":"Retired"}))?;
        }
        let (_, upcoming) = next(&db, Cursor::default())?;
        assert_eq!(upcoming.devices.len(), 1);
        assert_eq!(upcoming.devices[0].id, arriving.id);
        assert!(db.find(&original.id)?.is_some());
        retirement::verify_projection(&conn)?;
        conn.execute(
            "DELETE FROM block_device_watch WHERE device_id=?1",
            [&arriving.id],
        )?;
        assert_eq!(
            retirement::verify_projection(&conn).unwrap_err().0.code,
            "RecoveryRequired"
        );
        conn.execute(
            "INSERT INTO block_device_watch SELECT seq,id FROM versions WHERE id IN (?1,?2) AND rev=1",
            [&arriving.id, &original.id],
        )?;
        assert_eq!(
            retirement::verify_projection(&conn).unwrap_err().0.code,
            "RecoveryRequired"
        );
        Ok(())
    }
}
