//! Compact, authenticated Resource.List and Watch filters with bounded custody.
use super::*;

pub(crate) const FORMAT: u32 = crate::node::network::probe::state::FORMAT + 1;
pub(crate) const SCHEMA: &str = "CREATE TABLE watch_cursor_scopes (
    id TEXT PRIMARY KEY, owner TEXT NOT NULL, epoch TEXT NOT NULL, incarnation TEXT NOT NULL,
    kind TEXT NOT NULL, resource TEXT NOT NULL, retain_until INTEGER NOT NULL,
    UNIQUE(owner,epoch,incarnation,kind,resource)
) STRICT;
CREATE INDEX watch_cursor_scopes_expiry ON watch_cursor_scopes(retain_until);
ALTER TABLE event_offsets ADD COLUMN scope TEXT;";

pub(super) const MAX_TOKEN: usize = 256;
pub(super) const PREFIX: &str = "scope1.";
pub(super) const TOKEN_TTL: i64 = 600;
const PER_PRINCIPAL: i64 = 64;
const PER_AUTHORITY: i64 = 2048;

#[derive(Serialize, Deserialize)]
struct Payload(String, String, i64, i64, i64);

fn signing_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "page" => Some("page-scope-reference"),
        "watch" => Some("watch-scope-reference"),
        _ => None,
    }
}

fn authenticated(data: &[u8], kind: &str, resource: Option<&str>) -> Result<Vec<u8>> {
    let mut bytes = data.to_vec();
    bytes.push(0);
    serde_json::to_writer(&mut bytes, &(kind, resource))?;
    Ok(bytes)
}

pub(crate) fn reclaim(db: &Db<'_>) -> Result<()> {
    reclaim_at(db, chrono::Utc::now().timestamp(), &db.meta("epoch")?)
}

fn reclaim_at(db: &Db<'_>, now: i64, epoch: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM watch_cursor_scopes WHERE retain_until<=?1 OR epoch<>?2 OR incarnation<>?3",
        params![now, epoch, db.incarnation],
    )?;
    Ok(())
}

