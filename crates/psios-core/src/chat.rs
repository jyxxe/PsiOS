//! Local direct conversations using the Core identity and commit witness.
mod message;

use crate::{ById, Conditional, Result, db::Db, decode, err, label, now, policy, require};
use chrono::{DateTime, SecondsFormat, Utc};
use psios_api::{canonical, sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

pub use message::{ack, get_message, list_messages, open_window, prepare, send};
pub const INVITE: &str = "chat.invite";
pub const READ: &str = "chat.read";
pub const SEND: &str = "chat.send";

const SELECT: &str = "SELECT id,inviter,invitee,name,phase,revision,last_seq,invitation_until,created_at,creation_digest FROM direct_chats";

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
        .query_row(&format!("{SELECT} WHERE id=?1"), [id], Chat::from_row)
        .optional()?)
}

fn member(db: &Db<'_>, actor: &str, id: &str) -> Result<Chat> {
    policy::can(db, actor, READ)?;
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
    policy::can(db, actor, INVITE)?;
    policy::can(db, actor, READ)?;
    let p: Open = decode(request)?;
    label(&p.key)?;
    label(&p.name)?;
    require(
        actor != p.recipient_principal_id && (1..=86400).contains(&p.ttl_seconds),
        "InvalidArgument",
        "a direct invitation requires another principal and a bounded lifetime",
    )?;
    let id = chat_id("direct", json!([db.meta("authority")?, actor, p.key]));
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
    let total: u32 =
        db.c.query_row("SELECT count(*) FROM direct_chats", [], |row| row.get(0))?;
    require(
        total < 512,
        "QueueFull",
        "node conversation capacity reached",
    )?;
    for person in [actor, &p.recipient_principal_id] {
        let count: u32 = db.c.query_row(
            "SELECT count(*) FROM direct_chats WHERE inviter=?1 OR invitee=?1",
            [person],
            |row| row.get(0),
        )?;
        require(
            count < 64,
            "QueueFull",
            "principal conversation capacity reached",
        )?;
    }
    let until = Utc::now().timestamp() + i64::from(p.ttl_seconds);
    db.c.execute(
        "INSERT INTO direct_chats(id,inviter,invitee,name,invitation_until,created_at,creation_digest)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![id, actor, p.recipient_principal_id, p.name, until, now(), digest],
    )?;
    Ok(member(db, actor, &id)?.view())
}

pub fn get(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: ById = decode(request)?;
    Ok(member(db, actor, &p.id)?.view())
}

pub fn list(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct List {}
    let _: List = decode(request)?;
    policy::can(db, actor, READ)?;
    let mut statement = db.c.prepare(&format!(
        "{SELECT} WHERE inviter=?1 OR invitee=?1 ORDER BY created_at,id"
    ))?;
    let items = statement
        .query_map([actor], Chat::from_row)?
        .map(|row| row.map(|chat| chat.view()))
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"items": items}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Respond {
    id: String,
    expected_resource_version: String,
    accept: bool,
}

pub fn respond(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Respond = decode(request)?;
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

fn transition(db: &Db<'_>, actor: &str, chat: Chat, phase: &str) -> Result<Value> {
    db.c.execute(
        "UPDATE direct_chats SET phase=?2,revision=revision+1 WHERE id=?1",
        params![chat.id, phase],
    )?;
    Ok(member(db, actor, &chat.id)?.view())
}
