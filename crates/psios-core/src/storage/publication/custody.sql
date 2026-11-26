CREATE TABLE backup_stage_custody (
    operation TEXT PRIMARY KEY NOT NULL,
    stage_name TEXT NOT NULL UNIQUE,
    checkpoint_ref TEXT NOT NULL,
    evidence_ref TEXT
) STRICT;
CREATE INDEX backup_not_applied_by_seq ON versions(seq)
    WHERE kind='RecoveryEvidence' AND body->'evidence'->>'conclusion'='BackupPublicationNotApplied';
CREATE VIEW backup_not_applied AS SELECT * FROM versions INDEXED BY backup_not_applied_by_seq
    WHERE kind='RecoveryEvidence' AND body->'evidence'->>'conclusion'='BackupPublicationNotApplied';
CREATE UNIQUE INDEX backup_stage_reclaim_by_operation ON audit(resource)
    WHERE method='Storage.BackupStageReclaim';
INSERT INTO meta(key,value)
    SELECT 'backup_custody_epoch',CAST(COALESCE(MAX(seq),0) AS TEXT) FROM versions;
