use super::*;
pub(super) use crate::linux_clock::BootClock;

impl BootClock {
    pub(super) fn current(&self, db: &Db<'_>, session: &Doc) -> Result<bool> {
        Ok(db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_turns WHERE session=?1 AND lease=?2
             AND receipt IS NULL AND boot_id=?3 AND deadline_boot_ms>?4)",
            params![
                session.id,
                session.body["current_lease"].as_str(),
                self.boot_id,
                self.millis
            ],
            |r| r.get(0),
        )?)
    }
}

// A fenced provider can still be processing its previous call. Reserve its slot
// until the stored deadline even after cancellation. After a reboot or a format
// upgrade only the original UTC estimate is available for residual accounting.
fn outstanding(
    db: &Db<'_>,
    clock: Option<&BootClock>,
    model: Option<&str>,
    grant: Option<&str>,
    provider: Option<&str>,
) -> Result<u32> {
    Ok(db.c.query_row(
        "SELECT count(*) FROM session_turns st JOIN current s ON s.id=st.session
         JOIN current d ON d.id=s.spec->>'delegation_id' JOIN current m ON m.id=s.spec->'model_ref'->>'resource_id'
         WHERE st.receipt IS NULL AND (m.id=?1 OR d.body->>'grant_id'=?2 OR m.spec->>'provider_principal_id'=?3)
         AND CASE WHEN ?4 IS NULL THEN 1 WHEN st.boot_id=?4 THEN st.deadline_boot_ms>?5
                  ELSE julianday(st.expires)>julianday('now') END",
        params![model, grant, provider, clock.map(|c| &c.boot_id), clock.map(|c| c.millis)],
        |r| r.get(0),
    )?)
}

pub(super) fn model_slots(db: &Db<'_>, clock: &BootClock, model: &str) -> Result<u32> {
    outstanding(db, Some(clock), Some(model), None, None)
}

pub(crate) fn pending_for_grant(db: &Db<'_>, grant: &Doc) -> Result<bool> {
    let provider = grant.spec["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action == "model.serve")
        .then_some(grant.owner.as_str());
    // A missing clock must not prevent revocation or claim all work has finished.
    Ok(outstanding(
        db,
        BootClock::sample().ok().as_ref(),
        None,
        Some(&grant.id),
        provider,
    )? > 0)
}
