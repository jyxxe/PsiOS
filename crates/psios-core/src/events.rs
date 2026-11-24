use crate::{
    db::{Db, row},
    policy, *,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{VERSION, contract};
use ring::hmac;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

struct Cursor {
    mode: String,
    actor: String,
    epoch: String,
    incarnation: String,
    kind: String,
    snapshot: String,
    high: i64,
    position: i64,
    until: i64,
}
impl Cursor {
    fn key(db: &Db<'_>, actor: &str, mode: &str, epoch: &str) -> Result<hmac::Key> {
        let root = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
        let scope = serde_json::to_vec(&("psios/cursor/1", actor, mode, epoch, &db.incarnation))?;
        Ok(hmac::Key::new(
            hmac::HMAC_SHA256,
            hmac::sign(&root, &scope).as_ref(),
        ))
    }
    fn sign(&self, db: &Db<'_>) -> Result<String> {
        let data = serde_json::to_vec(&(
            &self.kind,
            &self.snapshot,
            self.high,
            self.position,
            self.until,
        ))?;
        let key = Self::key(db, &self.actor, &self.mode, &self.epoch)?;
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&data),
            URL_SAFE_NO_PAD.encode(hmac::sign(&key, &data))
        ))
    }
    fn read(db: &Db<'_>, token: &str, actor: &str, mode: &str) -> Result<Self> {
        let bad = || {
            err(
                "CursorExpired",
                "cursor expired or outside authorized query",
            )
        };
        let (data, tag) = token.split_once('.').ok_or_else(bad)?;
        let data = URL_SAFE_NO_PAD.decode(data).map_err(|_| bad())?;
        let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| bad())?;
        let epoch = db.meta("epoch")?;
        hmac::verify(&Self::key(db, actor, mode, &epoch)?, &data, &tag).map_err(|_| bad())?;
        let (kind, snapshot, high, position, until) =
            serde_json::from_slice(&data).map_err(|_| bad())?;
        if chrono::Utc::now().timestamp() >= until {
            return Err(bad());
        }
        Ok(Self {
            mode: mode.into(),
            actor: actor.into(),
            epoch,
            incarnation: db.incarnation.clone(),
            kind,
            snapshot,
            high,
            position,
            until,
        })
    }
}

pub(crate) fn sign_position(
    db: &Db<'_>,
    actor: &str,
    scope: &str,
    position: i64,
) -> Result<String> {
    Cursor {
        mode: "ordered-stream".into(),
        actor: actor.into(),
        epoch: db.meta("epoch")?,
        incarnation: db.incarnation.clone(),
        kind: scope.into(),
        snapshot: String::new(),
        high: 0,
        position,
        until: chrono::Utc::now().timestamp() + 600,
    }
    .sign(db)
}

