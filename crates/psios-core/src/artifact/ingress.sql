CREATE TABLE artifact_uploads (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    request_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    digest TEXT NOT NULL,
    declared_media_type TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
    received_bytes INTEGER NOT NULL DEFAULT 0 CHECK(received_bytes BETWEEN 0 AND size_bytes),
    expires_at TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'Open' CHECK(state IN ('Open','Expired','Discarded','Published','Withdrawn')),
    artifact_id TEXT UNIQUE,
    UNIQUE(owner,request_key),
    CHECK((state IN ('Published','Withdrawn')) = (artifact_id IS NOT NULL)),
    CHECK(artifact_id IS NULL OR received_bytes = size_bytes)
) STRICT;

CREATE INDEX artifact_open_uploads ON artifact_uploads(expires_at,id) WHERE state='Open';
CREATE INDEX artifact_active_uploads ON artifact_uploads(owner,state,size_bytes)
    WHERE state IN ('Open','Published');

CREATE TABLE artifact_upload_chunks (
    upload_id TEXT NOT NULL REFERENCES artifact_uploads(id),
    chunk_index INTEGER NOT NULL CHECK(chunk_index >= 0),
    digest TEXT NOT NULL,
    payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 131072),
    PRIMARY KEY(upload_id,chunk_index)
) STRICT, WITHOUT ROWID;
