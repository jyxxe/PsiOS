ALTER TABLE artifact_uploads ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'LocalSubmission'
    CHECK(source_kind IN ('LocalSubmission','LocalSessionOutput'));
