CREATE TABLE inherited_session_results(
    session TEXT PRIMARY KEY,
    task TEXT NOT NULL
) STRICT;
CREATE INDEX inherited_result_consumers ON inherited_session_results(task,session);
CREATE TRIGGER release_inherited_session_result AFTER INSERT ON versions
WHEN NEW.kind='Session'
BEGIN
    DELETE FROM inherited_session_results WHERE session=NEW.id
       AND (NEW.body->>'lifecycle' IN ('Failed','Succeeded','Cancelled') OR NEW.body->>'last_task' IS NOT task);
END;
