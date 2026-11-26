CREATE TABLE artifact_upload_authority (
    stage_id TEXT PRIMARY KEY REFERENCES artifact_uploads(id),
    delegation_ref TEXT NOT NULL CHECK(json_valid(delegation_ref) AND json_type(delegation_ref)='object'),
    ttl_seconds INTEGER NOT NULL CHECK(ttl_seconds BETWEEN 5 AND 3600),
    close_reason TEXT CHECK(close_reason='AuthorityRevoked')
) STRICT, WITHOUT ROWID;
