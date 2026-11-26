//! Periodically recheck enrolled attachments without using raw hotplug events as evidence.
use super::{KIND, changes_state, identity, sample, state};
use crate::{
    Result,
    db::{Db, Doc},
    err,
};
use rusqlite::params;
use serde_json::Value;

const BATCH_SIZE: i64 = 8;

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
            "SELECT coalesce(max(seq),0) FROM versions INDEXED BY versions_origin_kind
             WHERE kind=?1 AND rev=1",
            [KIND],
            |row| row.get(0),
        )?
    } else {
        cursor.through
    };
    // Spend the same bounded budget on terminal history. Skipping terminal rows
    // in SQL could scan arbitrarily many enrollments to fill one page.
    let mut origins = db.c.prepare_cached(
        "SELECT id,seq FROM versions INDEXED BY versions_origin_kind
         WHERE kind=?1 AND rev=1 AND seq>?2 AND seq<=?3 ORDER BY seq LIMIT ?4",
    )?;
    let mut devices = Vec::new();
    let mut after = through;
    for origin in origins.query_map(params![KIND, cursor.after, through, BATCH_SIZE], |row| {
        Ok((row.get::<_, String>(0)?, row.get(1)?))
    })? {
        let (id, seq) = origin?;
        after = seq;
        let device = db.of(&id, KIND)?;
        match device.body["lifecycle"].as_str() {
            Some("Present" | "Absent") => devices.push(device),
            Some("Replaced" | "BootEnded") => {}
            _ => return Err(err("RecoveryRequired", "invalid block device lifecycle")),
        }
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
