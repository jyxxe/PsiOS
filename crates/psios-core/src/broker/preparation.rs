//! A prepared request reserves private input custody, not a Task or a budget.
use super::dependencies::INPUT_KIND;
use crate::{
    Result,
    db::{Db, Doc},
    err, now,
    policy::{self, deadline::Clock},
    require,
};
use chrono::{DateTime, SecondsFormat, Utc};
use psios_api::{canonical, digest, sha256};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = 96;
pub(crate) const SCHEMA: &str = include_str!("preparation.sql");
const PREFIX: &str = "input-prepared-";
const TTL_MS: i64 = 600_000;
const COLLECT_LIMIT: u32 = 16;
const NODE_REQUESTS: i64 = 1024;
const OWNER_REQUESTS: i64 = 128;
const NODE_BYTES: i64 = 64 * 1024 * 1024;
const OWNER_BYTES: i64 = 8 * 1024 * 1024;

pub(super) fn reference(actor: &str, kind: &str, body: &Value) -> Value {
    let identity = sha256(&canonical(&json!([
        "psios-task-prepared-input-v1",
        actor,
        kind,
        body
    ])));
    json!({
        "resource_id":format!("{PREFIX}{}", &identity[7..]),
        "version":"r1",
        "digest":sha256(&canonical(body))
    })
}

pub(crate) fn is_prepared(reference: &Value) -> bool {
    reference["resource_id"]
        .as_str()
        .is_some_and(is_prepared_id)
}

fn is_prepared_id(id: &str) -> bool {
    id.starts_with(PREFIX)
}

fn invalid() -> crate::Error {
    err(
        "RecoveryRequired",
        "task preparation has inconsistent durable custody",
    )
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name].as_str().ok_or_else(invalid)
}

fn check_request(actor: &str, origin: &str, request: &Value) -> Result<()> {
    let audience = &request["origin_ref"];
    let expected_kind = if actor == policy::OWNER {
        "HumanRequest"
    } else {
        "Service"
    };
    require(
        canonical(audience) == origin.as_bytes()
            && audience["authority_id"] == actor
            && audience["origin_type"] == expected_kind
            && audience["origin_id"] == request["cancel_scope_id"]
            && is_prepared(&request["input_ref"])
            && request["request_digest"] == digest("task-submit", "request_digest", request)?,
        "InvalidArgument",
        "request differs from its scoped preparation",
    )
}

struct Sample {
    clock: Clock,
    utc_ms: i64,
}

impl Sample {
    fn now() -> Result<Self> {
        Ok(Self {
            clock: Clock::sample()?,
            utc_ms: Utc::now().timestamp_millis(),
        })
    }
}

struct Preparation {
    request: Value,
    input_id: String,
    incarnation: String,
    boot_id: String,
    time_ns: String,
    deadline_boot_ms: i64,
    expires_utc_ms: i64,
}

fn read_preparation(row: &Row<'_>) -> rusqlite::Result<Preparation> {
    Ok(Preparation {
        request: row.get("request")?,
        input_id: row.get("input_id")?,
        incarnation: row.get("incarnation")?,
        boot_id: row.get("boot_id")?,
        time_ns: row.get("time_ns")?,
        deadline_boot_ms: row.get("deadline_boot_ms")?,
        expires_utc_ms: row.get("expires_utc_ms")?,
    })
}

fn get(db: &Db<'_>, actor: &str, origin: &str, key: &str) -> Result<Option<Preparation>> {
    Ok(db
        .c
        .prepare_cached("SELECT * FROM task_preparations WHERE owner=?1 AND origin=?2 AND key=?3")?
        .query_row(params![actor, origin, key], read_preparation)
        .optional()?)
}

impl Preparation {
    fn live(&self, db: &Db<'_>, sample: &Sample) -> bool {
        self.incarnation == db.incarnation
            && self.boot_id == sample.clock.boot.boot_id
            && self.time_ns == sample.clock.time_ns
            && self.deadline_boot_ms > sample.clock.boot.millis
            && self.expires_utc_ms > sample.utc_ms
    }

