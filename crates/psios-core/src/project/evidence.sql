CREATE INDEX work_item_evidence_by_artifact ON versions((body->'artifact_ref'->>'resource_id'))
    WHERE kind='WorkItemEvidence';
CREATE INDEX work_item_evidence_by_item ON versions((body->>'work_item_id'))
    WHERE kind='WorkItemEvidence';
