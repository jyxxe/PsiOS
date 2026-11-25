CREATE TABLE task_concurrency (
    budget TEXT PRIMARY KEY REFERENCES budgets(id),
    maximum INTEGER NOT NULL CHECK(maximum BETWEEN 1 AND 16)
) STRICT;
CREATE INDEX queue_active_budget ON queue(budget,phase)
    WHERE phase IN('Claimed','Dispatched','Unknown');
CREATE VIEW occupied_task_slots AS
    SELECT budget, phase FROM queue WHERE phase IN('Claimed','Dispatched','Unknown');
