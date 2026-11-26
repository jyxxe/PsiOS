CREATE TABLE task_prepared_inputs(
 id TEXT PRIMARY KEY,
 owner TEXT NOT NULL,
 kind TEXT NOT NULL CHECK(kind IN ('Input','TaskInputManifest')),
 body TEXT NOT NULL,
 bytes INTEGER NOT NULL CHECK(bytes = length(CAST(body AS BLOB)) AND bytes BETWEEN 1 AND 524288),
 created_at TEXT NOT NULL,
 UNIQUE(owner,id)
) STRICT, WITHOUT ROWID;
CREATE TABLE task_preparations(
 owner TEXT NOT NULL,
 origin TEXT NOT NULL,
 key TEXT NOT NULL,
 request TEXT NOT NULL CHECK(length(CAST(request AS BLOB)) <= 8192),
 input_id TEXT NOT NULL,
 incarnation TEXT NOT NULL,
 boot_id TEXT NOT NULL,
 time_ns TEXT NOT NULL,
 issued_boot_ms INTEGER NOT NULL CHECK(issued_boot_ms >= 0),
 deadline_boot_ms INTEGER NOT NULL CHECK(deadline_boot_ms > issued_boot_ms AND deadline_boot_ms - issued_boot_ms <= 600000),
 issued_utc_ms INTEGER NOT NULL,
 expires_utc_ms INTEGER NOT NULL CHECK(expires_utc_ms > issued_utc_ms AND expires_utc_ms - issued_utc_ms <= 600000),
 PRIMARY KEY(owner,origin,key),
 FOREIGN KEY(owner,input_id) REFERENCES task_prepared_inputs(owner,id)
) STRICT, WITHOUT ROWID;
CREATE INDEX task_preparation_content ON task_preparations(owner,input_id);
CREATE INDEX task_preparation_expiry ON task_preparations(expires_utc_ms,owner,origin,key);
