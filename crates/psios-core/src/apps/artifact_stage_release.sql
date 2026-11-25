DROP INDEX application_installation_by_release;
CREATE UNIQUE INDEX application_artifact_evidence_by_release ON versions((spec->>'release_id'))
    WHERE rev=1 AND kind IN ('ApplicationArtifactInstallation','ApplicationArtifactReleaseStage');
