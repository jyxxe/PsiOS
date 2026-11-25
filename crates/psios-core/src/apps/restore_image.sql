CREATE TABLE oci_image_restore_holds (
    engine_id TEXT NOT NULL,
    image_id TEXT NOT NULL,
    operation TEXT NOT NULL UNIQUE,
    PRIMARY KEY(engine_id,image_id)
) STRICT, WITHOUT ROWID;
