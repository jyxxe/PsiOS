//! Local conversations using the Core identity and commit witness.
mod message;
mod room;

use crate::{ById, Conditional, Result, db::Db, decode, err, label, now, policy, require};
use chrono::{DateTime, SecondsFormat, Utc};
use psios_api::{canonical, sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub use message::{ack, get_message, list_messages, open_window, prepare, send};
pub use room::{invite, leave, open as open_room, remove, reopen, transfer_owner};
pub const INVITE: &str = "chat.invite";
pub const READ: &str = "chat.read";
pub const SEND: &str = "chat.send";
pub(crate) const CAPACITY_FORMAT: u32 = crate::apps::catalog::FORMAT + 1;
pub(crate) const CAPACITY_SCHEMA: &str = include_str!("chat/capacity.sql");

const SELECT: &str = "SELECT d.id,inviter,invitee,name,phase,revision,l.last_seq,invitation_until,created_at,
                     creation_digest,l.writer_epoch FROM direct_chats d JOIN chat_logs l ON l.id=d.id";

struct Chat {
    id: String,
    inviter: String,
    invitee: String,
    name: String,
    phase: String,
    revision: i64,
    last_seq: i64,
    invitation_until: i64,
    created_at: String,
    creation_digest: String,
    writer_epoch: i64,
}

impl Chat {
    fn peer(&self, actor: &str) -> &str {
        if actor == self.inviter {
            &self.invitee
        } else {
            &self.inviter
        }
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            inviter: row.get(1)?,
            invitee: row.get(2)?,
            name: row.get(3)?,
            phase: row.get(4)?,
            revision: row.get(5)?,
            last_seq: row.get(6)?,
            invitation_until: row.get(7)?,
            created_at: row.get(8)?,
            creation_digest: row.get(9)?,
            writer_epoch: row.get(10)?,
        })
    }

    fn invitee_state(&self) -> &str {
        match self.phase.as_str() {
            "Invited" if Utc::now().timestamp() >= self.invitation_until => "Expired",
            "Declined" => "Left",
            phase => phase,
        }
    }

    fn view(&self) -> Value {
        let peer = self.invitee_state();
        let reason = match peer {
            "Expired" => Some("InvitationExpired"),
            "Left" => Some("InvitationDeclined"),
            _ => None,
        };
        json!({"id": self.id, "kind": "DirectMessage", "name": self.name,
               "lifecycle": if matches!(self.phase.as_str(), "Archived" | "Declined") { "Archived" } else { "Active" },
               "resource_version": format!("r{}", self.revision),
               "membership_revision": self.membership_revision().to_string(), "created_at": self.created_at,
               "invitation_valid_until": timestamp(self.invitation_until),
               "members": [
                   {"principal_id": self.inviter, "role": "Owner", "lifecycle": "Active",
                    "membership_id": chat_id("membership", json!([self.id, self.inviter]))},
                   {"principal_id": self.invitee, "role": "Member",
                    "membership_id": chat_id("membership", json!([self.id, self.invitee])),
                    "reason": reason,
                    "lifecycle": if peer == "Archived" { "Active" } else { peer }}
               ],
               "history_policy": "FromJoin", "last_sequence": self.last_seq.to_string()})
    }

    fn membership_revision(&self) -> i64 {
        match self.phase.as_str() {
            "Active" | "Archived" | "Declined" => 2,
            _ => 1,
        }
    }

    fn cas(&self, expected: &str) -> Result<()> {
        require(
            expected == format!("r{}", self.revision),
            "Conflict",
            "conversation revision changed",
        )
    }
}

