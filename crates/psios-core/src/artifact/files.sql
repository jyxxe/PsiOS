CREATE TABLE oci_archive_blobs (
    artifact TEXT PRIMARY KEY REFERENCES artifact_uploads(artifact_id),
    disposed INTEGER NOT NULL DEFAULT 0 CHECK(disposed IN (0,1))
) STRICT, WITHOUT ROWID;
