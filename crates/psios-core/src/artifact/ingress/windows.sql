CREATE TABLE artifact_stage_windows (
    stage_id TEXT PRIMARY KEY REFERENCES artifact_uploads(id),
    clock_domain BLOB NOT NULL CHECK(length(clock_domain)=16),
    deadline_boot_ms INTEGER NOT NULL CHECK(deadline_boot_ms>0)
) STRICT, WITHOUT ROWID;

CREATE INDEX artifact_stage_window_deadline
    ON artifact_stage_windows(clock_domain,deadline_boot_ms,stage_id);
