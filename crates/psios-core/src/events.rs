pub(crate) mod offsets;
pub(crate) mod query;
pub(crate) mod scopes;

use crate::{
    db::{Db, row},
    policy, *,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use psios_api::{VERSION, contract};
use ring::hmac;
use rusqlite::{OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

struct Cursor {
    mode: String,
    actor: String,
    epoch: String,
    incarnation: String,
    kind: String,
    resource: Option<String>,
    snapshot: String,
    high: i64,
    position: i64,
    until: i64,
}

const ORDERED_SNAPSHOT_PREFIX: &str = "snapshot-";

#[derive(Deserialize, Serialize)]
struct CursorPayload(
    String,
    String,
    i64,
    i64,
    i64,
    #[serde(default, skip_serializing_if = "Option::is_none")] Option<String>,
);

impl Cursor {
    fn orders_by_creation(&self) -> bool {
        // Snapshot IDs are authenticated by the cursor. Existing pages keep
        // their original revision positions until they finish or expire.
        self.resource.is_none() && self.snapshot.starts_with(ORDERED_SNAPSHOT_PREFIX)
    }

    fn signing_mode(mode: &str, scoped: bool) -> &str {
        match (mode, scoped) {
            ("page", true) => "page-by-resource",
            ("watch", true) => "watch-by-resource",
            (mode, _) => mode,
        }
    }

    fn key(db: &Db<'_>, actor: &str, mode: &str, epoch: &str) -> Result<hmac::Key> {
        let root = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
        let scope = serde_json::to_vec(&("psios/cursor/1", actor, mode, epoch, &db.incarnation))?;
        Ok(hmac::Key::new(
            hmac::HMAC_SHA256,
            hmac::sign(&root, &scope).as_ref(),
        ))
    }
    fn sign(&self, db: &Db<'_>) -> Result<String> {
        let data = serde_json::to_vec(&CursorPayload(
            self.kind.clone(),
            self.snapshot.clone(),
            self.high,
            self.position,
            self.until,
            self.resource.clone(),
        ))?;
        let mode = Self::signing_mode(&self.mode, self.resource.is_some());
        let key = Self::key(db, &self.actor, mode, &self.epoch)?;
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&data),
            URL_SAFE_NO_PAD.encode(hmac::sign(&key, &data))
        );
        if matches!(self.mode.as_str(), "page" | "watch") && token.len() > scopes::MAX_TOKEN {
            return scopes::sign(db, self);
        }
        Ok(token)
    }
    fn read(db: &Db<'_>, token: &str, actor: &str, mode: &str) -> Result<Self> {
        if token.starts_with(scopes::PREFIX) {
            return scopes::read(db, token, actor, mode);
        }
        let cursor = Self::read_inline(db, token, actor, mode)?;
        require(
            cursor.until > chrono::Utc::now().timestamp(),
            "CursorExpired",
            "cursor expired or outside authorized query",
        )?;
        Ok(cursor)
    }

    fn read_inline(db: &Db<'_>, token: &str, actor: &str, mode: &str) -> Result<Self> {
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
        let CursorPayload(kind, snapshot, high, position, until, resource) =
            serde_json::from_slice(&data).map_err(|_| bad())?;
        let signing_mode = Self::signing_mode(mode, resource.is_some());
        hmac::verify(&Self::key(db, actor, signing_mode, &epoch)?, &data, &tag)
            .map_err(|_| bad())?;
        Ok(Self {
            mode: mode.into(),
            actor: actor.into(),
            epoch,
            incarnation: db.incarnation.clone(),
            kind,
            resource,
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
        resource: None,
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
        c.kind == scope && c.resource.is_none(),
        "CursorExpired",
        "cursor outside authorized stream",
    )?;
    Ok(c.position)
}

pub(crate) struct ScopedSnapshot {
    pub high: i64,
    pub position: i64,
    until: i64,
}

impl ScopedSnapshot {
    pub fn open(db: &Db<'_>, actor: &str, scope: &str, token: Option<&str>) -> Result<Self> {
        let Some(token) = token else {
            return Ok(Self {
                high: latest_sequence(db)?,
                position: 0,
                until: chrono::Utc::now().timestamp() + 600,
            });
        };
        require(
            token.len() <= 1024,
            "CursorExpired",
            "invalid snapshot cursor",
        )?;
        let cursor = Cursor::read(db, token, actor, "scoped-snapshot")?;
        require(
            cursor.kind == scope,
            "CursorExpired",
            "cursor outside authorized snapshot",
        )?;
        Ok(Self {
            high: cursor.high,
            position: cursor.position,
            until: cursor.until,
        })
    }

    pub fn sign(&self, db: &Db<'_>, actor: &str, scope: &str, position: i64) -> Result<String> {
        Cursor {
            mode: "scoped-snapshot".into(),
            actor: actor.into(),
            epoch: db.meta("epoch")?,
            incarnation: db.incarnation.clone(),
            kind: scope.into(),
            resource: None,
            snapshot: String::new(),
            high: self.high,
            position,
            until: self.until,
        }
        .sign(db)
    }
}

fn latest_sequence(db: &Db<'_>) -> Result<i64> {
    Ok(db
        .c
        .query_row("SELECT COALESCE(MAX(seq),0) FROM versions", [], |r| {
            r.get(0)
        })?)
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct List {
    kind: String,
    id: Option<String>,
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
    if let Some(id) = &p.id {
        label(id)?;
    }
    let mut c = if let Some(token) = p.cursor {
        let c = Cursor::read(db, &token, actor, "page")?;
        require(
            (p.kind.is_empty() || p.kind == c.kind)
                && p.id
                    .as_ref()
                    .is_none_or(|id| c.resource.as_ref() == Some(id)),
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
            resource: p.id,
            snapshot: format!("{ORDERED_SNAPSHOT_PREFIX}{}", uuid::Uuid::new_v4().simple()),
            high: latest_sequence(db)?,
            position: 0,
            until: chrono::Utc::now().timestamp() + scopes::TOKEN_TTL,
        }
    };
    let (sql, values) = query::selection(&c, query::View::Snapshot, n + 1);
    let mut items =
        db.c.prepare_cached(&sql)?
            .query_map(params_from_iter(values), |r| {
                Ok((r.get::<_, i64>("page_position")?, row(r)?.reference()))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    let complete = items.len() <= n as usize;
    items.truncate(n as usize);
    if let Some((position, _)) = items.last() {
        c.position = *position;
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
        "items": items.into_iter().map(|(_,reference)|reference).collect::<Vec<_>>(),
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
    let (sql, values) = query::selection(&c, query::View::Changes, n);
    let mut stmt = db.c.prepare_cached(&sql)?;
    let mut rows = stmt.query(params_from_iter(values))?;
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
    c.until = chrono::Utc::now().timestamp() + scopes::TOKEN_TTL;
    Ok(json!({"events": events, "cursor": c.sign(db)?}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    consumer: String,
    previous: Option<String>,
    cursor: String,
    expected_revision: Option<String>,
}

impl Ack {
    fn receipt(&self, db: &Db<'_>, actor: &str, stored: Option<&str>) -> Result<Value> {
        let mut result = json!({"cursor":self.cursor});
        if self.expected_revision.is_some() {
            result["revision"] = json!(offsets::revision(
                db,
                actor,
                &self.consumer,
                &self.cursor,
                stored
            )?);
        }
        Ok(result)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    kind: String,
    resource: Option<String>,
    snapshot: String,
    position: i64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    scope: Checkpoint,
    tag: String,
}

impl Checkpoint {
    fn scope_key(db: &Db<'_>) -> hmac::Key {
        let root = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
        hmac::Key::new(
            hmac::HMAC_SHA256,
            hmac::sign(&root, b"psios/event-offset/1").as_ref(),
        )
    }

    fn message(&self, actor: &str, consumer: &str, token: &str) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&(actor, consumer, token, self))?)
    }

    fn seal(self, db: &Db<'_>, actor: &str, consumer: &str, token: &str) -> Result<String> {
        let tag = URL_SAFE_NO_PAD.encode(hmac::sign(
            &Self::scope_key(db),
            &self.message(actor, consumer, token)?,
        ));
        Ok(serde_json::to_string(&StoredCheckpoint {
            scope: self,
            tag,
        })?)
    }

    fn stored(
        db: &Db<'_>,
        actor: &str,
        consumer: &str,
        token: &str,
        record: Option<&str>,
    ) -> Result<Self> {
        let corrupt = || err("RecoveryRequired", "stored checkpoint is unreadable");
        if token.starts_with(scopes::PREFIX) {
            // Acknowledgements outlive scope leases and their original
            // authorization epoch. Their scope is kept with the witnessed offset.
            let StoredCheckpoint { scope, tag } =
                serde_json::from_str(record.ok_or_else(corrupt)?).map_err(|_| corrupt())?;
            let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| corrupt())?;
            hmac::verify(
                &Self::scope_key(db),
                &scope.message(actor, consumer, token)?,
                &tag,
            )
            .map_err(|_| corrupt())?;
            return Ok(scope);
        }
        require(
            record.is_none(),
            "RecoveryRequired",
            "legacy checkpoint has an unexpected scope record",
        )?;
        let (payload, _) = token.split_once('.').ok_or_else(corrupt)?;
        let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| corrupt())?;
        let CursorPayload(kind, snapshot, _, position, _, resource) =
            serde_json::from_slice(&payload).map_err(|_| corrupt())?;
        Ok(Self {
            kind,
            resource,
            snapshot,
            position,
        })
    }
}

pub fn ack(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Ack = decode(params)?;
    label(&p.consumer)?;
    let expected = p.expected_revision.as_deref();
    require(
        params.get("expected_revision").is_none() || expected.is_some_and(offsets::valid_expected),
        "InvalidArgument",
        "invalid expected event offset revision",
    )?;
    let previous = p.previous.as_deref();
    require(
        expected.is_none() || previous.is_some_and(|cursor| !cursor.is_empty()),
        "InvalidArgument",
        "a checked event advance requires its previous cursor",
    )?;
    let old: Option<(String, Option<String>, Option<String>)> = db
        .c
        .query_row(
            "SELECT cursor,scope,checkpoint_revision FROM event_offsets WHERE owner=?1 AND consumer=?2",
            params![actor, p.consumer],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let old_token = old.as_ref().map(|(cursor, _, _)| cursor.as_str());
    let checked = if let Some((expected, previous)) = expected.zip(previous) {
        let (token, _, generation) = old
            .as_ref()
            .ok_or_else(|| err("Conflict", "consumer checkpoint changed"))?;
        let current = offsets::revision(db, actor, &p.consumer, token, generation.as_deref())?;
        let successor =
            offsets::checked_generation(db, actor, &p.consumer, expected, previous, &p.cursor)?;
        // Matching cursor text alone cannot distinguish a recreated offset from
        // the operation that committed after the original caller's reply was lost.
        require(
            (token == previous && current == expected)
                || (token == &p.cursor && generation.as_ref() == Some(&successor)),
            "Conflict",
            "consumer checkpoint changed",
        )?;
        Some(successor)
    } else {
        None
    };
    if let Some((token, record, generation)) = &old
        && token == &p.cursor
    {
        let checkpoint = Checkpoint::stored(db, actor, &p.consumer, token, record.as_deref())?;
        // A committed receipt can outlive its watch lease. The same signed
        // cursor can confirm receipt only within its original authorization epoch.
        if token.starts_with(scopes::PREFIX) {
            scopes::verify_acknowledged(db, actor, token, &checkpoint)?;
        } else {
            Cursor::read_inline(db, token, actor, "watch")?;
        }
        return p.receipt(db, actor, generation.as_deref());
    }
    let next = Cursor::read(db, &p.cursor, actor, "watch")?;
    require(
        old_token == previous,
        "Conflict",
        "consumer checkpoint changed",
    )?;
    if old.is_none() {
        offsets::check_admission(db, actor)?;
    }
    if let Some((token, record, _)) = old {
        let Checkpoint {
            kind,
            resource,
            snapshot,
            position,
        } = Checkpoint::stored(db, actor, &p.consumer, &token, record.as_deref())?;
        require(
            kind == next.kind && resource == next.resource,
            "Conflict",
            "consumer filter changed",
        )?;
        require(
            (snapshot == next.snapshot && position <= next.position)
                || (snapshot != next.snapshot
                    && next.position == next.high
                    && next.high >= position),
            "Conflict",
            "checkpoint would skip scope or move backwards",
        )?;
    }
    let record = p
        .cursor
        .starts_with(scopes::PREFIX)
        .then(|| {
            Checkpoint {
                kind: next.kind,
                resource: next.resource,
                snapshot: next.snapshot,
                position: next.position,
            }
            .seal(db, actor, &p.consumer, &p.cursor)
        })
        .transpose()?;
    let generation = checked.unwrap_or_else(offsets::new_generation);
    db.c.execute(
        "INSERT INTO event_offsets(owner,consumer,cursor,scope,checkpoint_revision) VALUES(?1,?2,?3,?4,?5)
        ON CONFLICT(owner,consumer) DO UPDATE SET cursor=excluded.cursor,scope=excluded.scope,
            checkpoint_revision=excluded.checkpoint_revision",
        params![actor, p.consumer, p.cursor, record, generation],
    )?;
    db.audit(actor, "Event.Ack", None, "Allowed")?;
    p.receipt(db, actor, Some(&generation))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unscoped_tokens_keep_the_original_payload() {
        let legacy = r#"["Task","original-snapshot",42,29,1000]"#;
        let decoded: CursorPayload = serde_json::from_str(legacy).unwrap();
        assert!(decoded.5.is_none());
        assert_eq!(serde_json::to_string(&decoded).unwrap(), legacy);
    }

    #[test]
    fn expired_inline_ack_confirms_only_the_exact_committed_position() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::Config::local(&directory.path().join("authority"));
        crate::Store::init(&config).unwrap();
        let mut store = crate::Store::open(config).unwrap();
        store
            .transaction("expired-ack-receipt", |db| {
                let actor = policy::OWNER;
                let page = list(db, actor, &json!({"kind":"Node"}))?;
                let mut cursor = Cursor::read(
                    db,
                    page["watch_start_cursor"].as_str().unwrap(),
                    actor,
                    "watch",
                )?;
                // Represent the same committed row after the signed wall-clock
                // deadline without waiting for the production watch TTL.
                cursor.until = chrono::Utc::now().timestamp() - 1;
                let token = cursor.sign(db)?;
                assert!(!token.starts_with(scopes::PREFIX));
                db.c.execute(
                    "INSERT INTO event_offsets(owner,consumer,cursor) VALUES(?1,'confirmed',?2)",
                    params![actor, token],
                )?;
                let request = json!({"consumer":"confirmed","cursor":token});
                let before = offsets::get(
                    db,
                    actor,
                    &json!({"id":"confirmed","include_revision":true}),
                )?;
                assert_eq!(ack(db, actor, &request)?, json!({"cursor":token}));
                let checked = json!({"consumer":"confirmed","cursor":token,"previous":token,
                                     "expected_revision":before["revision"]});
                assert_eq!(ack(db, actor, &checked)?, before);
                assert_eq!(
                    offsets::get(
                        db,
                        actor,
                        &json!({"id":"confirmed","include_revision":true})
                    )?,
                    before
                );
                assert_eq!(
                    poll(db, actor, &json!({"cursor":token}))
                        .unwrap_err()
                        .0
                        .code,
                    "CursorExpired"
                );
                assert_eq!(
                    ack(db, actor, &json!({"consumer":"new","cursor":token}))
                        .unwrap_err()
                        .0
                        .code,
                    "CursorExpired"
                );
                db.c.execute(
                    "UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='epoch'",
                    [],
                )?;
                assert_eq!(
                    ack(db, actor, &request).unwrap_err().0.code,
                    "CursorExpired"
                );
                assert_eq!(
                    ack(db, actor, &checked).unwrap_err().0.code,
                    "CursorExpired"
                );
                Ok(())
            })
            .unwrap();
    }
}
