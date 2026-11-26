CREATE TABLE session_input_holds(
    session TEXT NOT NULL,
    artifact TEXT NOT NULL,
    PRIMARY KEY(session,artifact)
) STRICT, WITHOUT ROWID;
CREATE INDEX session_input_by_artifact ON session_input_holds(artifact,session);
CREATE TRIGGER release_session_input_holds AFTER INSERT ON versions
WHEN NEW.kind='Session' AND NEW.body->>'lifecycle' IN ('Succeeded','Failed','Cancelled')
BEGIN
    DELETE FROM session_input_holds WHERE session=NEW.id;
END;
