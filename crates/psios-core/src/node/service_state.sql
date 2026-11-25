CREATE TABLE service_observation_positions(
    service TEXT PRIMARY KEY,
    dispatched_seq INTEGER NOT NULL CHECK(dispatched_seq > 0)
) STRICT;