fn timestamp(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .expect("stored Unix timestamp in range")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn chat_id(prefix: &str, fields: Value) -> String {
    let digest = sha256(&canonical(&fields));
    format!("{prefix}-{}", &digest[7..])
}

fn find(db: &Db<'_>, id: &str) -> Result<Option<Chat>> {
    Ok(db
        .c
        .query_row(&format!("{SELECT} WHERE d.id=?1"), [id], Chat::from_row)
        .optional()?)
}

struct Access {
    id: String,
    writable: bool,
    last_seq: i64,
    writer_epoch: i64,
    membership_revision: i64,
    membership: String,
    read_after: i64,
    peer: Option<String>,
}

fn access(db: &Db<'_>, actor: &str, id: &str) -> Result<Access> {
    if room::is_room(id) {
        return room::access(db, actor, id);
    }
    let chat = active_member(db, actor, id)?;
    Ok(Access {
        peer: Some(chat.peer(actor).to_owned()),
        membership_revision: chat.membership_revision(),
        id: chat.id,
        writable: chat.phase == "Active",
        last_seq: chat.last_seq,
        writer_epoch: chat.writer_epoch,
        membership: String::new(),
        read_after: 0,
    })
}

fn conversation_capacity(db: &Db<'_>, stamp: i64) -> Result<()> {
    let total: u32 = db.c.query_row(
        "SELECT (SELECT count(*) FROM direct_chats WHERE phase='Active')
              + (SELECT count(*) FROM direct_chats WHERE phase='Invited' AND invitation_until>?1)
              + (SELECT count(*) FROM chat_rooms WHERE phase='Active' AND expires>?1)",
        [stamp],
        |row| row.get(0),
    )?;
    require(
        total < 512,
        "QueueFull",
        "node active conversation capacity reached",
    )
}

fn principal_capacity(db: &Db<'_>, actor: &str, current: Option<&str>, stamp: i64) -> Result<()> {
    let count: u32 = db.c.query_row(
        "SELECT (SELECT count(*) FROM direct_chats WHERE (inviter=?1 OR invitee=?1)
                   AND phase IN('Active','Invited') AND (phase='Active' OR invitation_until>?3))
              + (SELECT count(*) FROM room_memberships m JOIN chat_rooms r ON r.id=m.conversation
                 WHERE m.principal=?1 AND (?2 IS NULL OR r.id!=?2) AND r.phase='Active' AND r.expires>?3
                   AND m.phase IN('Active','Invited') AND (m.phase='Active' OR m.invitation_until>?3))",
        params![actor, current, stamp],
        |row| row.get(0),
    )?;
    require(
        count < 64,
        "QueueFull",
        "principal active conversation capacity reached",
    )
}

fn member(db: &Db<'_>, actor: &str, id: &str) -> Result<Chat> {
    policy::can_target(db, actor, READ, id)?;
    find(db, id)?
        .filter(|chat| chat.inviter == actor || chat.invitee == actor)
        .ok_or_else(|| {
            err(
                "PermissionDenied",
                "conversation unavailable to this principal",
            )
        })
}

fn active_member(db: &Db<'_>, actor: &str, id: &str) -> Result<Chat> {
    let chat = member(db, actor, id)?;
    require(
        matches!(chat.phase.as_str(), "Active" | "Archived"),
        "PermissionDenied",
        "membership is inactive",
    )?;
    Ok(chat)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    key: String,
    name: String,
    recipient_principal_id: String,
    ttl_seconds: u32,
}

pub fn open(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Open = decode(request)?;
    label(&p.key)?;
    label(&p.name)?;
    require(
        actor != p.recipient_principal_id && (1..=86400).contains(&p.ttl_seconds),
        "InvalidArgument",
        "a direct invitation requires another principal and a bounded lifetime",
    )?;
    let id = chat_id("direct", json!([db.meta("authority")?, actor, p.key]));
    policy::can_target(db, actor, INVITE, &id)?;
    policy::can_target(db, actor, READ, &id)?;
    let digest = sha256(&canonical(request));
    if let Some(chat) = find(db, &id)? {
        require(
            chat.creation_digest == digest,
            "IdempotencyConflict",
            "conversation key already bound",
        )?;
        return Ok(chat.view());
    }
    policy::active(db, &p.recipient_principal_id)?;
    db.cfg.check_admission_space()?;
    let stamp = Utc::now().timestamp();
    conversation_capacity(db, stamp)?;
    for person in [actor, &p.recipient_principal_id] {
        principal_capacity(db, person, None, stamp)?;
    }
    let until = stamp + i64::from(p.ttl_seconds);
    db.c.execute("INSERT INTO chat_logs(id) VALUES(?1)", [&id])?;
    db.c.execute(
        "INSERT INTO direct_chats(id,inviter,invitee,name,invitation_until,created_at,creation_digest)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![id, actor, p.recipient_principal_id, p.name, until, now(), digest],
    )?;
    Ok(member(db, actor, &id)?.view())
}

