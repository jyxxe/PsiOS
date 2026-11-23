//! Opt-in point-to-point notifications. Content and linked resources keep their own ownership.
use super::*;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Bind {
    name: String,
    sender_principal_id: String,
}
pub fn bind(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "mail.receive")?;
    let v: Bind = decode(p)?;
    label(&v.name)?;
    policy::active(db, &v.sender_principal_id)?;
    let id = registry::stable("mailbox", json!([actor, v.sender_principal_id, v.name]));
    if let Ok(d) = db.get(&id) {
        require(state(&d) == "Open", "PreconditionFailed", "mailbox closed")?;
        return Ok(d.reference());
    }
    let count: u32 = db.c.query_row(
        "SELECT count(*) FROM current WHERE kind='Mailbox' AND owner=?1",
        [actor],
        |r| r.get(0),
    )?;
    require(count < 128, "QuotaExceeded", "mailbox registration limit")?;
    Ok(db.create("Mailbox",&id,actor,p.clone(),json!({"lifecycle":"Open","sender":v.sender_principal_id,"received":0,"acked":0,"delivered":0}))?.reference())
}
pub fn close(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let c: Conditional = decode(p)?;
    let d = db.of(&c.id, "Mailbox")?;
    policy::owned(actor, &d)?;
    d.cas(&c.expected_resource_version)?;
    let mut b = d.body.clone();
    b["lifecycle"] = json!("Closed");
    Ok(db.status(&d, b)?.reference())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    mailbox_id: String,
    key: String,
    topic: String,
    resource_ref: Option<Value>,
}
pub fn send(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "mail.send")?;
    let v: Send = decode(p)?;
    label(&v.key)?;
    require(
        matches!(
            v.topic.as_str(),
            "ObservationReady" | "ReviewRequested" | "WorkAvailable"
        ),
        "InvalidArgument",
        "unsupported signal topic",
    )?;
    let d = db.of(&v.mailbox_id, "Mailbox")?;
    require(
        state(&d) == "Open" && d.spec["sender_principal_id"] == actor,
        "PermissionDenied",
        "mailbox does not accept this sender",
    )?;
    policy::active(db, &d.owner)?;
    policy::can(db, &d.owner, "mail.receive")?;
    if let Some(r) = &v.resource_ref {
        let doc = db.resolve(r)?;
        policy::can(db, actor, "resource.read")?;
        policy::can(db, &d.owner, "resource.read")?;
        require(
            policy::visible(actor, &doc) && policy::visible(&d.owner, &doc),
            "PermissionDenied",
            "reference not shared by both principals",
        )?;
    }
    let id = registry::stable("signal", json!([d.id, actor, v.key]));
    if let Ok(old) = db.get(&id) {
        require(
            old.spec == *p,
            "IdempotencyConflict",
            "signal key already bound",
        )?;
        return Ok(old.reference());
    }
    let seq = d.body["received"].as_u64().unwrap() + 1;
    require(
        seq <= 16384 && seq - d.body["acked"].as_u64().unwrap() <= 256,
        "QueueFull",
        "mailbox retention or outstanding signal limit",
    )?;
    let signal=db.create("AgentSignal",&id,&d.owner,p.clone(),json!({"mailbox_id":d.id,"sender":actor,"topic":v.topic,"seq":seq,"resource_ref":v.resource_ref}))?;
    db.c.execute(
        "INSERT INTO signals VALUES(?1,?2,?3)",
        params![d.id, seq as i64, id],
    )?;
    let mut b = d.body.clone();
    b["received"] = json!(seq);
    db.status(&d, b)?;
    Ok(signal.reference())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    id: String,
    limit: Option<u32>,
}
pub fn read(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "mail.receive")?;
    let p: Read = decode(p)?;
    let d = db.of(&p.id, "Mailbox")?;
    policy::owned(actor, &d)?;
    let limit = p.limit.unwrap_or(32);
    require(
        (1..=128).contains(&limit),
        "InvalidArgument",
        "invalid inbox page limit",
    )?;
    let ids =
        db.c.prepare("SELECT id FROM signals WHERE mailbox=?1 AND seq>?2 ORDER BY seq LIMIT ?3")?
            .query_map(params![d.id, d.body["acked"].as_i64(), limit], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut items = Vec::new();
    let mut last = d.body["delivered"].as_u64().unwrap();
    for id in ids {
        let signal = db.of(&id, "AgentSignal")?;
        last = last.max(signal.body["seq"].as_u64().unwrap());
        items.push(json!({"reference":signal.reference(),"signal":signal.body}));
    }
    let d = if d.body["delivered"] != last {
        let mut b = d.body.clone();
        b["delivered"] = json!(last);
        db.status(&d, b)?
    } else {
        d
    };
    Ok(json!({"mailbox_ref":d.reference(),"acked":d.body["acked"],"items":items}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    id: String,
    previous: u32,
    through: u32,
}
pub fn ack(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "mail.receive")?;
    let p: Ack = decode(p)?;
    let d = db.of(&p.id, "Mailbox")?;
    policy::owned(actor, &d)?;
    if d.body["acked"] == p.through {
        return Ok(d.reference());
    }
    require(
        d.body["acked"] == p.previous
            && p.through > p.previous
            && u64::from(p.through) <= d.body["delivered"].as_u64().unwrap(),
        "Conflict",
        "acknowledgement skips delivery or conflicts with consumer",
    )?;
    let mut b = d.body.clone();
    b["acked"] = json!(p.through);
    Ok(db.status(&d, b)?.reference())
}
pub(super) fn consume(db: &Db<'_>, actor: &str, signal: &Value, session: &str) -> Result<()> {
    policy::can(db, actor, "mail.receive")?;
    let d = db.resolve(signal)?;
    require(
        d.kind == "AgentSignal" && d.owner == actor,
        "PermissionDenied",
        "signal belongs to another recipient",
    )?;
    let box_ = db.of(d.body["mailbox_id"].as_str().unwrap(), "Mailbox")?;
    require(
        state(&box_) == "Open",
        "PreconditionFailed",
        "signal channel closed",
    )?;
    require(
        db.c.execute(
            "INSERT OR IGNORE INTO signal_sessions VALUES(?1,?2)",
            params![d.id, session],
        )? == 1,
        "IdempotencyConflict",
        "signal already routed to another session",
    )
}
