ALTER TABLE artifact_uploads RENAME COLUMN source_kind TO prior_source_kind;
ALTER TABLE artifact_uploads ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'LocalSubmission'
    CHECK(source_kind IN ('LocalSubmission','LocalSessionOutput','LocalOciImageCapture'));
UPDATE artifact_uploads SET source_kind=prior_source_kind WHERE prior_source_kind!='LocalSubmission';
ALTER TABLE artifact_uploads DROP COLUMN prior_source_kind;

CREATE TABLE oci_image_captures (
    artifact TEXT PRIMARY KEY REFERENCES artifact_uploads(artifact_id),
    proof TEXT NOT NULL UNIQUE
) STRICT, WITHOUT ROWID;
