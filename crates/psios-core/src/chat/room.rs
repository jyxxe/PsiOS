use super::{
    Access, INVITE, READ, Respond, chat_id, conversation_capacity, principal_capacity, timestamp,
};
use crate::{Conditional, Result, db::Db, decode, err, label, now, policy, require};
use chrono::Utc;
use psios_api::{canonical, id, sha256};
use rusqlite::{OptionalExtension, Row, params};
use serde::Deserialize;
use serde_json::{Value, json};

const ROOM: &str = "SELECT r.id,r.owner,r.name,r.phase,r.revision,r.membership_revision,r.capacity,
    r.expires,r.created_at,r.creation_digest,l.last_seq,l.writer_epoch
    FROM chat_rooms r JOIN chat_logs l ON l.id=r.id";
const MEMBER: &str = "SELECT id,principal,role,phase,invitation_until,read_after,reason
    FROM room_memberships";

struct Room {
    id: String,
    owner: String,
    name: String,
    phase: String,
    revision: i64,
    membership_revision: i64,
    capacity: u32,
    expires: i64,
    created_at: String,
    creation_digest: String,
    last_seq: i64,
    writer_epoch: i64,
}

impl Room {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            owner: row.get(1)?,
            name: row.get(2)?,
            phase: row.get(3)?,
            revision: row.get(4)?,
            membership_revision: row.get(5)?,
            capacity: row.get(6)?,
            expires: row.get(7)?,
            created_at: row.get(8)?,
            creation_digest: row.get(9)?,
            last_seq: row.get(10)?,
            writer_epoch: row.get(11)?,
        })
    }

    fn active(&self, stamp: i64) -> bool {
        self.phase == "Active" && stamp < self.expires
    }

    fn cas(&self, expected: &str) -> Result<()> {
        require(
            expected == format!("r{}", self.revision),
            "Conflict",
            "conversation revision changed",
        )
    }

    fn view(&self, db: &Db<'_>, viewer: &Member) -> Result<Value> {
        let stamp = Utc::now().timestamp();
        let mut stmt = db.c.prepare(&format!(
            "{MEMBER} WHERE conversation=?1 AND
                (id=?2 OR (?3 AND phase='Active') OR (?4 AND phase='Invited'))
             ORDER BY CASE role WHEN 'Owner' THEN 0 ELSE 1 END,created_at,id"
        ))?;
        let members = stmt
            .query_map(
                params![
                    self.id,
                    viewer.id,
                    viewer.phase == "Active",
                    viewer.principal == self.owner
                ],
                Member::from_row,
            )?
            .map(|row| row.map(|member| member.view(stamp)))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(
            json!({"id": self.id, "kind": "AdHocRoom", "name": self.name,
            "owner_principal_id": self.owner, "lifecycle": if self.active(stamp) {"Active"} else {"Archived"},
            "resource_version": format!("r{}",self.revision), "membership_revision": self.membership_revision.to_string(),
            "history_policy": "FromJoin", "capacity": self.capacity, "created_at": self.created_at,
            "expires_at": timestamp(self.expires), "membership_id": viewer.id, "members": members}),
        )
    }
}

struct Member {
    id: String,
    principal: String,
    role: String,
    phase: String,
    invitation_until: Option<i64>,
    read_after: i64,
    reason: Option<String>,
}

impl Member {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            principal: row.get(1)?,
            role: row.get(2)?,
            phase: row.get(3)?,
            invitation_until: row.get(4)?,
            read_after: row.get(5)?,
            reason: row.get(6)?,
        })
    }

    fn expired(&self, stamp: i64) -> bool {
        self.phase == "Invited" && self.invitation_until.is_some_and(|until| until <= stamp)
    }

    fn view(&self, stamp: i64) -> Value {
        let (phase, reason) = if self.expired(stamp) {
            ("Expired", Some("InvitationExpired"))
        } else {
            (self.phase.as_str(), self.reason.as_deref())
        };
        json!({"membership_id": self.id, "principal_id": self.principal, "role": self.role,
            "lifecycle": phase, "reason": reason, "invitation_valid_until": self.invitation_until.map(timestamp)})
    }
}

fn unavailable() -> crate::Error {
    err(
        "PermissionDenied",
        "conversation unavailable to this principal",
    )
}

pub(super) fn is_room(id: &str) -> bool {
    id.starts_with("room-")
}

fn find(db: &Db<'_>, id: &str) -> Result<Option<Room>> {
    Ok(db
        .c
        .query_row(&format!("{ROOM} WHERE r.id=?1"), [id], Room::from_row)
        .optional()?)
}

fn current_member(db: &Db<'_>, id: &str, actor: &str) -> Result<Member> {
    db.c.query_row(
        &format!(
            "{MEMBER} WHERE conversation=?1 AND principal=?2 AND phase IN('Invited','Active')"
        ),
        [id, actor],
        Member::from_row,
    )
    .optional()?
    .ok_or_else(unavailable)
}

