use super::*;
use ring::{
    aead, hmac,
    rand::{SecureRandom, SystemRandom},
};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretPut {
    name: String,
    value: String,
}
fn vault(db: &Db<'_>) -> Result<aead::LessSafeKey> {
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &db.keys[..32])
            .map_err(|_| err("RecoveryRequired", "vault key unavailable"))?,
    ))
}
pub fn secret_put(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let s: SecretPut = decode(p)?;
    label(&s.name)?;
    require(
        (1..=8192).contains(&s.value.len()),
        "InvalidArgument",
        "secret size outside limit",
    )?;
    let count:u32=db.c.query_row("SELECT count(*) FROM current WHERE kind='Secret' AND owner=?1 AND body->>'lifecycle'='Active'",[actor],|r|r.get(0))?;
    require(count < 256, "QuotaExceeded", "active secret quota exceeded")?;
    let d = db.record(
        "Secret",
        actor,
        json!({"name": s.name, "lifecycle": "Active", "use": "secret.mac","key_version":1}),
    )?;
    seal_secret(db, &d, s.value)?;
    Ok(d.reference())
}
fn secret_aad(d: &Doc) -> Vec<u8> {
    match d.body["key_version"].as_u64() {
        Some(version) => canonical(&json!([d.id, version])),
        None => d.id.as_bytes().to_vec(), // prior on-disk ciphertext uses the stable ID alone
    }
}
fn seal_secret(db: &Db<'_>, d: &Doc, value: String) -> Result<()> {
    let mut nonce = [0u8; 12];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| err("InternalError", "entropy unavailable"))?;
    let mut bytes = Zeroizing::new(value.into_bytes());
    vault(db)?
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
}
pub fn secret_rotate(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let v: Rotate = decode(p)?;
    let d = db.of(&v.id, "Secret")?;
    owned(actor, &d)?;
    d.cas(&v.expected_resource_version)?;
    require(
        d.body["lifecycle"] == "Active",
        "PreconditionFailed",
        "revoked secret cannot be rotated",
    )?;
    require(
        (1..=8192).contains(&v.value.len()),
        "InvalidArgument",
        "secret size outside limit",
    )?;
    let mut b = d.body.clone();
    b["key_version"] = json!(b["key_version"].as_u64().unwrap_or(1) + 1);
    let rotated = db.status(&d, b)?;
    seal_secret(db, &rotated, v.value)?;
    epoch(db)?;
    Ok(rotated.reference())
}
pub fn secret_revoke(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    can(db, actor, "secret.manage")?;
    let p: Conditional = decode(p)?;
    let d = db.of(&p.id, "Secret")?;
    owned(actor, &d)?;
    d.cas(&p.expected_resource_version)?;
    db.c.execute("DELETE FROM secrets WHERE id=?1", [&d.id])?;
    let mut b = d.body.clone();
    b["lifecycle"] = json!("Revoked");
    epoch(db)?;
    Ok(db.status(&d, b)?.reference())
}
pub fn mac(db: &Db<'_>, actor: &str, secret: &str, message: &str) -> Result<Value> {
    let d = db.of(secret, "Secret")?;
    owned(actor, &d)?;
    require(
        d.body["lifecycle"] == "Active",
        "PermissionDenied",
        "secret revoked",
    )?;
    let (n, c): (Vec<u8>, Vec<u8>) = db.c.query_row(
        "SELECT nonce,ciphertext FROM secrets WHERE id=?1",
        [secret],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut bytes = Zeroizing::new(c);
    let nonce = aead::Nonce::try_assume_unique_for_key(&n)
        .map_err(|_| err("RecoveryRequired", "invalid vault nonce"))?;
    let plain = vault(db)?
        .open_in_place(nonce, aead::Aad::from(secret_aad(&d)), &mut bytes)
        .map_err(|_| err("RecoveryRequired", "vault authentication failed"))?;
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, plain),
        message.as_bytes(),
    );
    use base64::Engine;
    Ok(
        json!({"algorithm": "HMAC-SHA256", "tag": base64::engine::general_purpose::STANDARD.encode(tag)}),
    )
}
