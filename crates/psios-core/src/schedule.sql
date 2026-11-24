CREATE TABLE pending_schedules(
 schedule TEXT PRIMARY KEY, run_ms INTEGER NOT NULL, deadline_ms INTEGER NOT NULL CHECK(deadline_ms>=run_ms)
) STRICT;
CREATE INDEX schedule_run ON pending_schedules(run_ms,schedule);
CREATE INDEX schedule_deadline ON pending_schedules(deadline_ms,schedule);
