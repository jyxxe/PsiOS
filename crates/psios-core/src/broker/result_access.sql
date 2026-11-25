ALTER TABLE task_dependencies ADD COLUMN read_delegation TEXT;
CREATE INDEX task_dependency_reads ON task_dependencies(read_delegation, task)
    WHERE read_delegation IS NOT NULL;