    fn exact(&self, request: &Value) -> Result<()> {
        require(
            self.request == *request,
            "IdempotencyConflict",
            "preparation key is already bound to a different request",
        )
    }
}

fn input(conn: &Connection, actor: &str, id: &str) -> Result<Option<Doc>> {
    let doc = conn
        .prepare_cached(
            "SELECT kind,body,created_at FROM task_prepared_inputs WHERE owner=?1 AND id=?2",
        )?
        .query_row(params![actor, id], |row| {
            let created: String = row.get(2)?;
            Ok(Doc {
                id: id.to_owned(),
                rev: 1,
                generation: 1,
                kind: row.get(0)?,
                owner: actor.to_owned(),
                spec: json!({}),
                body: row.get(1)?,
                stamp: created.clone(),
                created,
            })
        })
        .optional()?;
    if let Some(doc) = &doc {
        require(
            matches!(doc.kind.as_str(), "Input" | INPUT_KIND)
                && reference(actor, &doc.kind, &doc.body) == doc.reference()
                && DateTime::parse_from_rfc3339(&doc.created).is_ok(),
            "RecoveryRequired",
            "staged Task input disagrees with its fixed reference",
        )?;
    }
    Ok(doc)
}

fn document(conn: &Connection, actor: &str, preparation: &Preparation) -> Result<Doc> {
    let doc = input(conn, actor, &preparation.input_id)?.ok_or_else(invalid)?;
    require(
        doc.reference() == preparation.request["input_ref"],
        "RecoveryRequired",
        "prepared request disagrees with its staged input",
    )?;
    Ok(doc)
}

fn live_request(
    db: &Db<'_>,
    actor: &str,
    origin: &str,
    request: &Value,
    sample: &Sample,
) -> Result<Option<Preparation>> {
    check_request(actor, origin, request)?;
    let Some(preparation) = get(db, actor, origin, field(request, "idempotency_key")?)? else {
        return Ok(None);
    };
    preparation.exact(request)?;
    Ok(preparation.live(db, sample).then_some(preparation))
}

pub(super) fn load(db: &Db<'_>, actor: &str, origin: &str, request: &Value) -> Result<Option<Doc>> {
    live_request(db, actor, origin, request, &Sample::now()?)?
        .map(|preparation| document(db.c, actor, &preparation))
        .transpose()
}

pub(super) fn window(
    db: &Db<'_>,
    actor: &str,
    origin: &str,
    request: &Value,
) -> Result<Option<String>> {
    let sample = Sample::now()?;
    live_request(db, actor, origin, request, &sample)?
        .map(|preparation| {
            let left = preparation.deadline_boot_ms - sample.clock.boot.millis;
            let until = preparation
                .expires_utc_ms
                .min(sample.utc_ms.saturating_add(left));
            DateTime::<Utc>::from_timestamp_millis(until)
                .map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true))
                .ok_or_else(invalid)
        })
        .transpose()
}

