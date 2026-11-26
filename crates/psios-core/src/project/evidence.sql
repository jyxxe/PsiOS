CREATE INDEX work_item_evidence_by_artifact ON versions((body->'artifact_ref'->>'resource_id'))
    WHERE kind='WorkItemEvidence';