fn membership(db: &Db<'_>, room: &str, member: &str) -> Result<Member> {
    db.c.query_row(
        &format!("{MEMBER} WHERE conversation=?1 AND id=?2"),
        [room, member],
        Member::from_row,
    )
    .optional()?
    .ok_or_else(unavailable)
}

fn visible(db: &Db<'_>, actor: &str, id: &str) -> Result<(Room, Member)> {
    policy::can_target(db, actor, READ, id)?;
    let member = current_member(db, id, actor)?;
    let room = find(db, id)?.ok_or_else(unavailable)?;
    Ok((room, member))
}

fn owned(db: &Db<'_>, actor: &str, id: &str) -> Result<(Room, Member)> {
    policy::can_target(db, actor, INVITE, id)?;
    let (room, member) = visible(db, actor, id)?;
    require(
        room.owner == actor && member.phase == "Active",
        "PermissionDenied",
        "room owner required",
    )?;
    Ok((room, member))
}

pub(super) fn access(db: &Db<'_>, actor: &str, id: &str) -> Result<Access> {
    let (room, member) = visible(db, actor, id)?;
    require(
        member.phase == "Active",
        "PermissionDenied",
        "membership is inactive",
    )?;
    require(
        member.read_after <= room.last_seq,
        "RecoveryRequired",
        "membership history boundary exceeds the conversation log",
    )?;
    Ok(Access {
        writable: room.active(Utc::now().timestamp()),
        id: room.id,
        last_seq: room.last_seq,
        writer_epoch: room.writer_epoch,
        membership_revision: room.membership_revision,
        membership: member.id,
        read_after: member.read_after,
        peer: None,
    })
}

pub(super) fn get(db: &Db<'_>, actor: &str, id: &str) -> Result<Value> {
    let (room, member) = visible(db, actor, id)?;
    room.view(db, &member)
}

