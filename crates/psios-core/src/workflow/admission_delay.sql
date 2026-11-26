CREATE TABLE workflow_admission_deferrals (
    run TEXT PRIMARY KEY REFERENCES active_workflows(run) ON DELETE CASCADE,
    run_rev INTEGER NOT NULL CHECK(run_rev > 0),
    step TEXT NOT NULL CHECK(length(step) BETWEEN 1 AND 200),
    boot_id TEXT NOT NULL,
    time_ns TEXT NOT NULL,
    deferred_boot_ms INTEGER NOT NULL CHECK(deferred_boot_ms >= 0)
) STRICT, WITHOUT ROWID;
