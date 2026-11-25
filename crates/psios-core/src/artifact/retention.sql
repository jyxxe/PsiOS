CREATE INDEX recovery_observation_artifacts
 ON versions(json_extract(body,'$.evidence.observation_ref.resource_id'))
 WHERE kind='RecoveryEvidence';
ALTER TABLE active_workflows ADD COLUMN result_blocked INTEGER NOT NULL DEFAULT 0
 CHECK(result_blocked IN (0,1));
