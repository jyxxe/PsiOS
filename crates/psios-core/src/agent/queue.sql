ALTER TABLE session_turns ADD COLUMN queue_model TEXT;
CREATE UNIQUE INDEX model_claim_keys ON session_turns(queue_model, claim_key)
 WHERE queue_model IS NOT NULL;

-- A bounded scheduling index of the current Session revision. Sessions that
-- await a task or are paused retain an admission slot without being claimable.
CREATE TABLE model_queue(
 session TEXT PRIMARY KEY, model TEXT NOT NULL, ready_seq INTEGER, lease TEXT
) STRICT;
CREATE INDEX model_queue_order ON model_queue(model,ready_seq);
CREATE TRIGGER index_provider_session AFTER INSERT ON versions
 WHEN NEW.kind='Session' AND NEW.spec->>'provider_queue'=1
BEGIN
 DELETE FROM model_queue WHERE session=NEW.id;
 INSERT INTO model_queue(session,model,ready_seq,lease)
 SELECT NEW.id, NEW.spec->'model_ref'->>'resource_id',
        CASE WHEN NEW.body->>'lifecycle' IN ('AwaitingModel','ModelInFlight') THEN NEW.seq END,
        CASE WHEN NEW.body->>'lifecycle'='ModelInFlight' THEN NEW.body->>'current_lease' END
 WHERE NEW.body->>'lifecycle' NOT IN ('Succeeded','Failed','Cancelled');
END;
