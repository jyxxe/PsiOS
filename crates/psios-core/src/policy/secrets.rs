use super::*;
use psios_api::MAX_SECRET_BYTES;
use ring::{
    aead, hmac,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension};
use zeroize::Zeroizing;

pub(crate) mod receipts;
use receipts::Mutation;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretPut {
    name: String,
    value: String,
    idempotency_key: Option<String>,
}
fn vault(keys: &[u8]) -> Result<aead::LessSafeKey> {
    let material = keys
        .get(..32)
        .ok_or_else(|| err("RecoveryRequired", "vault key unavailable"))?;
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, material)
            .map_err(|_| err("RecoveryRequired", "vault key unavailable"))?,
    ))
}
pub fn secret_put(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let s: SecretPut = decode(p)?;
    label(&s.name)?;
    let value = Zeroizing::new(s.value);
    require(
        (1..=MAX_SECRET_BYTES).contains(&value.len()),
        "InvalidArgument",
        "secret size outside limit",
    )?;
    Mutation {
        method: "Secret.Put",
        request_key: s.idempotency_key.as_deref(),
        target: &s.name,
        expected: None,
        value: Some(&value),
    }
    .execute(db, actor, || {
        let count: u32 = db.c.query_row(
            "SELECT count(*) FROM current WHERE kind='Secret' AND owner=?1 AND body->>'lifecycle'='Active'",
            [actor],
            |r| r.get(0),
        )?;
        require(count < 256, "QuotaExceeded", "active secret quota exceeded")?;
        let secret = db.record(
            "Secret",
            actor,
            json!({"name": s.name, "lifecycle": "Active", "use": "secret.mac", "key_version": 1}),
        )?;
        seal_secret(db, &secret, &value)?;
        Ok(secret)
    })
}
fn secret_aad(d: &Doc) -> Vec<u8> {
    match d.body["key_version"].as_u64() {
        Some(version) => canonical(&json!([d.id, version])),
        None => d.id.as_bytes().to_vec(), // prior on-disk ciphertext uses the stable ID alone
    }
}
fn seal_secret(db: &Db<'_>, d: &Doc, value: &str) -> Result<()> {
    let mut nonce = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| err("InternalError", "entropy unavailable"))?;
    let mut bytes = Zeroizing::new(value.as_bytes().to_vec());
    vault(db.keys)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(secret_aad(d)),
            &mut *bytes,
        )
        .map_err(|_| err("InternalError", "vault encryption failed"))?;
    db.c.execute(
        "INSERT INTO secrets VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET nonce=excluded.nonce,ciphertext=excluded.ciphertext",
        params![d.id, nonce.as_slice(), bytes.as_slice()],
    )?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rotate {
    id: String,
    expected_resource_version: String,
    value: String,
    idempotency_key: Option<String>,
}
pub fn secret_rotate(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let v: Rotate = decode(p)?;
    let value = Zeroizing::new(v.value);
    require(
        (1..=MAX_SECRET_BYTES).contains(&value.len()),
        "InvalidArgument",
        "secret size outside limit",
    )?;
    Mutation {
        method: "Secret.Rotate",
        request_key: v.idempotency_key.as_deref(),
        target: &v.id,
        expected: Some(&v.expected_resource_version),
        value: Some(&value),
    }
    .execute(db, actor, || {
        let secret = db.of(&v.id, "Secret")?;
        owned(actor, &secret)?;
        secret.cas(&v.expected_resource_version)?;
        require(
            secret.body["lifecycle"] == "Active",
            "PreconditionFailed",
            "revoked secret cannot be rotated",
        )?;
        let mut body = secret.body.clone();
        body["key_version"] = json!(body["key_version"].as_u64().unwrap_or(1) + 1);
        let rotated = db.status(&secret, body)?;
        seal_secret(db, &rotated, &value)?;
        epoch(db)?;
        Ok(rotated)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoke {
    id: String,
    expected_resource_version: String,
    idempotency_key: Option<String>,
}
pub fn secret_revoke(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let v: Revoke = decode(p)?;
    Mutation {
        method: "Secret.Revoke",
        request_key: v.idempotency_key.as_deref(),
        target: &v.id,
        expected: Some(&v.expected_resource_version),
        value: None,
    }
    .execute(db, actor, || {
        let secret = db.of(&v.id, "Secret")?;
        owned(actor, &secret)?;
        secret.cas(&v.expected_resource_version)?;
        db.c.execute("DELETE FROM secrets WHERE id=?1", [&secret.id])?;
        let mut body = secret.body.clone();
        body["lifecycle"] = json!("Revoked");
        epoch(db)?;
        db.status(&secret, body)
    })
}
fn checked_payload(conn: &Connection, id: &str) -> Result<(Vec<u8>, Zeroizing<Vec<u8>>)> {
    let tag_bytes = aead::AES_256_GCM.tag_len();
    let pair = conn
        .prepare_cached(
            "SELECT nonce,ciphertext FROM secrets WHERE id=?1
         AND length(nonce)=12 AND length(ciphertext) BETWEEN ?2 AND ?3",
        )?
        .query_row(
            params![
                id,
                (tag_bytes + 1) as i64,
                (tag_bytes + MAX_SECRET_BYTES) as i64
            ],
            |row| Ok((row.get(0)?, Zeroizing::new(row.get(1)?))),
        )
        .optional()?;
    pair.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "active Secret has no valid sealed payload",
        )
    })
}