fn claim(db: &Db<'_>, cursor: &Cursor) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let resource = cursor.resource.as_deref().unwrap_or_default();
    let existing: Option<(String, i64)> = db.c.query_row(
        "SELECT id,retain_until FROM watch_cursor_scopes
           WHERE owner=?1 AND epoch=?2 AND incarnation=?3 AND kind=?4 AND resource=?5 AND retain_until>?6",
        params![cursor.actor,cursor.epoch,cursor.incarnation,cursor.kind,resource,now],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    // Keep signed expiry independent of physical retention. Coarse renewal
    // avoids a new authority write on each ordinary or waiting poll.
    let retain_until = cursor.until.saturating_add(TOKEN_TTL);
    if let Some((id, previous)) = existing {
        if previous < cursor.until {
            db.c.execute(
                "UPDATE watch_cursor_scopes SET retain_until=?2 WHERE id=?1",
                params![id, retain_until],
            )?;
        }
        return Ok(id);
    }
    reclaim_at(db, now, &cursor.epoch)?;
    let (principal, total): (i64, i64) = db.c.query_row(
        "SELECT count(*) FILTER (WHERE owner=?1), count(*) FROM watch_cursor_scopes",
        [&cursor.actor],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    require(
        principal < PER_PRINCIPAL && total < PER_AUTHORITY,
        "QuotaExceeded",
        "active long resource cursor scopes exceed their admission limit",
    )?;
    let id = URL_SAFE_NO_PAD.encode(uuid::Uuid::new_v4().as_bytes());
    db.c.execute(
        "INSERT INTO watch_cursor_scopes(id,owner,epoch,incarnation,kind,resource,retain_until)
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![
            id,
            cursor.actor,
            cursor.epoch,
            cursor.incarnation,
            cursor.kind,
            resource,
            retain_until
        ],
    )?;
    Ok(id)
}

pub(super) fn sign(db: &Db<'_>, cursor: &Cursor) -> Result<String> {
    let mode = signing_mode(&cursor.mode)
        .ok_or_else(|| err("InternalError", "unsupported compact resource cursor mode"))?;
    let data = serde_json::to_vec(&Payload(
        claim(db, cursor)?,
        cursor.snapshot.clone(),
        cursor.high,
        cursor.position,
        cursor.until,
    ))?;
    let key = Cursor::key(db, &cursor.actor, mode, &cursor.epoch)?;
    let message = authenticated(&data, &cursor.kind, cursor.resource.as_deref())?;
    let token = format!(
        "{PREFIX}{}.{}",
        URL_SAFE_NO_PAD.encode(&data),
        URL_SAFE_NO_PAD.encode(hmac::sign(&key, &message))
    );
    require(
        token.len() <= MAX_TOKEN,
        "RecoveryRequired",
        "compact resource cursor exceeds the snapshot contract",
    )?;
    Ok(token)
}

pub(super) fn read(db: &Db<'_>, token: &str, actor: &str, mode: &str) -> Result<Cursor> {
    let bad = || {
        err(
            "CursorExpired",
            "cursor expired or outside authorized query",
        )
    };
    let signing_mode = signing_mode(mode).ok_or_else(bad)?;
    require(
        token.len() <= MAX_TOKEN,
        "CursorExpired",
        "invalid compact resource cursor",
    )?;
    let (data, tag) = token
        .strip_prefix(PREFIX)
        .and_then(|value| value.split_once('.'))
        .ok_or_else(bad)?;
    let data = URL_SAFE_NO_PAD.decode(data).map_err(|_| bad())?;
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| bad())?;
    let Payload(id, snapshot, high, position, until) =
        serde_json::from_slice(&data).map_err(|_| bad())?;
    let now = chrono::Utc::now().timestamp();
    if id.len() != 22 || until <= now {
        return Err(bad());
    }
    let epoch = db.meta("epoch")?;
    let (kind, resource): (String, String) =
        db.c.query_row(
            "SELECT kind,resource FROM watch_cursor_scopes
          WHERE id=?1 AND owner=?2 AND epoch=?3 AND incarnation=?4 AND retain_until>=?5",
            params![id, actor, epoch, db.incarnation, until],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(bad)?;
    let resource = (!resource.is_empty()).then_some(resource);
    let message = authenticated(&data, &kind, resource.as_deref())?;
    hmac::verify(
        &Cursor::key(db, actor, signing_mode, &epoch)?,
        &message,
        &tag,
    )
    .map_err(|_| bad())?;
    Ok(Cursor {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store(path: &std::path::Path) -> crate::Store {
        let config = crate::Config::local(&path.join("authority"));
        crate::Store::init(&config).unwrap();
        crate::Store::open(config).unwrap()
    }

    fn exact(id: &str) -> Value {
        json!({"kind":"WatchFixture","id":id})
    }

    #[test]
    fn compact_watch_renews_exactly_and_checkpoint_survives_scope_expiry() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = store(directory.path());
        let actor = policy::OWNER;
        // The backslash and quote also exercise maximal JSON escaping.
        let target = "\\\"".repeat(100);
        let other = "a".repeat(200);
        store
            .transaction("bounded-watch-expiry", |db| {
                let page = list(db, actor, &exact(&target))?;
                assert_eq!(page["items"], json!([]));
                let initial = page["watch_start_cursor"].as_str().unwrap();
                assert!(initial.starts_with(PREFIX) && initial.len() <= MAX_TOKEN);
                assert_eq!(
                    Cursor::read(db, initial, "other-actor", "watch")
                        .err()
                        .unwrap()
                        .0
                        .code,
                    "CursorExpired"
                );
                assert_eq!(
                    Cursor::read(db, initial, actor, "page")
                        .err()
                        .unwrap()
                        .0
                        .code,
                    "CursorExpired"
                );
                db.c.execute("UPDATE watch_cursor_scopes SET resource=?1", [&other])?;
                assert_eq!(
                    Cursor::read(db, initial, actor, "watch")
                        .err()
                        .unwrap()
                        .0
                        .code,
                    "CursorExpired"
                );
                db.c.execute("UPDATE watch_cursor_scopes SET resource=?1", [&target])?;

                let first = db.create(
                    "WatchFixture",
                    &target,
                    actor,
                    json!({}),
                    json!({"lifecycle":"Initial"}),
                )?;
                db.create(
                    "WatchFixture",
                    &other,
                    actor,
                    json!({}),
                    json!({"lifecycle":"Initial"}),
                )?;
                let next = db.status(&first, json!({"lifecycle":"Ready"}))?;
                let mut soon = Cursor::read(db, initial, actor, "watch")?;
                soon.until = chrono::Utc::now().timestamp() + 100;
                let token = soon.sign(db)?;
                db.c.execute(
                    "UPDATE watch_cursor_scopes SET retain_until=?1",
                    [soon.until],
                )?;
                let read = poll(db, actor, &json!({"cursor":token}))?;
                assert_eq!(read["events"].as_array().unwrap().len(), 2);
                assert_eq!(read["events"][0]["resource_ref"], first.reference());
                assert_eq!(read["events"][1]["resource_ref"], next.reference());
                let renewed = read["cursor"].as_str().unwrap();
                assert!(
                    renewed.len() <= MAX_TOKEN
                        && Cursor::read(db, renewed, actor, "watch")?.until > soon.until
                );
                ack(
                    db,
                    actor,
                    &json!({"consumer":"expiry-test","cursor":renewed}),
                )?;

                db.c.execute("UPDATE watch_cursor_scopes SET retain_until=0", [])?;
                reclaim(db)?;
                assert_eq!(
                    poll(db, actor, &json!({"cursor":renewed}))
                        .unwrap_err()
                        .0
                        .code,
                    "CursorExpired"
                );
                assert_eq!(
                    offsets::get(db, actor, &json!({"id":"expiry-test"}))?["cursor"],
                    renewed
                );
                let wrong = list(db, actor, &exact(&other))?;
                db.c.execute_batch("SAVEPOINT changed_offset_scope")?;
                db.c.execute(
                    "UPDATE event_offsets SET scope=json_set(scope,'$.scope.resource',?1)",
                    [&other],
                )?;
                assert_eq!(
                    ack(
                        db,
                        actor,
                        &json!({"consumer":"expiry-test","previous":renewed,
                    "cursor":wrong["watch_start_cursor"]})
                    )
                    .unwrap_err()
                    .0
                    .code,
                    "RecoveryRequired",
                );
                db.c.execute_batch(
                    "ROLLBACK TO changed_offset_scope; RELEASE changed_offset_scope",
                )?;
                let wrong = ack(
                    db,
                    actor,
                    &json!({"consumer":"expiry-test","previous":renewed,
                "cursor":wrong["watch_start_cursor"]}),
                )
                .unwrap_err();
                assert_eq!(wrong.0.code, "Conflict");
                let refresh = list(db, actor, &exact(&target))?;
                assert_eq!(refresh["items"], json!([next.reference()]));
                assert_eq!(
                    ack(
                        db,
                        actor,
                        &json!({"consumer":"expiry-test","previous":renewed,
                "cursor":refresh["watch_start_cursor"]})
                    )?["cursor"],
                    refresh["watch_start_cursor"]
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn scope_admission_reuses_exact_filter_and_reclaims_both_limits() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = store(directory.path());
        store.transaction("bounded-watch-admission", |db| {
            let resource = |n| format!("{n:04}{}", "x".repeat(196));
            for n in 0..PER_PRINCIPAL {
                list(db, policy::OWNER, &exact(&resource(n)))?;
            }
            list(db, policy::OWNER, &exact(&resource(0)))?;
            assert_eq!(list(db, policy::OWNER, &exact(&resource(PER_PRINCIPAL))).unwrap_err().0.code, "QuotaExceeded");
            let live = db.c.query_row("SELECT count(*) FROM watch_cursor_scopes", [], |r| r.get::<_, i64>(0))?;
            assert_eq!(live, PER_PRINCIPAL);
            db.c.execute("UPDATE watch_cursor_scopes SET retain_until=0 WHERE resource=?1", [resource(0)])?;
            list(db, policy::OWNER, &exact(&resource(PER_PRINCIPAL)))?;
            db.c.execute(
                "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<?1)
                 INSERT INTO watch_cursor_scopes SELECT 'synthetic-'||v,'test-'||v,?2,?3,'Task','fixture',?4 FROM n",
                 params![PER_AUTHORITY - PER_PRINCIPAL, db.meta("epoch")?, db.incarnation, chrono::Utc::now().timestamp()+800],
            )?;
            assert_eq!(list(db, "other-actor", &exact(&resource(0))).unwrap_err().0.code, "QuotaExceeded");
            db.c.execute("UPDATE watch_cursor_scopes SET retain_until=0 WHERE id LIKE 'synthetic-%'", [])?;
            list(db, "other-actor", &exact(&resource(0)))?;
            Ok(())
        }).unwrap();
    }
}
