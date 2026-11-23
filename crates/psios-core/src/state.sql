CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL) STRICT;
CREATE TABLE versions(
 seq INTEGER PRIMARY KEY, id TEXT NOT NULL, rev INTEGER NOT NULL, gen INTEGER NOT NULL,
 kind TEXT NOT NULL, owner TEXT NOT NULL, spec TEXT NOT NULL, body TEXT NOT NULL,
 created TEXT NOT NULL, stamp TEXT NOT NULL, event_id TEXT NOT NULL UNIQUE,
 correlation TEXT NOT NULL, incarnation TEXT NOT NULL, UNIQUE(id,rev)
) STRICT;
CREATE INDEX versions_list ON versions(kind,owner,id,seq);
CREATE VIEW current AS SELECT v.* FROM versions v WHERE NOT EXISTS
 (SELECT 1 FROM versions newer WHERE newer.id=v.id AND newer.rev>v.rev);
CREATE TABLE credentials(digest TEXT PRIMARY KEY,principal TEXT NOT NULL UNIQUE) STRICT;
CREATE TABLE secrets(id TEXT PRIMARY KEY,nonce BLOB NOT NULL,ciphertext BLOB NOT NULL) STRICT;
CREATE TABLE budgets(id TEXT PRIMARY KEY,maximum INTEGER NOT NULL,reserved INTEGER NOT NULL DEFAULT 0,
 spent INTEGER NOT NULL DEFAULT 0,CHECK(reserved>=0 AND spent>=0 AND reserved+spent<=maximum)) STRICT;
CREATE TABLE submissions(owner TEXT NOT NULL,origin TEXT NOT NULL,key TEXT NOT NULL,digest TEXT NOT NULL,
 scope TEXT NOT NULL,task TEXT UNIQUE,receipt TEXT, cancelled INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(owner,origin,key)) STRICT;
CREATE TABLE queue(task TEXT PRIMARY KEY, stamp INTEGER NOT NULL, phase TEXT NOT NULL,
 budget TEXT NOT NULL,attempt INTEGER NOT NULL DEFAULT 0,operation TEXT,grant_id TEXT NOT NULL,valid_until TEXT NOT NULL) STRICT;
CREATE INDEX queue_ready ON queue(phase,stamp);
CREATE TABLE approval_bindings(request TEXT PRIMARY KEY,task TEXT NOT NULL UNIQUE,decision TEXT UNIQUE,operation TEXT UNIQUE) STRICT;
CREATE TABLE holds(volume TEXT PRIMARY KEY, holder TEXT NOT NULL) STRICT;
CREATE TABLE event_offsets(owner TEXT NOT NULL,consumer TEXT NOT NULL,cursor TEXT NOT NULL,PRIMARY KEY(owner,consumer)) STRICT;
CREATE TABLE audit(seq INTEGER PRIMARY KEY,at TEXT NOT NULL,actor TEXT NOT NULL,method TEXT NOT NULL,
 resource TEXT,decision TEXT NOT NULL,request TEXT NOT NULL) STRICT;
PRAGMA user_version=1;
