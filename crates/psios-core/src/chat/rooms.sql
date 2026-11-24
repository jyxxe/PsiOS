CREATE TABLE chat_logs(
 id TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0 CHECK(last_seq>=0),
 writer_epoch INTEGER NOT NULL DEFAULT 1 CHECK(writer_epoch>0)
) STRICT;
INSERT INTO chat_logs(id,last_seq) SELECT id,last_seq FROM direct_chats;

ALTER TABLE chat_messages RENAME TO v19_chat_messages;
CREATE TABLE chat_messages(
 conversation TEXT NOT NULL REFERENCES chat_logs(id), seq INTEGER NOT NULL, id TEXT NOT NULL UNIQUE,
 sender TEXT NOT NULL, send_key TEXT NOT NULL, receipt TEXT NOT NULL, content TEXT NOT NULL,
 reply_to TEXT, PRIMARY KEY(conversation,seq), UNIQUE(conversation,sender,send_key)
) STRICT;
INSERT INTO chat_messages SELECT * FROM v19_chat_messages;
DROP TABLE v19_chat_messages;

ALTER TABLE chat_send_windows RENAME TO v19_chat_send_windows;
CREATE TABLE chat_send_windows(
 conversation TEXT NOT NULL REFERENCES chat_logs(id), sender TEXT NOT NULL, membership TEXT NOT NULL,
 namespace TEXT NOT NULL, incarnation TEXT NOT NULL, valid_until INTEGER NOT NULL,
 sender_read_delegation TEXT NOT NULL, sender_send_delegation TEXT NOT NULL, peer_read_delegation TEXT,
 PRIMARY KEY(conversation,sender)
) STRICT;
INSERT INTO chat_send_windows
 SELECT conversation,sender,'',namespace,incarnation,valid_until,sender_read_delegation,sender_send_delegation,
        peer_read_delegation FROM v19_chat_send_windows;
DROP TABLE v19_chat_send_windows;

ALTER TABLE chat_read_offsets RENAME TO v19_chat_read_offsets;
CREATE TABLE chat_read_offsets(
 conversation TEXT NOT NULL REFERENCES chat_logs(id), principal TEXT NOT NULL, membership TEXT NOT NULL,
 seq INTEGER NOT NULL, PRIMARY KEY(conversation,principal,membership)
) STRICT;
INSERT INTO chat_read_offsets SELECT conversation,principal,'',seq FROM v19_chat_read_offsets;
DROP TABLE v19_chat_read_offsets;
ALTER TABLE direct_chats DROP COLUMN last_seq;

CREATE TABLE chat_rooms(
 id TEXT PRIMARY KEY REFERENCES chat_logs(id), owner TEXT NOT NULL, name TEXT NOT NULL,
 phase TEXT NOT NULL DEFAULT 'Active' CHECK(phase IN('Active','Archived')),
 revision INTEGER NOT NULL DEFAULT 1, membership_revision INTEGER NOT NULL DEFAULT 1,
 capacity INTEGER NOT NULL CHECK(capacity BETWEEN 2 AND 16), expires INTEGER NOT NULL,
 created_at TEXT NOT NULL, creation_digest TEXT NOT NULL
) STRICT;
CREATE TABLE room_memberships(
 id TEXT PRIMARY KEY, conversation TEXT NOT NULL REFERENCES chat_rooms(id), principal TEXT NOT NULL,
 role TEXT NOT NULL CHECK(role IN('Owner','Member')),
 phase TEXT NOT NULL CHECK(phase IN('Invited','Active','Left','Removed','Expired')),
 invitation_until INTEGER, read_after INTEGER NOT NULL DEFAULT 0, reason TEXT, created_at TEXT NOT NULL,
 CHECK(phase!='Invited' OR invitation_until IS NOT NULL), CHECK(read_after>=0)
) STRICT;
CREATE UNIQUE INDEX room_pending_or_active ON room_memberships(conversation,principal)
 WHERE phase IN('Invited','Active');
CREATE INDEX room_member_rooms ON room_memberships(principal,conversation);
