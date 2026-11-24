use super::{Access, READ, SEND, access, chat_id, timestamp};
use crate::{Result, db::Db, decode, err, events, label, now, policy, require};
use chrono::Utc;
use psios_api::{VERSION, canonical, contract, digest, id, sha256};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

const TEXT_BYTES: usize = 4096;
const MESSAGE_SELECT: &str = "SELECT m.receipt,m.sender,m.content,reply.id FROM chat_messages m
    LEFT JOIN chat_messages reply ON reply.id=m.reply_to AND reply.conversation=m.conversation AND reply.seq>?2";

fn writable(db: &Db<'_>, actor: &str, id: &str) -> Result<Access> {
    policy::can_target(db, actor, SEND, id)?;
    let chat = access(db, actor, id)?;
    require(
        chat.writable,
        "PreconditionFailed",
        "conversation is archived or expired",
    )?;
    if let Some(peer) = &chat.peer {
        policy::active(db, peer)?;
        policy::can_target(db, peer, READ, id)?;
    }
    Ok(chat)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Window {
    id: String,
    ttl_seconds: u32,
}

struct SendWindow {
    namespace: String,
    valid_until: i64,
    sender_read: String,
    sender_send: String,
    peer_read: Option<String>,
}

fn current_window(
    db: &Db<'_>,
    chat: &Access,
    actor: &str,
    stamp: i64,
) -> Result<Option<SendWindow>> {
    let Some(window) = db.c.query_row(
        "SELECT namespace,valid_until,sender_read_delegation,sender_send_delegation,peer_read_delegation
         FROM chat_send_windows WHERE conversation=?1 AND sender=?2 AND incarnation=?3 AND valid_until>?4 AND membership=?5",
        params![chat.id, actor, db.incarnation, stamp, chat.membership],
        |row| Ok(SendWindow { namespace: row.get(0)?, valid_until: row.get(1)?,
            sender_read: row.get(2)?, sender_send: row.get(3)?, peer_read: row.get(4)? }),
    ).optional()? else {
        return Ok(None);
    };
    if chat.peer.is_some() != window.peer_read.is_some() {
        return Ok(None);
    }
    let peer = chat
        .peer
        .as_deref()
        .zip(window.peer_read.as_deref())
        .map(|(principal, delegation)| (principal, READ, delegation));
    let sender = [
        (actor, READ, window.sender_read.as_str()),
        (actor, SEND, window.sender_send.as_str()),
    ];
    for (principal, action, delegation) in sender.into_iter().chain(peer) {
        match policy::delegation(db, principal, delegation, action)
            .and_then(|grant| policy::tool_scope(db, &grant.reference(), &chat.id))
        {
            Ok(_) => {}
            Err(error) if error.0.code == "PermissionDenied" => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    Ok(Some(window))
}

pub fn open_window(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Window = decode(request)?;
    require(
        (1..=3600).contains(&p.ttl_seconds),
        "InvalidArgument",
        "send window lifetime must be 1 to 3600 seconds",
    )?;
    let chat = writable(db, actor, &p.id)?;
    let stamp = Utc::now().timestamp();
    if let Some(window) = current_window(db, &chat, actor, stamp)? {
        return Ok(
            json!({"namespace": window.namespace, "valid_until": timestamp(window.valid_until)}),
        );
    }
    db.cfg.check_admission_space()?;
    let namespace = id("send");
    let until = stamp + i64::from(p.ttl_seconds);
    let sender_read = policy::authorized_delegation(db, actor, READ, Some(&chat.id))?;
    let sender_send = policy::authorized_delegation(db, actor, SEND, Some(&chat.id))?;
    let peer_read = chat
        .peer
        .as_deref()
        .map(|peer| {
            policy::authorized_delegation(db, peer, READ, Some(&chat.id)).map(|grant| grant.id)
        })
        .transpose()?;
    db.c.execute(
        "INSERT INTO chat_send_windows(conversation,sender,membership,namespace,incarnation,valid_until,
             sender_read_delegation,sender_send_delegation,peer_read_delegation) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(conversation,sender) DO UPDATE SET namespace=excluded.namespace,
             incarnation=excluded.incarnation,valid_until=excluded.valid_until,
             membership=excluded.membership,
             sender_read_delegation=excluded.sender_read_delegation,sender_send_delegation=excluded.sender_send_delegation,
             peer_read_delegation=excluded.peer_read_delegation",
        params![chat.id, actor, chat.membership, namespace, db.incarnation, until, sender_read.id, sender_send.id, peer_read],
    )?;
    Ok(json!({"namespace": namespace, "valid_until": timestamp(until)}))
}

fn require_key(db: &Db<'_>, chat: &Access, actor: &str, key: &str) -> Result<()> {
    let bad = || {
        err(
            "IdempotencyKeyExpired",
            "send key requires the current conversation namespace",
        )
    };
    let (namespace, nonce) = key.split_once(':').ok_or_else(bad)?;
    label(nonce)?;
    require(
        nonce.len() <= 120,
        "InvalidArgument",
        "client send suffix exceeds 120 bytes",
    )?;
    let window = current_window(db, chat, actor, Utc::now().timestamp())?.ok_or_else(bad)?;
    if window.namespace != namespace {
        return Err(bad());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepare {
    conversation_id: String,
    client_send_id: String,
    text: String,
    reply_to_message_id: Option<String>,
}

fn validate_text(text: &str) -> Result<()> {
    require(
        !text.is_empty()
            && text.len() <= TEXT_BYTES
            && !text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')),
        "InvalidArgument",
        "message must contain 1 to 4096 UTF-8 bytes of plain text",
    )
}

fn verify_reply(db: &Db<'_>, chat: &Access, reply: Option<&str>) -> Result<()> {
    if let Some(reply) = reply {
        let exists: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_messages WHERE conversation=?1 AND id=?2 AND seq>?3)",
            params![chat.id, reply, chat.read_after],
            |row| row.get(0),
        )?;
        require(
            exists,
            "NotFound",
            "reply target unavailable in this conversation",
        )?;
    }
    Ok(())
}

pub fn prepare(db: &Db<'_>, actor: &str, value: &Value) -> Result<Value> {
    let p: Prepare = decode(value)?;
    let chat = writable(db, actor, &p.conversation_id)?;
    require_key(db, &chat, actor, &p.client_send_id)?;
    validate_text(&p.text)?;
    verify_reply(db, &chat, p.reply_to_message_id.as_deref())?;
    let mut send = json!({"schema_version": VERSION, "conversation_id": chat.id,
        "client_send_id": p.client_send_id, "message_type": "message",
        "content": {"parts": [{"kind": "text", "text": p.text}]}, "mentions": [], "attachment_refs": []});
    if let Some(reply) = p.reply_to_message_id {
        send["reply_to_message_id"] = json!(reply);
    }
    send["content_digest"] = json!(digest("chat-send", "content_digest", &send)?);
    contract("chat-send", &send)?;
    Ok(send)
}

fn verify_send(p: &Value) -> Result<()> {
    contract("chat-send", p)?;
    let parts = p["content"]["parts"].as_array().unwrap();
    require(
        p["message_type"] == "message"
            && p["mentions"] == json!([])
            && p["attachment_refs"] == json!([])
            && parts.len() == 1
            && parts[0]["kind"] == "text"
            && ["thread_id", "correlation_id", "causation_id", "task_ref"]
                .iter()
                .all(|key| p.get(*key).is_none()),
        "UnsupportedCapability",
        "local conversations accept one text part and an optional same-room reply",
    )?;
    validate_text(parts[0]["text"].as_str().unwrap())?;
    require(
        p["content_digest"] == digest("chat-send", "content_digest", p)?,
        "InvalidArgument",
        "message digest does not match the send request",
    )
}

pub fn send(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    verify_send(p)?;
    let chat = writable(db, actor, p["conversation_id"].as_str().unwrap())?;
    let send_key = p["client_send_id"].as_str().unwrap();
    require_key(db, &chat, actor, send_key)?;
    let previous =
        db.c.query_row(
            "SELECT receipt FROM chat_messages WHERE conversation=?1 AND sender=?2 AND send_key=?3",
            params![chat.id, actor, send_key],
            |row| row.get::<_, Value>(0),
        )
        .optional()?;
    if let Some(mut receipt) = previous {
        require(
            receipt["content_digest"] == p["content_digest"],
            "IdempotencyConflict",
            "send key already bound",
        )?;
        receipt["deduplicated"] = json!(true);
        return Ok(receipt);
    }
    let reply = p["reply_to_message_id"].as_str();
    verify_reply(db, &chat, reply)?;
    db.cfg.check_admission_space()?;
    let total: u32 =
        db.c.query_row("SELECT count(*) FROM chat_messages", [], |row| row.get(0))?;
    require(
        chat.last_seq < 1024 && total < 16384,
        "QueueFull",
        "message retention capacity reached",
    )?;
    let sequence = chat.last_seq + 1;
    let message = chat_id("message", json!([chat.id, actor, send_key]));
    let receipt = json!({"schema_version": VERSION, "message_id": message, "conversation_id": chat.id,
        "client_send_id": send_key, "conversation_seq": sequence.to_string(), "writer_epoch": chat.writer_epoch.to_string(),
        "membership_revision": chat.membership_revision.to_string(), "recovery_incarnation": db.incarnation,
        "accepted_at": now(), "content_digest": p["content_digest"], "deduplicated": false,
        "durability_profile": {"profile_ref": db.get(policy::RUNTIME_PROFILE)?.reference(), "ack_level": "Durable",
                               "fault_scope": ["ProcessCrash"], "rpo_seconds": "0"}});
    contract("chat-accepted", &receipt)?;
    db.c.execute(
        "INSERT INTO chat_messages(conversation,seq,id,sender,send_key,receipt,content,reply_to)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![
            chat.id,
            sequence,
            message,
            actor,
            send_key,
            receipt,
            p["content"],
            reply
        ],
    )?;
    let advanced = db.c.execute(
        "UPDATE chat_logs SET last_seq=?2 WHERE id=?1 AND last_seq=?3 AND writer_epoch=?4",
        params![chat.id, sequence, chat.last_seq, chat.writer_epoch],
    )?;
    require(
        advanced == 1,
        "RecoveryRequired",
        "conversation writer or commit boundary changed",
    )?;
    Ok(receipt)
}

struct Stored {
    receipt: Value,
    sender: String,
    content: Value,
    reply: Option<String>,
}

impl Stored {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            receipt: row.get(0)?,
            sender: row.get(1)?,
            content: row.get(2)?,
            reply: row.get(3)?,
        })
    }

    fn view(self) -> Result<Value> {
        let mut message = self.receipt;
        let map = message.as_object_mut().unwrap();
        map.remove("deduplicated");
        let initial = map.remove("content_digest").unwrap();
        let content_id = chat_id("message-content", json!([message["message_id"], "r1"]));
        message["initial_send_digest"] = initial;
        message["sender_principal_id"] = json!(self.sender);
        message["message_revision"] = json!("r1");
        message["content_state"] = json!("Visible");
        message["message_type"] = json!("message");
        message["mentions"] = json!([]);
        message["attachment_refs"] = json!([]);
        message["retention_refs"] = json!([]);
        message["current_content_ref"] = json!({"resource_id": content_id, "version": "r1",
                                                   "digest": sha256(&canonical(&self.content))});
        if let Some(reply) = self.reply {
            message["reply_to_message_id"] = json!(reply);
        }
        contract("message", &message)?;
        Ok(json!({"message": message, "content": self.content}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Lookup {
    conversation_id: String,
    message_id: String,
    content_ref: Option<Value>,
}

pub fn get_message(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Lookup = decode(request)?;
    let chat = access(db, actor, &p.conversation_id)?;
    let view =
        db.c.query_row(
            &format!("{MESSAGE_SELECT} WHERE m.conversation=?1 AND m.seq>?2 AND m.id=?3"),
            params![chat.id, chat.read_after, p.message_id],
            Stored::from_row,
        )
        .optional()?
        .ok_or_else(|| err("NotFound", "message unavailable in this conversation"))?
        .view()?;
    if let Some(reference) = p.content_ref {
        require(
            reference == view["message"]["current_content_ref"],
            "PreconditionFailed",
            "message content reference changed",
        )?;
    }
    Ok(view)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    id: String,
    cursor: Option<String>,
    limit: Option<u32>,
}

fn cursor_scope(chat: &Access) -> String {
    if chat.membership.is_empty() {
        format!("{}:{}", chat.id, chat.membership_revision)
    } else {
        format!("{}:{}", chat.id, chat.membership)
    }
}

fn cursor_position(db: &Db<'_>, chat: &Access, actor: &str, token: &str) -> Result<i64> {
    let offset = events::read_position(db, actor, &cursor_scope(chat), token)?;
    require(
        (0..=chat.last_seq - chat.read_after).contains(&offset),
        "CursorExpired",
        "conversation history changed",
    )?;
    Ok(chat.read_after + offset)
}

fn read_sequence(db: &Db<'_>, chat: &Access, actor: &str) -> Result<i64> {
    Ok(db
        .c
        .query_row(
            "SELECT seq FROM chat_read_offsets WHERE conversation=?1 AND principal=?2 AND membership=?3",
            params![chat.id, actor, chat.membership],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

pub fn list_messages(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Page = decode(request)?;
    let chat = access(db, actor, &p.id)?;
    let mut after = p
        .cursor
        .as_deref()
        .map(|token| cursor_position(db, &chat, actor, token))
        .transpose()?
        .unwrap_or(chat.read_after);
    let limit = p.limit.unwrap_or(16);
    require(
        (1..=32).contains(&limit),
        "InvalidArgument",
        "message page limit must be 1 to 32",
    )?;
    let mut stmt = db.c.prepare(&format!(
        "{MESSAGE_SELECT} WHERE m.conversation=?1 AND m.seq>?3 ORDER BY m.seq LIMIT ?4"
    ))?;
    let rows = stmt
        .query_map(
            params![chat.id, chat.read_after, after, limit + 1],
            Stored::from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > limit as usize;
    let mut items = Vec::with_capacity(rows.len().min(limit as usize));
    for row in rows.into_iter().take(limit as usize) {
        after += 1;
        require(
            row.receipt["conversation_seq"]
                .as_str()
                .and_then(|value| value.parse::<i64>().ok())
                == Some(after),
            "RecoveryRequired",
            "gap in conversation message log",
        )?;
        items.push(row.view()?);
    }
    Ok(json!({"items": items, "has_more": has_more,
              "cursor": events::sign_position(db, actor, &cursor_scope(&chat), after - chat.read_after)?,
              "read_sequence": read_sequence(db, &chat, actor)?.to_string()}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    id: String,
    cursor: String,
}

pub fn ack(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Ack = decode(request)?;
    let chat = access(db, actor, &p.id)?;
    let next = cursor_position(db, &chat, actor, &p.cursor)?;
    let old = read_sequence(db, &chat, actor)?;
    if next == chat.read_after && old == 0 {
        return Ok(json!({"read_sequence": "0"}));
    }
    require(
        next >= old,
        "Conflict",
        "read position cannot move backward",
    )?;
    if next > old {
        db.c.execute(
            "INSERT INTO chat_read_offsets VALUES(?1,?2,?3,?4)
             ON CONFLICT(conversation,principal,membership) DO UPDATE SET seq=excluded.seq",
            params![chat.id, actor, chat.membership, next],
        )?;
    }
    Ok(json!({"read_sequence": next.to_string()}))
}