fn open_secret<'a>(
    key: &aead::LessSafeKey,
    secret: &Doc,
    nonce: &[u8],
    bytes: &'a mut [u8],
) -> Result<&'a mut [u8]> {
    let nonce = aead::Nonce::try_assume_unique_for_key(nonce)
        .map_err(|_| err("RecoveryRequired", "invalid vault nonce"))?;
    key.open_in_place(nonce, aead::Aad::from(secret_aad(secret)), bytes)
        .map_err(|_| err("RecoveryRequired", "vault authentication failed"))
}

pub(crate) fn verify(conn: &Connection, keys: &[u8]) -> Result<()> {
    let orphan: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM secrets s LEFT JOIN current c
         ON c.id=s.id AND c.kind='Secret' WHERE c.id IS NULL)",
        [],
        |row| row.get(0),
    )?;
    require(
        !orphan,
        "RecoveryRequired",
        "vault payload has no Secret reference",
    )?;
    let key = vault(keys)?;
    let mut statement = conn.prepare(
        "SELECT c.*, s.id IS NOT NULL AS vault_has_payload FROM current c
         LEFT JOIN secrets s ON s.id=c.id WHERE c.kind='Secret'",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let secret = crate::db::row(row)?;
        let present: bool = row.get("vault_has_payload")?;
        match secret.body["lifecycle"].as_str() {
            Some("Active") => {
                let (nonce, mut bytes) = checked_payload(conn, &secret.id)?;
                open_secret(&key, &secret, &nonce, &mut bytes)?;
            }
            Some("Revoked") if !present => {}
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "Secret lifecycle disagrees with vault",
                ));
            }
        }
    }
    Ok(())
}

pub fn mac(db: &Db<'_>, actor: &str, secret: &str, message: &str) -> Result<Value> {
    let d = db.of(secret, "Secret")?;
    owned(actor, &d)?;
    require(
        d.body["lifecycle"] == "Active",
        "PermissionDenied",
        "secret revoked",
    )?;
    let (nonce, mut bytes) = checked_payload(db.c, secret)?;
    let plain = open_secret(&vault(db.keys)?, &d, &nonce, &mut bytes)?;
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, plain),
        message.as_bytes(),
    );
    use base64::Engine;
    Ok(
        json!({"algorithm": "HMAC-SHA256", "tag": base64::engine::general_purpose::STANDARD.encode(tag)}),
    )
}
