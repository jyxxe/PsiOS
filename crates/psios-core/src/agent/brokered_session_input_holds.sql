CREATE TABLE brokered_session_input_holds(
    task TEXT NOT NULL,
    artifact TEXT NOT NULL,
    PRIMARY KEY(task,artifact)
) STRICT, WITHOUT ROWID;
CREATE INDEX brokered_session_input_by_artifact ON brokered_session_input_holds(artifact,task);
