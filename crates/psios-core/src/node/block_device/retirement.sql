CREATE TABLE block_device_watch (
    origin_seq INTEGER PRIMARY KEY REFERENCES versions(seq),
    device_id TEXT NOT NULL UNIQUE
) STRICT;

INSERT INTO block_device_watch(origin_seq, device_id)
SELECT origin.seq, origin.id FROM versions AS origin INDEXED BY versions_origin_kind
WHERE origin.kind='BlockDevice' AND origin.rev=1
  AND (SELECT latest.body->>'lifecycle' FROM versions AS latest
       WHERE latest.id=origin.id ORDER BY latest.rev DESC LIMIT 1) IN ('Present','Absent');

CREATE TRIGGER block_device_watch_revision AFTER INSERT ON versions
WHEN NEW.kind='BlockDevice' BEGIN
    INSERT INTO block_device_watch(origin_seq, device_id)
    SELECT NEW.seq, NEW.id WHERE NEW.rev=1 AND NEW.body->>'lifecycle'='Present';
    DELETE FROM block_device_watch
    WHERE device_id=NEW.id AND NEW.body->>'lifecycle' IN ('Replaced','BootEnded','Retired');
END;