pub(super) fn list(db: &Db<'_>, actor: &str) -> Result<Vec<Value>> {
    let mut stmt = db.c.prepare(
        "SELECT conversation FROM room_memberships WHERE principal=?1 AND phase IN('Invited','Active') ORDER BY conversation"
    )?;
    let mut items = Vec::new();
    for row in stmt.query_map([actor], |row| row.get::<_, String>(0))? {
        match get(db, actor, &row?) {
            Ok(view) => items.push(view),
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(items)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    key: String,
    name: String,
    ttl_seconds: u32,
    capacity: u32,
}

pub fn open(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let p: Open = decode(request)?;
    label(&p.key)?;
    label(&p.name)?;
    require(
        (2..=16).contains(&p.capacity) && (1..=604800).contains(&p.ttl_seconds),
        "InvalidArgument",
        "rooms require capacity 2 to 16 and lifetime 1 to 604800 seconds",
    )?;
    let id = chat_id("room", json!([db.meta("authority")?, actor, p.key]));
    policy::can_target(db, actor, INVITE, &id)?;
    policy::can_target(db, actor, READ, &id)?;
    let digest = sha256(&canonical(request));
    if let Some(room) = find(db, &id)? {
        require(
            room.creation_digest == digest,
            "IdempotencyConflict",
            "room key already bound",
        )?;
        return room.view(db, &current_member(db, &id, actor)?);
    }
    db.cfg.check_admission_space()?;
    conversation_capacity(db)?;
    principal_capacity(db, actor, None)?;
    let expires = Utc::now().timestamp() + i64::from(p.ttl_seconds);
    db.c.execute("INSERT INTO chat_logs(id) VALUES(?1)", [&id])?;
    db.c.execute(
        "INSERT INTO chat_rooms(id,owner,name,capacity,expires,created_at,creation_digest)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![id, actor, p.name, p.capacity, expires, now(), digest],
    )?;
    db.c.execute(
        "INSERT INTO room_memberships(id,conversation,principal,role,phase,created_at)
         VALUES(?1,?2,?3,'Owner','Active',?4)",
        params![psios_api::id("membership"), id, actor, now()],
    )?;
    get(db, actor, &id)
}

fn changed(db: &Db<'_>, room: &Room) -> Result<Room> {
    db.c.execute("UPDATE chat_rooms SET revision=revision+1,membership_revision=membership_revision+1 WHERE id=?1",
        [&room.id])?;
    find(db, &room.id)?.ok_or_else(unavailable)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invite {
    id: String,
    expected_resource_version: String,
    principal_id: String,
    ttl_seconds: u32,
}

pub fn invite(db: &Db<'_>, actor: &str, value: &Value) -> Result<Value> {
    let p: Invite = decode(value)?;
    require(
        is_room(&p.id),
        "UnsupportedCapability",
        "invitations require an AdHocRoom",
    )?;
    let (room, owner) = owned(db, actor, &p.id)?;
    room.cas(&p.expected_resource_version)?;
    let stamp = Utc::now().timestamp();
    require(
        room.active(stamp),
        "PreconditionFailed",
        "room archived or expired",
    )?;
    require(
        (1..=86400).contains(&p.ttl_seconds),
        "InvalidArgument",
        "invalid invitation lifetime",
    )?;
    policy::active(db, &p.principal_id)?;
    db.cfg.check_admission_space()?;
    principal_capacity(db, &p.principal_id, Some(&room.id))?;
    db.c.execute(
        "UPDATE room_memberships SET phase='Expired',reason='InvitationExpired'
         WHERE conversation=?1 AND phase='Invited' AND invitation_until<=?2",
        params![room.id, stamp],
    )?;
    let (reserved, total): (u32,u32) = db.c.query_row(
        "SELECT count(*) FILTER (WHERE phase IN('Invited','Active')),count(*) FROM room_memberships WHERE conversation=?1",
        [&room.id], |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    require(
        reserved < room.capacity && total < 128,
        "QueueFull",
        "room membership capacity reached",
    )?;
    let occupied: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM room_memberships WHERE conversation=?1 AND principal=?2 AND phase IN('Invited','Active'))",
        params![room.id,p.principal_id], |row| row.get(0),
    )?;
    require(!occupied, "Conflict", "principal already invited or active")?;
    let membership = id("membership");
    db.c.execute(
        "INSERT INTO room_memberships(id,conversation,principal,role,phase,invitation_until,created_at)
         VALUES(?1,?2,?3,'Member','Invited',?4,?5)",
        params![membership, room.id, p.principal_id, room.expires.min(stamp+i64::from(p.ttl_seconds)), now()],
    )?;
    Ok(json!({"conversation": changed(db, &room)?.view(db, &owner)?, "membership_id": membership}))
}

pub(super) fn respond(db: &Db<'_>, actor: &str, p: Respond) -> Result<Value> {
    let (room, invitee) = visible(db, actor, &p.id)?;
    require(
        p.membership_id.as_deref() == Some(invitee.id.as_str()),
        "PermissionDenied",
        "invitation unavailable",
    )?;
    room.cas(&p.expected_resource_version)?;
    let stamp = Utc::now().timestamp();
    require(
        room.active(stamp) && invitee.phase == "Invited" && !invitee.expired(stamp),
        "PreconditionFailed",
        "invitation is no longer active",
    )?;
    if p.accept {
        db.c.execute(
            "UPDATE room_memberships SET phase='Active',read_after=?2 WHERE id=?1",
            params![invitee.id, room.last_seq],
        )?;
    } else {
        db.c.execute(
            "UPDATE room_memberships SET phase='Left',reason='InvitationDeclined' WHERE id=?1",
            [&invitee.id],
        )?;
    }
    changed(db, &room)?.view(db, &membership(db, &room.id, &invitee.id)?)
}

pub(super) fn archive(db: &Db<'_>, actor: &str, p: Conditional) -> Result<Value> {
    let (room, owner) = owned(db, actor, &p.id)?;
    room.cas(&p.expected_resource_version)?;
    require(
        room.phase == "Active",
        "PreconditionFailed",
        "room already archived",
    )?;
    db.c.execute(
        "UPDATE chat_rooms SET phase='Archived',revision=revision+1 WHERE id=?1",
        [&room.id],
    )?;
    find(db, &room.id)?
        .ok_or_else(unavailable)?
        .view(db, &owner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Remove {
    id: String,
    expected_resource_version: String,
    membership_id: String,
}

pub fn remove(db: &Db<'_>, actor: &str, value: &Value) -> Result<Value> {
    let p: Remove = decode(value)?;
    let (room, owner) = owned(db, actor, &p.id)?;
    room.cas(&p.expected_resource_version)?;
    let member = membership(db, &room.id, &p.membership_id)?;
    require(
        member.role != "Owner",
        "PreconditionFailed",
        "room must retain its owner",
    )?;
    require(
        matches!(member.phase.as_str(), "Invited" | "Active"),
        "PreconditionFailed",
        "membership already ended",
    )?;
    db.c.execute(
        "UPDATE room_memberships SET phase='Removed',reason='RemovedByOwner' WHERE id=?1",
        [&member.id],
    )?;
    changed(db, &room)?.view(db, &owner)
}

pub fn leave(db: &Db<'_>, actor: &str, value: &Value) -> Result<Value> {
    let p: Conditional = decode(value)?;
    let (room, member) = visible(db, actor, &p.id)?;
    room.cas(&p.expected_resource_version)?;
    require(
        member.role != "Owner",
        "PreconditionFailed",
        "room owner can archive the room",
    )?;
    require(
        member.phase == "Active",
        "PreconditionFailed",
        "only active members can leave",
    )?;
    db.c.execute(
        "UPDATE room_memberships SET phase='Left',reason='LeftByMember' WHERE id=?1",
        [&member.id],
    )?;
    let room = changed(db, &room)?;
    Ok(
        json!({"id": room.id, "resource_version": format!("r{}", room.revision),
              "membership": membership(db, &room.id, &member.id)?.view(Utc::now().timestamp())}),
    )
}
