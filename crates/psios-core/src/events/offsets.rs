//! Admission and explicit retirement of confirmed consumer offsets.
use super::*;

pub(crate) const FORMAT: u32 = crate::apps::health::FORMAT + 1;
pub(crate) const SCHEMA: &str = "ALTER TABLE event_offsets ADD COLUMN checkpoint_revision TEXT;";
const PER_PRINCIPAL: i64 = 128;
const PER_AUTHORITY: i64 = 4096;
const NEW_PREFIX: &str = "offset-";
const LEGACY_PREFIX: &str = "legacy-";

pub(super) fn check_admission(db: &Db<'_>, actor: &str) -> Result<()> {
    // Limit work even when an existing authority is already over the new quota.
    let (principal, total): (i64, i64) = db.c.query_row(
        "SELECT
            (SELECT count(*) FROM (SELECT 1 FROM event_offsets WHERE owner=?1 LIMIT ?2)),
            (SELECT count(*) FROM (SELECT 1 FROM event_offsets LIMIT ?3))",
        params![actor, PER_PRINCIPAL, PER_AUTHORITY],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    require(
        principal < PER_PRINCIPAL && total < PER_AUTHORITY,
        "QuotaExceeded",
        "durable event consumer offset capacity is full",
    )
}

pub(super) fn new_revision() -> String {
    format!("{NEW_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

fn valid_generation(revision: &str) -> bool {
    revision.strip_prefix(NEW_PREFIX).is_some_and(|value| {
        value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_expected(revision: &str) -> bool {
    [NEW_PREFIX, LEGACY_PREFIX].into_iter().any(|prefix| {
        revision.strip_prefix(prefix).is_some_and(|value| {
            value.len() == 43
                && URL_SAFE_NO_PAD
                    .decode(value)
                    .is_ok_and(|bytes| bytes.len() == 32)
        })
    })
}

fn revision(
    db: &Db<'_>,
    owner: &str,
    consumer: &str,
    cursor: &str,
    stored: Option<&str>,
) -> Result<String> {
    require(
        stored.is_none_or(valid_generation),
        "RecoveryRequired",
        "stored event offset revision is invalid",
    )?;
    // Existing rows keep their exact offset and avoid an unbounded migration
    // rewrite. Any fresh or advanced checkpoint has a different revision domain.
    let prefix = if stored.is_some() {
        NEW_PREFIX
    } else {
        LEGACY_PREFIX
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, &db.keys[32..]);
    let encoded = serde_json::to_vec(&(
        "psios/event-offset-cas/1",
        &db.incarnation,
        owner,
        consumer,
        cursor,
        stored,
    ))?;
    Ok(format!(
        "{prefix}{}",
        URL_SAFE_NO_PAD.encode(hmac::sign(&key, &encoded))
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Get {
    id: String,
    #[serde(default)]
    include_revision: bool,
}

fn lookup(db: &Db<'_>, owner: &str, consumer: &str) -> Result<Option<(String, Option<String>)>> {
    Ok(db
        .c
        .query_row(
            "SELECT cursor,checkpoint_revision FROM event_offsets WHERE owner=?1 AND consumer=?2",
            params![owner, consumer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

pub(crate) fn get(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Get = decode(params)?;
    let current = lookup(db, actor, &p.id)?;
    if !p.include_revision {
        return Ok(json!({"cursor":current.map(|(cursor, _)| cursor)}));
    }
    if let Some((cursor, stored)) = current {
        let revision = revision(db, actor, &p.id, &cursor, stored.as_deref())?;
        Ok(json!({"cursor":cursor,"revision":revision}))
    } else {
        Ok(json!({"cursor":null,"revision":null}))
    }
}

fn owner<'a>(db: &Db<'_>, actor: &'a str, named: Option<&'a str>) -> Result<&'a str> {
    let owner = named.unwrap_or(actor);
    if owner != actor {
        policy::admin(actor)?;
        label(owner)?;
        let principal = db.find(owner)?;
        require(
            principal
                .is_some_and(|doc| doc.kind == "Principal" && doc.body["lifecycle"] == "Revoked"),
            "PermissionDenied",
            "another principal's offsets can be retired only after explicit principal revocation",
        )?;
    }
    Ok(owner)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    owner: Option<String>,
    after: Option<String>,
    limit: Option<u32>,
}

pub(crate) fn list(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: List = decode(params)?;
    let owner = owner(db, actor, p.owner.as_deref())?;
    if let Some(after) = &p.after {
        label(after)?;
    }
    let limit = super::limit(p.limit)?;
    let mut statement = db.c.prepare(
        "SELECT consumer,cursor,checkpoint_revision FROM event_offsets
         WHERE owner=?1 AND consumer>?2 ORDER BY consumer LIMIT ?3",
    )?;
    let mut rows = statement.query(params![
        owner,
        p.after.as_deref().unwrap_or_default(),
        limit + 1
    ])?;
    let mut items = Vec::new();
    let mut next_consumer: Option<String> = None;
    while let Some(row) = rows.next()? {
        if items.len() == limit as usize {
            return Ok(json!({"items":items,"next_consumer":next_consumer}));
        }
        let consumer: String = row.get(0)?;
        let cursor: String = row.get(1)?;
        let stored: Option<String> = row.get(2)?;
        let revision = revision(db, owner, &consumer, &cursor, stored.as_deref())?;
        items.push(json!({"consumer":consumer,"revision":revision}));
        next_consumer = Some(consumer);
    }
    Ok(json!({"items":items,"next_consumer":null}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delete {
    owner: Option<String>,
    consumer: String,
    expected_revision: String,
}

pub(crate) fn delete(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let p: Delete = decode(params)?;
    let owner = owner(db, actor, p.owner.as_deref())?;
    label(&p.consumer)?;
    require(
        valid_expected(&p.expected_revision),
        "InvalidArgument",
        "invalid expected event offset revision",
    )?;
    let Some((cursor, stored)) = lookup(db, owner, &p.consumer)? else {
        return Ok(json!({"deleted":false}));
    };
    require(
        revision(db, owner, &p.consumer, &cursor, stored.as_deref())? == p.expected_revision,
        "Conflict",
        "consumer checkpoint changed",
    )?;
    require(
        db.c.execute(
            "DELETE FROM event_offsets WHERE owner=?1 AND consumer=?2 AND cursor=?3 AND checkpoint_revision IS ?4",
            params![owner, p.consumer, cursor, stored],
        )? == 1,
        "Conflict", "consumer checkpoint changed",
    )?;
    let target = serde_json::to_string(&(owner, &p.consumer))?;
    db.audit(actor, "Event.OffsetDelete", Some(&target), "Deleted")?;
    Ok(json!({"deleted":true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_offset_advances_at_authority_capacity_and_deletion_frees_only_its_slot() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::Config::local(&directory.path().join("authority"));
        crate::Store::init(&config).unwrap();
        let mut store = crate::Store::open(config).unwrap();
        let actor = policy::OWNER;
        store
            .transaction("event-offset-legacy-and-authority-cap", |db| {
                let initial = super::super::list(db, actor, &json!({"kind":"Node"}))?;
                let cursor = &initial["watch_start_cursor"];
                db.c.execute(
                    "INSERT INTO event_offsets(owner,consumer,cursor) VALUES(?1,'legacy',?2)",
                    params![actor, cursor.as_str().unwrap()],
                )?;
                db.c.execute(
                    "WITH RECURSIVE n(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM n WHERE v<?1)
                 INSERT INTO event_offsets(owner,consumer,cursor)
                 SELECT 'legacy-owner-'||v,'reader',?2 FROM n",
                    params![PER_AUTHORITY - 1, cursor.as_str().unwrap()],
                )?;
                let get_legacy =
                    |db: &Db<'_>| get(db, actor, &json!({"id":"legacy","include_revision":true}));
                let legacy = get_legacy(db)?;
                assert!(
                    legacy["revision"]
                        .as_str()
                        .unwrap()
                        .starts_with(LEGACY_PREFIX)
                );
                assert_eq!(
                    get(db, actor, &json!({"id":"legacy"}))?,
                    json!({"cursor":cursor})
                );
                assert_eq!(
                    super::super::ack(db, actor, &json!({"consumer":"full","cursor":cursor}))
                        .unwrap_err()
                        .0
                        .code,
                    "QuotaExceeded"
                );
                super::super::ack(db, actor, &json!({"consumer":"legacy","cursor":cursor}))?;
                assert_eq!(get_legacy(db)?, legacy);
                let refreshed = super::super::list(db, actor, &json!({"kind":"Node"}))?;
                let next = &refreshed["watch_start_cursor"];
                super::super::ack(
                    db,
                    actor,
                    &json!({"consumer":"legacy","previous":cursor,"cursor":next}),
                )?;
                let advanced = get_legacy(db)?;
                assert_eq!(advanced["cursor"], *next);
                assert_ne!(advanced["revision"], legacy["revision"]);
                assert_eq!(
                    delete(
                        db,
                        actor,
                        &json!({"consumer":"legacy","expected_revision":legacy["revision"]})
                    )
                    .unwrap_err()
                    .0
                    .code,
                    "Conflict"
                );
                assert_eq!(
                    delete(
                        db,
                        actor,
                        &json!({"consumer":"legacy","expected_revision":advanced["revision"]})
                    )?,
                    json!({"deleted":true})
                );

                super::super::ack(db, actor, &json!({"consumer":"legacy","cursor":next}))?;
                let recreated = get_legacy(db)?;
                assert_eq!(recreated["cursor"], advanced["cursor"]);
                assert_ne!(recreated["revision"], advanced["revision"]);
                assert_eq!(
                    delete(
                        db,
                        actor,
                        &json!({"consumer":"legacy","expected_revision":advanced["revision"]})
                    )
                    .unwrap_err()
                    .0
                    .code,
                    "Conflict"
                );
                assert_eq!(
                    db.c.query_row("SELECT count(*) FROM event_offsets", [], |r| r
                        .get::<_, i64>(0))?,
                    PER_AUTHORITY
                );
                Ok(())
            })
            .unwrap();
    }
}
