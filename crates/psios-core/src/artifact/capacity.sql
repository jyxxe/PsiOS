CREATE TABLE read_result_charges(
    resource TEXT PRIMARY KEY,
    owner TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes > 0),
    kind TEXT NOT NULL CHECK(kind IN ('Reserved','Limited','Retained'))
) STRICT, WITHOUT ROWID;

CREATE TABLE read_result_usage(
    owner TEXT PRIMARY KEY,
    reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes >= 0),
    retained_bytes INTEGER NOT NULL CHECK(retained_bytes >= 0)
) STRICT, WITHOUT ROWID;

CREATE TRIGGER account_read_result_charge AFTER INSERT ON read_result_charges
BEGIN
    INSERT INTO read_result_usage(owner, reserved_bytes, retained_bytes)
    VALUES(NEW.owner, NEW.bytes * (NEW.kind<>'Retained'), NEW.bytes * (NEW.kind='Retained'))
    ON CONFLICT(owner) DO UPDATE SET
        reserved_bytes=reserved_bytes+excluded.reserved_bytes,
        retained_bytes=retained_bytes+excluded.retained_bytes;
END;

CREATE TRIGGER release_read_result_charge AFTER DELETE ON read_result_charges
BEGIN
    UPDATE read_result_usage SET reserved_bytes=reserved_bytes-OLD.bytes*(OLD.kind<>'Retained'),
        retained_bytes=retained_bytes-OLD.bytes*(OLD.kind='Retained')
    WHERE owner=OLD.owner;
    DELETE FROM read_result_usage WHERE owner=OLD.owner AND reserved_bytes=0 AND retained_bytes=0;
END;
