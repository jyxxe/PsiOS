ALTER TABLE oci_image_captures ADD COLUMN restore_status TEXT NOT NULL DEFAULT 'Unverified'
    CHECK (restore_status IN ('Unverified','Verified','Unsupported'));
