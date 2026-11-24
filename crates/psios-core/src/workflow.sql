CREATE TABLE active_workflows(
 run TEXT PRIMARY KEY, deadline_ms INTEGER NOT NULL, ready_ms INTEGER NOT NULL,
 task TEXT UNIQUE, task_rev INTEGER NOT NULL DEFAULT 0,
 stopping INTEGER NOT NULL DEFAULT 0 CHECK(stopping IN(0,1)),
 CHECK((task IS NULL AND task_rev=0) OR (task IS NOT NULL AND task_rev>0))
) STRICT;
CREATE INDEX workflow_pending_order ON active_workflows(ready_ms,run);