pub(crate) fn read_position(db: &Db<'_>, actor: &str, scope: &str, token: &str) -> Result<i64> {
    require(
        token.len() <= 1024,
        "CursorExpired",
        "invalid stream cursor",
    )?;
    let c = Cursor::read(db, token, actor, "ordered-stream")?;
    require(
        c.kind == scope,
        "CursorExpired",
        "cursor outside authorized stream",
    )?;
    Ok(c.position)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct List {
    kind: String,
    cursor: Option<String>,
    limit: Option<u32>,
}
fn limit(n: Option<u32>) -> Result<u32> {
    let n = n.unwrap_or(32);
    require(
        (1..=128).contains(&n),
        "InvalidArgument",
        "page limit must be 1..128",
    )?;
    Ok(n)
}
pub fn list(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: List = decode(p)?;
    let n = limit(p.limit)?;
    require(
        p.kind.len() <= 32 && p.kind.bytes().all(|b| b.is_ascii_alphanumeric()),
        "InvalidArgument",
        "invalid kind filter",
    )?;
    let mut c = if let Some(token) = p.cursor {
        let c = Cursor::read(db, &token, actor, "page")?;
        require(
            p.kind.is_empty() || p.kind == c.kind,
            "CursorExpired",
            "query filter changed",
        )?;
        c
    } else {
        Cursor {
            mode: "page".into(),
            actor: actor.into(),
            epoch: db.meta("epoch")?,
            incarnation: db.incarnation.clone(),
            kind: p.kind,
            snapshot: uuid::Uuid::new_v4().simple().to_string(),
            high: db
                .c
                .query_row("SELECT COALESCE(MAX(seq),0) FROM versions", [], |r| {
                    r.get(0)
                })?,
            position: 0,
            until: chrono::Utc::now().timestamp() + 600,
        }
    };
    let mut docs=db.c.prepare("SELECT * FROM versions v WHERE v.seq<=?1 AND v.seq>?2 AND (?3='' OR v.kind=?3)
      AND (?4=?5 OR v.owner IN (?4,'system')) AND NOT EXISTS(SELECT 1 FROM versions w WHERE w.id=v.id AND w.seq<=?1 AND w.rev>v.rev)
      ORDER BY v.seq LIMIT ?6")?.query_map(params![c.high,c.position,c.kind,actor,policy::OWNER,n+1],|r|Ok((r.get::<_,i64>("seq")?,row(r)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let complete = docs.len() <= n as usize;
    docs.truncate(n as usize);
    if let Some(d) = docs.last() {
        c.position = d.0
    }
    let next = if complete { None } else { Some(c.sign(db)?) };
    c.mode = "watch".into();
    c.position = c.high;
    let expiry = chrono::DateTime::from_timestamp(c.until, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let page = json!({
        "schema_version": VERSION,
        "authority_id": db.meta("authority")?,
        "recovery_incarnation": c.incarnation,
        "snapshot_id": c.snapshot,
        "items": docs.iter().map(|(_,d)|d.reference()).collect::<Vec<_>>(),
        "complete": complete,
        "next_cursor": next,
        "watch_start_cursor": c.sign(db)?,
        "expires_at": expiry,
        "authorization_epoch": c.epoch
    });
    contract("snapshot-page", &page)?;
    Ok(page)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    cursor: String,
    limit: Option<u32>,
}
pub fn poll(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Poll = decode(p)?;
    let mut c = Cursor::read(db, &p.cursor, actor, "watch")?;
    let n = limit(p.limit)?;
    let auth = db.meta("authority")?;
    let context = db.get(policy::PROFILE)?.reference();
    let mut stmt=db.c.prepare("SELECT * FROM versions WHERE seq>?1 AND (?2='' OR kind=?2) AND (?3=?4 OR owner IN (?3,'system')) ORDER BY seq LIMIT ?5")?;
    let mut rows = stmt.query(params![c.position, c.kind, actor, policy::OWNER, n])?;
    let mut events = Vec::new();
    while let Some(r) = rows.next()? {
        let d = row(r)?;
        c.position = r.get("seq")?;
        let rf = d.reference();
        let mut event = json!({"schema_version":VERSION,
            "event_type":format!("psios.{}.updated.v1",d.kind.to_ascii_lowercase()),"event_id":r.get::<_,String>("event_id")?,
            "source_authority_id":auth,"recovery_incarnation":r.get::<_,String>("incarnation")?,"source_seq":c.position.to_string(),
            "occurred_at":d.stamp,"received_at":d.stamp,"resource_ref":rf,"payload_ref":rf,
            "correlation_id":r.get::<_,String>("correlation")?,"authorization_context_ref":context});
        if d.kind == "Task" {
            event["task_id"] = json!(d.id)
        }
        if d.kind == "Run" {
            event["task_id"] = d.body["task_id"].clone();
            event["run_id"] = json!(d.id)
        }
        contract("event", &event)?;
        events.push(event);
    }
    c.until = chrono::Utc::now().timestamp() + 600;
    Ok(json!({"events": events, "cursor": c.sign(db)?}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    consumer: String,
    previous: Option<String>,
    cursor: String,
}
pub fn ack(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: Ack = decode(p)?;
    label(&p.consumer)?;
    let next = Cursor::read(db, &p.cursor, actor, "watch")?;
    let old: Option<String> =
        db.c.query_row(
            "SELECT cursor FROM event_offsets WHERE owner=?1 AND consumer=?2",
            params![actor, p.consumer],
            |r| r.get(0),
        )
        .optional()?;
    if old.as_ref() == Some(&p.cursor) {
        return Ok(json!({"cursor": p.cursor}));
    }
    require(old == p.previous, "Conflict", "consumer checkpoint changed")?;
    if let Some(old) = old {
        // This value was authenticated at its original commit and is read from witnessed storage.
        // Its old signature cannot authorize a new read after a policy epoch changes.
        let corrupt = || err("RecoveryRequired", "stored checkpoint is unreadable");
        let (payload, _) = old.split_once('.').ok_or_else(corrupt)?;
        let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| corrupt())?;
        let (kind, snapshot, _, position, _): (String, String, i64, i64, i64) =
            serde_json::from_slice(&payload).map_err(|_| corrupt())?;
        require(kind == next.kind, "Conflict", "consumer filter changed")?;
        require(
            (snapshot == next.snapshot && position <= next.position)
                || (snapshot != next.snapshot
                    && next.position == next.high
                    && next.high >= position),
            "Conflict",
            "checkpoint would skip scope or move backwards",
        )?;
    }
    db.c.execute("INSERT INTO event_offsets VALUES(?1,?2,?3) ON CONFLICT(owner,consumer) DO UPDATE SET cursor=excluded.cursor",params![actor,p.consumer,p.cursor])?;
    Ok(json!({"cursor": p.cursor}))
}
pub fn offset(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    let p: ById = decode(p)?;
    let cursor: Option<String> =
        db.c.query_row(
            "SELECT cursor FROM event_offsets WHERE owner=?1 AND consumer=?2",
            params![actor, p.id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json!({"cursor": cursor}))
}
