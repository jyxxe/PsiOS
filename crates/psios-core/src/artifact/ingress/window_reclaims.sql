ALTER TABLE artifact_stage_windows ADD COLUMN reclaimable
    INTEGER NOT NULL DEFAULT 0 CHECK(reclaimable IN (0,1));

UPDATE artifact_stage_windows SET reclaimable=1
    WHERE EXISTS(SELECT 1 FROM artifact_uploads u WHERE u.id=stage_id
                 AND u.state IN ('Discarded','Expired'));

CREATE INDEX artifact_stage_ready_for_reclaim
    ON artifact_stage_windows(deadline_boot_ms,stage_id)
    WHERE reclaimable=1;

CREATE TABLE reclaimed_artifact_uploads (
    artifact_id TEXT PRIMARY KEY,
    stage_id TEXT NOT NULL UNIQUE,
    release_digest TEXT NOT NULL CHECK(length(release_digest)=71)
) STRICT, WITHOUT ROWID;
