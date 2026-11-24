CREATE TABLE direct_chats(
 id TEXT PRIMARY KEY, inviter TEXT NOT NULL, invitee TEXT NOT NULL, name TEXT NOT NULL,
 phase TEXT NOT NULL DEFAULT 'Invited' CHECK(phase IN('Invited','Active','Declined','Archived')),
 revision INTEGER NOT NULL DEFAULT 1, last_seq INTEGER NOT NULL DEFAULT 0,
 invitation_until INTEGER NOT NULL, created_at TEXT NOT NULL, creation_digest TEXT NOT NULL,
 CHECK(inviter!=invitee)
) STRICT;
CREATE INDEX chat_inviter ON direct_chats(inviter,id);
CREATE INDEX chat_invitee ON direct_chats(invitee,id);
CREATE TABLE chat_send_windows(
 conversation TEXT NOT NULL REFERENCES direct_chats(id), sender TEXT NOT NULL,
 namespace TEXT NOT NULL, incarnation TEXT NOT NULL, valid_until INTEGER NOT NULL,
 sender_read_delegation TEXT NOT NULL, sender_send_delegation TEXT NOT NULL, peer_read_delegation TEXT NOT NULL,
 PRIMARY KEY(conversation,sender)
) STRICT;
CREATE TABLE chat_messages(
 conversation TEXT NOT NULL REFERENCES direct_chats(id), seq INTEGER NOT NULL, id TEXT NOT NULL UNIQUE,
 sender TEXT NOT NULL, send_key TEXT NOT NULL, receipt TEXT NOT NULL, content TEXT NOT NULL,
 reply_to TEXT, PRIMARY KEY(conversation,seq), UNIQUE(conversation,sender,send_key)
) STRICT;
CREATE TABLE chat_read_offsets(
 conversation TEXT NOT NULL REFERENCES direct_chats(id), principal TEXT NOT NULL, seq INTEGER NOT NULL,
 PRIMARY KEY(conversation,principal)
) STRICT;
