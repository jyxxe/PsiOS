CREATE UNIQUE INDEX application_installation_by_release ON versions((spec->>'release_id'))
    WHERE kind='ApplicationArtifactInstallation' AND rev=1;
CREATE INDEX deployment_release_lookup ON versions((spec->>'application_id'), (spec->'release_ref'->>'resource_id'))
    WHERE kind='Deployment';
