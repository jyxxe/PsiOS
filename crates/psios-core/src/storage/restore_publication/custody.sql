CREATE TABLE restore_stage_disposals (
    operation TEXT PRIMARY KEY, checkpoint_ref TEXT NOT NULL, evidence_ref TEXT NOT NULL
) STRICT;
CREATE INDEX restore_not_applied_by_seq ON versions(seq)
    WHERE kind='RecoveryEvidence' AND body->'evidence'->>'conclusion'='RestorePublicationNotApplied';
CREATE VIEW restore_not_applied AS SELECT * FROM versions INDEXED BY restore_not_applied_by_seq
    WHERE kind='RecoveryEvidence' AND body->'evidence'->>'conclusion'='RestorePublicationNotApplied';
CREATE UNIQUE INDEX restore_stage_reclaim_by_operation ON audit(resource)
    WHERE method='Storage.RestoreStageReclaim';
INSERT INTO meta(key,value)
    SELECT 'restore_custody_epoch',CAST(COALESCE(MAX(seq),0) AS TEXT) FROM versions;
