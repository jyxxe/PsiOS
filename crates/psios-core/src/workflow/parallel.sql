DROP INDEX workflow_pending_order;
ALTER TABLE active_workflows RENAME TO serial_workflow_runs;
CREATE TABLE active_workflows(
    run TEXT PRIMARY KEY,
    deadline_ms INTEGER NOT NULL,
    ready_ms INTEGER,
    stopping INTEGER NOT NULL DEFAULT 0 CHECK(stopping IN (0,1)),
    result_blocked INTEGER NOT NULL DEFAULT 0 CHECK(result_blocked IN (0,1)),
    max_parallel_steps INTEGER NOT NULL DEFAULT 1 CHECK(max_parallel_steps BETWEEN 1 AND 16)
) STRICT;
CREATE INDEX workflow_pending_order ON active_workflows(ready_ms,run) WHERE ready_ms IS NOT NULL;
CREATE TABLE active_workflow_tasks(
    run TEXT NOT NULL REFERENCES active_workflows(run) ON DELETE CASCADE,
    step TEXT NOT NULL,
    task TEXT NOT NULL UNIQUE,
    task_rev INTEGER NOT NULL CHECK(task_rev > 0),
    PRIMARY KEY(run,step)
) STRICT;
INSERT INTO active_workflows(run,deadline_ms,ready_ms,stopping,result_blocked)
SELECT run,deadline_ms,
       CASE WHEN task IS NULL AND stopping=0 AND result_blocked=0 THEN ready_ms END,
       stopping,result_blocked
FROM serial_workflow_runs;
INSERT INTO active_workflow_tasks(run,step,task,task_rev)
SELECT old.run, (SELECT current.body->>'active_step' FROM current
                 WHERE current.id=old.run AND current.kind='WorkflowRun'), old.task,old.task_rev
FROM serial_workflow_runs old WHERE old.task IS NOT NULL;
DROP TABLE serial_workflow_runs;
