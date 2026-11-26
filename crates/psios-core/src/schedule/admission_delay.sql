CREATE TABLE schedule_admission_deferrals (
    schedule TEXT PRIMARY KEY,
    schedule_rev INTEGER NOT NULL CHECK(schedule_rev > 0),
    kind TEXT NOT NULL CHECK(kind IN ('Timed','ResourceEvent')),
    anchor INTEGER NOT NULL,
    trigger TEXT NOT NULL,
    ready_order INTEGER NOT NULL CHECK(ready_order > anchor),
    boot_id TEXT NOT NULL,
    time_ns TEXT NOT NULL,
    deferred_boot_ms INTEGER NOT NULL CHECK(deferred_boot_ms >= 0),
    FOREIGN KEY(schedule,schedule_rev) REFERENCES versions(id,rev)
) STRICT, WITHOUT ROWID;
CREATE INDEX schedule_admission_order ON schedule_admission_deferrals(kind,ready_order);
