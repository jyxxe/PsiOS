CREATE TABLE application_image_archive_pins (
    artifact TEXT NOT NULL REFERENCES oci_image_captures(artifact),
    holder TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('Task','Release')),
    PRIMARY KEY(kind,holder)
) STRICT, WITHOUT ROWID;

CREATE INDEX application_image_archive_pins_by_artifact ON application_image_archive_pins(artifact);
