CREATE TABLE model_context_snapshots(
    lease TEXT PRIMARY KEY REFERENCES session_turns(lease),
    session_ref TEXT NOT NULL CHECK(json_valid(session_ref)),
    usage_ref TEXT NOT NULL CHECK(json_valid(usage_ref)),
    context_digest TEXT NOT NULL CHECK(length(context_digest)=71 AND substr(context_digest,1,7)='sha256:')
) STRICT, WITHOUT ROWID;
