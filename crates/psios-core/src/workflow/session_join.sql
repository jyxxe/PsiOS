CREATE TABLE workflow_session_joins(
    launch_task TEXT PRIMARY KEY,
    run TEXT NOT NULL,
    step TEXT NOT NULL,
    session TEXT NOT NULL UNIQUE,
    opened_ref TEXT NOT NULL CHECK(json_valid(opened_ref)),
    UNIQUE(run,step)
) STRICT;
ALTER TABLE active_workflow_tasks ADD COLUMN session_rev INTEGER CHECK(session_rev > 0);