pub fn get(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: ById = decode(request)?;
    view(db, actor, &p.id)
}

fn view(db: &Db<'_>, actor: &str, id: &str) -> Result<Value> {
    if room::is_room(id) {
        return room::get(db, actor, id);
    }
    Ok(member(db, actor, id)?.view())
}

pub fn list(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct List {
        after_id: Option<String>,
        limit: Option<usize>,
    }
    let p: List = decode(request)?;
    policy::can(db, actor, READ)?;
    let limit = p.limit.unwrap_or(64);
    require(
        (1..=64).contains(&limit),
        "InvalidArgument",
        "invalid conversation page size",
    )?;
    if let Some(after) = &p.after_id {
        let digest = after
            .strip_prefix("direct-")
            .or_else(|| after.strip_prefix("room-"));
        require(
            digest.is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }),
            "InvalidArgument",
            "invalid conversation page position",
        )?;
    }
    let mut statement = db.c.prepare(
        "SELECT id FROM direct_chats WHERE (inviter=?1 OR invitee=?1) AND id>?2
          UNION ALL
         SELECT conversation FROM room_memberships
          WHERE principal=?1 AND phase IN('Invited','Active') AND conversation>?2
         ORDER BY id",
    )?;
    let mut items = Vec::<Value>::with_capacity(limit);
    let mut next_after_id = None;
    for row in statement.query_map(params![actor, p.after_id.as_deref().unwrap_or("")], |row| {
        row.get::<_, String>(0)
    })? {
        match view(db, actor, &row?) {
            Ok(_) if items.len() == limit => {
                next_after_id = items.last().map(|item| item["id"].clone());
                break;
            }
            Ok(item) => items.push(item),
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(json!({"items": items, "next_after_id":next_after_id}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Respond {
    id: String,
    expected_resource_version: String,
    accept: bool,
    membership_id: Option<String>,
}

pub fn respond(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Respond = decode(request)?;
    if room::is_room(&p.id) {
        return room::respond(db, actor, p);
    }
    require(
        p.membership_id.is_none(),
        "InvalidArgument",
        "direct conversations use a fixed invitation",
    )?;
    let chat = member(db, actor, &p.id)?;
    require(
        chat.invitee == actor,
        "PermissionDenied",
        "only the invited principal can respond",
    )?;
    chat.cas(&p.expected_resource_version)?;
    require(
        chat.invitee_state() == "Invited",
        "PreconditionFailed",
        "invitation is no longer active",
    )?;
    if p.accept {
        policy::active(db, &chat.inviter)?;
    }
    transition(
        db,
        actor,
        chat,
        if p.accept { "Active" } else { "Declined" },
    )
}

pub fn archive(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Conditional = decode(request)?;
    if room::is_room(&p.id) {
        return room::archive(db, actor, p);
    }
    let chat = active_member(db, actor, &p.id)?;
    require(
        chat.inviter == actor,
        "PermissionDenied",
        "only the conversation owner can archive",
    )?;
    chat.cas(&p.expected_resource_version)?;
    require(
        chat.phase == "Active",
        "PreconditionFailed",
        "conversation already archived",
    )?;
    transition(db, actor, chat, "Archived")
}

pub(crate) fn principal_revoked(db: &Db<'_>, principal: &str) -> Result<()> {
    room::archive_owned(db, principal)?;
    db.c.execute(
        "UPDATE chat_rooms SET revision=revision+1,membership_revision=membership_revision+1
         WHERE id IN (SELECT conversation FROM room_memberships WHERE principal=?1
                       AND role='Member' AND phase IN('Invited','Active'))",
        [principal],
    )?;
    db.c.execute(
        "UPDATE room_memberships SET phase='Removed',reason='PrincipalRevoked'
         WHERE principal=?1 AND role='Member' AND phase IN('Invited','Active')",
        [principal],
    )?;
    Ok(())
}

fn transition(db: &Db<'_>, actor: &str, chat: Chat, phase: &str) -> Result<Value> {
    db.c.execute(
        "UPDATE direct_chats SET phase=?2,revision=revision+1 WHERE id=?1",
        params![chat.id, phase],
    )?;
    Ok(member(db, actor, &chat.id)?.view())
}
