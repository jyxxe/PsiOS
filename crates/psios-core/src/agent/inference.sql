CREATE TABLE model_inference_leases(
 lease TEXT PRIMARY KEY REFERENCES session_turns(lease),
 session_revision INTEGER NOT NULL CHECK(session_revision > 0),
 model TEXT NOT NULL,
 boot_id TEXT NOT NULL,
 time_ns TEXT NOT NULL,
 deadline_boot_ms INTEGER NOT NULL,
 status TEXT NOT NULL DEFAULT 'Pending' CHECK(status IN ('Pending','Responded','NotStarted','Unknown')),
 source TEXT CHECK(source IN ('Provider','LeaseExpired','SessionCancelled')),
 receipt TEXT,
 CHECK((status IN ('Pending','Responded') AND source IS NULL AND receipt IS NULL)
    OR (status='NotStarted' AND source IS 'Provider' AND receipt IS NOT NULL)
    OR (status='Unknown' AND source IS NOT NULL AND receipt IS NOT NULL))
) STRICT;
CREATE INDEX model_inference_pending ON model_inference_leases(deadline_boot_ms,lease) WHERE status='Pending';
CREATE INDEX model_inference_pending_by_model ON model_inference_leases(model,deadline_boot_ms,lease) WHERE status='Pending';
