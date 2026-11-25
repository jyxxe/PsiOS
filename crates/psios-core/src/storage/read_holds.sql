CREATE TABLE volume_read_holds(
    volume TEXT NOT NULL,
    task TEXT NOT NULL REFERENCES queue(task) ON DELETE CASCADE,
    PRIMARY KEY(volume, task)
) STRICT, WITHOUT ROWID;
CREATE INDEX volume_read_holds_by_task ON volume_read_holds(task);
