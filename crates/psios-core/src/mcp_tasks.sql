CREATE TABLE mcp_task_handles (
    handle TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    submission_key TEXT NOT NULL,
    recovery_incarnation TEXT NOT NULL,
    boot_id TEXT NOT NULL,
    time_ns TEXT NOT NULL,
    created_boot_ms INTEGER NOT NULL,
    created_utc_ms INTEGER NOT NULL,
    ttl_ms INTEGER NOT NULL CHECK(ttl_ms BETWEEN 1000 AND 600000),
    spec TEXT NOT NULL CHECK(json_valid(spec)),
    UNIQUE(owner, endpoint, submission_key)
) STRICT;
CREATE INDEX mcp_task_handles_owner ON mcp_task_handles(owner);
CREATE UNIQUE INDEX mcp_task_handles_task ON mcp_task_handles(json_extract(spec,'$.task_ref.resource_id'));
