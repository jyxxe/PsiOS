ALTER TABLE model_budgets ADD COLUMN maximum_lease_ms INTEGER
    CHECK(maximum_lease_ms IS NULL OR maximum_lease_ms BETWEEN 0 AND 2592000000);
ALTER TABLE model_budgets ADD COLUMN charged_lease_ms INTEGER NOT NULL DEFAULT 0
    CHECK(charged_lease_ms BETWEEN 0 AND coalesce(maximum_lease_ms,0));
CREATE TABLE model_lease_times(
    lease TEXT PRIMARY KEY REFERENCES session_turns(lease),
    budget TEXT NOT NULL REFERENCES model_budgets(budget),
    time_ns TEXT NOT NULL,
    reserved_ms INTEGER NOT NULL CHECK(reserved_ms > 0),
    settled_ms INTEGER CHECK(settled_ms BETWEEN 1 AND reserved_ms)
) STRICT;
CREATE INDEX model_time_pending ON model_lease_times(budget,lease) WHERE settled_ms IS NULL;
