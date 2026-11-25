CREATE TABLE recovery_admissions (
    task TEXT PRIMARY KEY REFERENCES queue(task) ON DELETE CASCADE,
    operation TEXT NOT NULL UNIQUE,
    operation_revision INTEGER NOT NULL CHECK(operation_revision > 0)
) STRICT;
CREATE INDEX queue_admitted ON queue(task) WHERE phase <> 'Done';
