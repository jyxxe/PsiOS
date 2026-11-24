CREATE TABLE IF NOT EXISTS authorization_deadlines (
    resource TEXT PRIMARY KEY,
    boot_id TEXT NOT NULL,
    time_ns TEXT NOT NULL,
    deadline_boot_ms INTEGER NOT NULL
) STRICT;
