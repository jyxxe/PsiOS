CREATE INDEX versions_event_id ON versions(id, seq);
CREATE INDEX versions_event_kind ON versions(kind, seq);
CREATE INDEX versions_event_owner ON versions(owner, seq);
CREATE INDEX versions_event_owner_kind ON versions(owner, kind, seq);
