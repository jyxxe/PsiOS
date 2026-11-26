//! A fair pass over one indexed queue of durable reclamations.
use crate::{Result, now};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Default)]
pub(crate) struct Cursor {
    through: Option<String>,
    after: (String, String),
}

impl Cursor {
    // The query returns (due time, ID) after the last key, through the fixed
    // cutoff. A backwards wall-clock step cannot make unopened work due early.
    pub(super) fn select_due(&mut self, conn: &Connection, query: &str) -> Result<Option<&str>> {
        let current = now();
        let through = self.through.get_or_insert_with(|| current.clone());
        let cutoff = current.as_str().min(through.as_str());
        let position = conn
            .query_row(query, params![cutoff, self.after.0, self.after.1], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let Some(position) = position else {
            *self = Self::default();
            return Ok(None);
        };
        // Advance before validation or physical disposal. Neither a failing
        // receipt nor a rollback of its transaction moves this cursor back.
        self.after = position;
        Ok(Some(&self.after.1))
    }
}
