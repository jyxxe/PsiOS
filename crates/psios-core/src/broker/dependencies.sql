CREATE TABLE task_dependencies (
    task TEXT NOT NULL REFERENCES queue(task) ON DELETE CASCADE,
    producer TEXT NOT NULL,
    PRIMARY KEY(task, producer)
) STRICT;
CREATE INDEX task_dependencies_producer ON task_dependencies(producer, task);
