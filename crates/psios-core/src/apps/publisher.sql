CREATE UNIQUE INDEX oci_release_publisher_proof
    ON versions((body->'provenance'->'trust_evidence_or_admission_policy_ref'->>'resource_id'))
    WHERE kind='ApplicationReleaseSpec' AND rev=1 AND body->'provenance'->>'source_trust'='PublisherVerified';