pub(crate) fn preview(db: &Db<'_>, actor: &str, id: &str) -> Result<Option<Doc>> {
    if !is_prepared_id(id) {
        return Ok(None);
    }
    let sample = Sample::now()?;
    let eligible =
        db.c.prepare_cached(
            "SELECT request FROM task_preparations
         WHERE owner=?1 AND input_id=?2 AND incarnation=?3 AND boot_id=?4 AND time_ns=?5
             AND deadline_boot_ms>?6 AND expires_utc_ms>?7",
        )?
        .query_map(
            params![
                actor,
                id,
                db.incarnation,
                sample.clock.boot.boot_id,
                sample.clock.time_ns,
                sample.clock.boot.millis,
                sample.utc_ms
            ],
            |row| row.get::<_, Value>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for request in eligible {
        let delegation = field(&request["delegation_ref"], "resource_id")?;
        let tool = field(&request, "task_type")?;
        let exact = policy::delegation(db, actor, delegation, "task.submit")
            .and_then(|_| policy::delegation(db, actor, delegation, tool));
        match exact {
            Ok(live) if live.reference() == request["delegation_ref"] => {
                let doc = input(db.c, actor, id)?.ok_or_else(invalid)?;
                require(
                    doc.reference() == request["input_ref"],
                    "RecoveryRequired",
                    "preview request disagrees with its staged input",
                )?;
                return Ok(Some(doc));
            }
            Ok(_) => {}
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn timestamp_ms(raw: &str) -> Result<i64> {
    DateTime::parse_from_rfc3339(raw)
        .map(|at| at.timestamp_millis())
        .map_err(|_| err("InvalidArgument", "preparation deadline is not RFC 3339"))
}

fn deadlines(
    db: &Db<'_>,
    delegation: &Doc,
    request: &Value,
    sample: &Sample,
) -> Result<(i64, i64)> {
    let grant = db.of(field(&delegation.body, "grant_id")?, "Grant")?;
    let mut until = sample
        .utc_ms
        .saturating_add(TTL_MS)
        .min(timestamp_ms(field(&delegation.body, "valid_until")?)?)
        .min(timestamp_ms(field(&grant.spec, "valid_until")?)?);
    if let Some(at) = request["deadline_at"].as_str() {
        until = until.min(timestamp_ms(at)?);
    }
    let mut deadline = sample
        .clock
        .boot
        .millis
        .saturating_add(until.saturating_sub(sample.utc_ms));
    for source in [&delegation.id, &grant.id] {
        if let Some((namespace, upper)) = db.c.query_row(
            "SELECT time_ns,deadline_boot_ms FROM authorization_deadlines WHERE resource=?1 AND boot_id=?2",
            params![source, sample.clock.boot.boot_id],
            |r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?)),
        ).optional()? {
            require(namespace == sample.clock.time_ns, "DeadlineExceeded", "preparation authorization belongs to another time namespace")?;
            deadline = deadline.min(upper);
        }
    }
    require(
        deadline > sample.clock.boot.millis,
        "DeadlineExceeded",
        "preparation has no remaining authorization window",
    )?;
    until = until.min(
        sample
            .utc_ms
            .saturating_add(deadline - sample.clock.boot.millis),
    );
    Ok((until, deadline))
}

fn require_transaction(db: &Db<'_>) -> Result<()> {
    require(
        !db.c.is_autocommit(),
        "RecoveryRequired",
        "prepared Task input custody requires an authority transaction",
    )
}

fn capacity(db: &Db<'_>, actor: &str, bytes: i64) -> Result<()> {
    let (node, owner, node_bytes, owner_bytes): (i64, i64, i64, i64) = db.c.query_row(
        "SELECT (SELECT count(*) FROM task_preparations),
                (SELECT count(*) FROM task_preparations WHERE owner=?1),
                (SELECT coalesce(sum(bytes),0) FROM task_prepared_inputs),
                (SELECT coalesce(sum(bytes),0) FROM task_prepared_inputs WHERE owner=?1)",
        [actor],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    if node < NODE_REQUESTS
        && owner < OWNER_REQUESTS
        && node_bytes <= NODE_BYTES.saturating_sub(bytes)
        && owner_bytes <= OWNER_BYTES.saturating_sub(bytes)
    {
        return Ok(());
    }
    let mut error = err("QuotaExceeded", "pending Task preparation quota exceeded");
    error.0.retryable = true;
    error.0.retry_condition = "SameRequestAfterBackoff".into();
    Err(error)
}

pub(super) fn stage(
    db: &Db<'_>,
    actor: &str,
    origin: &str,
    request: &Value,
    kind: &str,
    body: &Value,
    delegation: &Doc,
) -> Result<()> {
    require_transaction(db)?;
    check_request(actor, origin, request)?;
    let input_ref = &request["input_ref"];
    require(
        matches!(kind, "Input" | INPUT_KIND)
            && *input_ref == reference(actor, kind, body)
            && delegation.kind == "Delegation"
            && delegation.owner == actor
            && delegation.reference() == request["delegation_ref"]
            && delegation.body["budget_ref"] == request["budget_ref"],
        "RecoveryRequired",
        "preparation disagrees with the authorized request",
    )?;
    let key = field(request, "idempotency_key")?;
    let id = field(input_ref, "resource_id")?;
    let sample = Sample::now()?;
    if let Some(old) = get(db, actor, origin, key)? {
        old.exact(request)?;
        if old.live(db, &sample) {
            document(db.c, actor, &old)?;
            return Ok(());
        }
        remove(db, actor, origin, key, &old.input_id)?;
    }
    let (expires, deadline) = deadlines(db, delegation, request, &sample)?;
    let existing = input(db.c, actor, id)?;
    let encoded = if let Some(existing) = &existing {
        require(
            existing.kind == kind
                && existing.reference() == *input_ref
                && canonical(&existing.body) == canonical(body),
            "RecoveryRequired",
            "prepared input identity collision",
        )?;
        None
    } else {
        Some(serde_json::to_string(body)?)
    };
    let bytes = encoded.as_ref().map_or(0, |body| body.len() as i64);
    capacity(db, actor, bytes)?;
    db.cfg.check_admission_space()?;
    if let Some(encoded) = encoded {
        db.c.execute(
            "INSERT INTO task_prepared_inputs(id,owner,kind,body,bytes,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![id,actor,kind,encoded,bytes,now()],
        )?;
    }
    db.c.execute(
        "INSERT INTO task_preparations(owner,origin,key,request,input_id,incarnation,boot_id,time_ns,
            issued_boot_ms,deadline_boot_ms,issued_utc_ms,expires_utc_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![actor,origin,key,request,id,db.incarnation,sample.clock.boot.boot_id,sample.clock.time_ns,
                sample.clock.boot.millis,deadline,sample.utc_ms,expires],
    )?;
    Ok(())
}

pub(super) fn promote(db: &Db<'_>, input: &Doc) -> Result<Doc> {
    require_transaction(db)?;
    let fixed = input.reference();
    require(
        matches!(input.kind.as_str(), "Input" | INPUT_KIND)
            && fixed == reference(&input.owner, &input.kind, &input.body),
        "RecoveryRequired",
        "prepared input cannot be promoted outside its fixed identity",
    )?;
    if let Some(durable) = db.find(&input.id)? {
        require(
            durable.kind == input.kind
                && durable.owner == input.owner
                && durable.reference() == fixed
                && canonical(&durable.body) == canonical(&input.body),
            "RecoveryRequired",
            "prepared input collides with a different durable resource",
        )?;
        return Ok(durable);
    }
    policy::check_input_admission(db, &input.owner, canonical(&input.body).len())?;
    let durable = db.create(
        &input.kind,
        &input.id,
        &input.owner,
        input.spec.clone(),
        input.body.clone(),
    )?;
    require(
        durable.reference() == fixed,
        "RecoveryRequired",
        "promoted input changed its fixed reference",
    )?;
    Ok(durable)
}

fn remove(db: &Db<'_>, actor: &str, origin: &str, key: &str, id: &str) -> Result<()> {
    require(
        db.c.execute(
            "DELETE FROM task_preparations WHERE owner=?1 AND origin=?2 AND key=?3 AND input_id=?4",
            params![actor, origin, key, id],
        )? == 1,
        "RecoveryRequired",
        "task preparation lost its exact key before release",
    )?;
    db.c.execute(
        "DELETE FROM task_prepared_inputs
         WHERE owner=?1 AND id=?2 AND NOT EXISTS(
            SELECT 1 FROM task_preparations WHERE owner=?1 AND input_id=?2)",
        params![actor, id],
    )?;
    Ok(())
}

pub(super) fn consume(db: &Db<'_>, actor: &str, origin: &str, request: &Value) -> Result<()> {
    require_transaction(db)?;
    let sample = Sample::now()?;
    let preparation = live_request(db, actor, origin, request, &sample)?.ok_or_else(|| {
        err(
            "DeadlineExceeded",
            "Task preparation expired before transaction completion",
        )
    })?;
    remove(
        db,
        actor,
        origin,
        field(request, "idempotency_key")?,
        &preparation.input_id,
    )
}

pub(super) fn cancel(
    db: &Db<'_>,
    actor: &str,
    origin: &str,
    key: &str,
    digest: &str,
) -> Result<()> {
    require_transaction(db)?;
    let Some(preparation) = get(db, actor, origin, key)? else {
        return Ok(());
    };
    if preparation.live(db, &Sample::now()?) {
        require(
            preparation.request["request_digest"] == digest,
            "IdempotencyConflict",
            "cancellation differs from the current Task preparation",
        )?;
    }
    remove(db, actor, origin, key, &preparation.input_id)
}

pub(crate) fn collect(db: &Db<'_>) -> Result<u32> {
    require_transaction(db)?;
    let sample = Sample::now()?;
    let expired = db.c.prepare_cached(
        "SELECT owner,origin,key,input_id FROM task_preparations
         WHERE incarnation<>?1 OR boot_id<>?2 OR time_ns<>?3 OR deadline_boot_ms<=?4 OR expires_utc_ms<=?5
         ORDER BY expires_utc_ms,owner,origin,key LIMIT ?6",
    )?
    .query_map(
        params![db.incarnation,sample.clock.boot.boot_id,sample.clock.time_ns,
                sample.clock.boot.millis,sample.utc_ms,COLLECT_LIMIT],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)),
    )?
    .collect::<rusqlite::Result<Vec<_>>>()?;
    for (actor, origin, key, id) in &expired {
        remove(db, actor, origin, key, id)?;
    }
    Ok(expired.len() as u32)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    verify_records(conn).map_err(|error| {
        if error.0.code == "RecoveryRequired" {
            error
        } else {
            err(
                "RecoveryRequired",
                format!("prepared Task input verification: {}", error.0.message),
            )
        }
    })
}

fn verify_records(conn: &Connection) -> Result<()> {
    let bad: bool = conn.query_row(
        "SELECT (SELECT count(*) FROM task_preparations)>?1
            OR (SELECT coalesce(sum(bytes),0) FROM task_prepared_inputs)>?2
            OR EXISTS(SELECT 1 FROM task_preparations GROUP BY owner HAVING count(*)>?3)
            OR EXISTS(SELECT 1 FROM task_prepared_inputs GROUP BY owner HAVING sum(bytes)>?4)
            OR EXISTS(SELECT 1 FROM task_prepared_inputs i WHERE NOT EXISTS(
              SELECT 1 FROM task_preparations p WHERE p.owner=i.owner AND p.input_id=i.id))",
        params![NODE_REQUESTS, NODE_BYTES, OWNER_REQUESTS, OWNER_BYTES],
        |r| r.get(0),
    )?;
    require(
        !bad,
        "RecoveryRequired",
        "prepared Task input custody exceeds its durable quota or has orphaned content",
    )?;
    let mut statement = conn.prepare("SELECT * FROM task_preparations")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let actor: String = row.get("owner")?;
        let origin: String = row.get("origin")?;
        let key: String = row.get("key")?;
        let preparation = read_preparation(row)?;
        check_request(&actor, &origin, &preparation.request)?;
        document(conn, &actor, &preparation)?;
        require(
            preparation.request["idempotency_key"] == key
                && uuid::Uuid::parse_str(&preparation.boot_id).is_ok()
                && !preparation.incarnation.is_empty()
                && !preparation.time_ns.is_empty(),
            "RecoveryRequired",
            "prepared Task request is outside its stored key or clock",
        )?;
    }
    Ok(())
}
