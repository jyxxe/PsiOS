CREATE TABLE artifact_chunks(
 artifact_id TEXT NOT NULL,
 artifact_rev INTEGER NOT NULL,
 chunk_index INTEGER NOT NULL CHECK(chunk_index>=0),
 payload BLOB NOT NULL CHECK(length(payload) BETWEEN 1 AND 131072),
 PRIMARY KEY(artifact_id,artifact_rev,chunk_index),
 FOREIGN KEY(artifact_id,artifact_rev) REFERENCES versions(id,rev)
) STRICT, WITHOUT ROWID;
