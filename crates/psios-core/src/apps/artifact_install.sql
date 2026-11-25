CREATE TABLE application_artifact_pins (
    artifact TEXT NOT NULL REFERENCES artifact_uploads(artifact_id),
    holder TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('Task','Release')),
    PRIMARY KEY(kind,holder)
) STRICT, WITHOUT ROWID;

CREATE INDEX application_artifact_pins_by_artifact ON application_artifact_pins(artifact);
