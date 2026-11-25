CREATE TABLE pending_event_schedules(
 schedule TEXT PRIMARY KEY, source TEXT NOT NULL,
 source_rev INTEGER NOT NULL CHECK(source_rev>0), deadline_ms INTEGER NOT NULL
) STRICT;
CREATE INDEX event_schedule_deadlines ON pending_event_schedules(deadline_ms,schedule);
