use super::*;
pub(super) use crate::policy::deadline::Clock;

impl Clock {
    pub(super) fn current(&self, db: &Db<'_>, session: &Doc) -> Result<bool> {
        Ok(db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_turns t LEFT JOIN model_lease_times a ON a.lease=t.lease
             LEFT JOIN model_inference_leases i ON i.lease=t.lease
             WHERE t.session=?1 AND t.lease=?2 AND t.receipt IS NULL AND t.boot_id=?3
               AND t.deadline_boot_ms>?4 AND (a.lease IS NULL OR a.time_ns=?5)
               AND (i.lease IS NULL OR (i.status='Pending' AND i.time_ns=?5)))",
            params![
                session.id,
                session.body["current_lease"].as_str(),
                self.boot.boot_id,
                self.boot.millis,
                self.time_ns,
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
    clock: Option<&Clock>,
    model: Option<&str>,
    grant: Option<&str>,
    provider: Option<&str>,
) -> Result<u32> {
    Ok(db.c.query_row(
        "SELECT count(*) FROM session_turns st JOIN current s ON s.id=st.session
         JOIN current d ON d.id=s.spec->>'delegation_id' JOIN current m ON m.id=s.spec->'model_ref'->>'resource_id'
         LEFT JOIN model_lease_times a ON a.lease=st.lease
         LEFT JOIN model_inference_leases i ON i.lease=st.lease
         LEFT JOIN model_provider_authorities p ON p.lease=st.lease
         WHERE st.receipt IS NULL AND (m.id=?1 OR d.body->>'grant_id'=?2 OR p.root_grant=?2
               OR (p.lease IS NULL AND m.spec->>'provider_principal_id'=?3))
         AND i.status IS NOT 'NotStarted'
         AND CASE WHEN ?4 IS NULL THEN 1 WHEN st.boot_id=?4 AND (a.lease IS NULL OR a.time_ns=?6)
                  AND (i.lease IS NULL OR i.time_ns=?6) THEN st.deadline_boot_ms>?5
                  ELSE julianday(st.expires)>julianday('now') END",
        params![model, grant, provider, clock.map(|c| &c.boot.boot_id), clock.map(|c| c.boot.millis), clock.map(|c| &c.time_ns)],
        |r| r.get(0),
    )?)
}

pub(super) fn model_slots(db: &Db<'_>, clock: &Clock, model: &str) -> Result<u32> {
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
        Clock::sample().ok().as_ref(),
        None,
        Some(&grant.id),
        provider,
    )? > 0)
}
