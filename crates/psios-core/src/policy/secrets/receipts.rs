use super::*;

pub(crate) const FORMAT: u32 = crate::local_read_tools::APPLICATION_INSPECT_FORMAT + 1;
pub(crate) const SCHEMA: &str = "
CREATE TABLE secret_mutations(
    actor TEXT NOT NULL,
    lookup_tag BLOB NOT NULL CHECK(length(lookup_tag)=32),
    request_tag BLOB NOT NULL CHECK(length(request_tag)=32),
    secret TEXT NOT NULL,
    revision INTEGER NOT NULL,
    receipt_tag BLOB NOT NULL CHECK(length(receipt_tag)=32),
    PRIMARY KEY(actor,lookup_tag),
    FOREIGN KEY(secret,revision) REFERENCES versions(id,rev)
) STRICT, WITHOUT ROWID;";

pub(super) struct Mutation<'a> {
    pub method: &'static str,
    pub request_key: Option<&'a str>,
    pub target: &'a str,
    pub expected: Option<&'a str>,
    pub value: Option<&'a str>,
}

fn signing_key(keys: &[u8]) -> Result<hmac::Key> {
    let witness = keys
        .get(32..64)
        .ok_or_else(|| err("RecoveryRequired", "Secret receipt key unavailable"))?;
    let witness = hmac::Key::new(hmac::HMAC_SHA256, witness);
    let derived = hmac::sign(&witness, b"psios.secret-mutation-receipts.v1");
    Ok(hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref()))
}

struct Receipt {
    actor: String,
    lookup_tag: Vec<u8>,
    request_tag: Vec<u8>,
    secret: String,
    revision: i64,
    receipt_tag: Vec<u8>,
}

impl Receipt {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            actor: row.get("actor")?,
            lookup_tag: row.get("lookup_tag")?,
            request_tag: row.get("request_tag")?,
            secret: row.get("secret")?,
            revision: row.get("revision")?,
            receipt_tag: row.get("receipt_tag")?,
        })
    }

    fn input(actor: &str, lookup_tag: &[u8], request_tag: &[u8], secret: &Doc) -> Vec<u8> {
        let mut input = canonical(&json!(["receipt", actor, secret.reference()]));
        input.push(0);
        input.extend_from_slice(lookup_tag);
        input.extend_from_slice(request_tag);
        input
    }

    fn verified(&self, conn: &Connection, key: &hmac::Key) -> Result<Doc> {
        let invalid = || err("RecoveryRequired", "Secret mutation receipt is invalid");
        let secret = conn
            .prepare_cached("SELECT * FROM versions WHERE id=?1 AND rev=?2 AND kind='Secret'")?
            .query_row(params![self.secret, self.revision], row)
            .optional()?
            .ok_or_else(invalid)?;
        require(
            secret.owner == self.actor || self.actor == OWNER,
            "RecoveryRequired",
            "Secret mutation receipt owner disagrees with its result",
        )?;
        let input = Self::input(&self.actor, &self.lookup_tag, &self.request_tag, &secret);
        hmac::verify(key, &input, &self.receipt_tag).map_err(|_| invalid())?;
        Ok(secret)
    }
}

impl Mutation<'_> {
    pub(super) fn execute(
        &self,
        db: &Db<'_>,
        actor: &str,
        effect: impl FnOnce() -> Result<Doc>,
    ) -> Result<Value> {
        let Some(request_key) = self.request_key else {
            let secret = effect()?;
            db.audit(actor, self.method, Some(&secret.id), "Allowed")?;
            return Ok(secret.reference());
        };
        label(request_key)?;
        let key = signing_key(db.keys)?;
        let lookup = hmac::sign(&key, &canonical(&json!(["lookup", actor, request_key])));
        let existing =
            db.c.prepare_cached("SELECT * FROM secret_mutations WHERE actor=?1 AND lookup_tag=?2")?
                .query_row(params![actor, lookup.as_ref()], Receipt::from_row)
                .optional()?;

        let mut request = Zeroizing::new(canonical(&json!([
            "request",
            actor,
            self.method,
            self.target,
            self.expected
        ])));
        request.push(0);
        request.extend_from_slice(lookup.as_ref());
        if let Some(value) = self.value {
            request.extend_from_slice(value.as_bytes());
        }
        if let Some(existing) = existing {
            let secret = existing.verified(db.c, &key)?;
            hmac::verify(&key, &request, &existing.request_tag).map_err(|_| {
                err(
                    "IdempotencyConflict",
                    "Secret mutation key belongs to a different request",
                )
            })?;
            return Ok(secret.reference());
        }

        let request_tag = hmac::sign(&key, &request);
        let secret = effect()?;
        let receipt = Receipt::input(actor, lookup.as_ref(), request_tag.as_ref(), &secret);
        let receipt_tag = hmac::sign(&key, &receipt);
        db.c.execute(
            "INSERT INTO secret_mutations(actor,lookup_tag,request_tag,secret,revision,receipt_tag)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                actor,
                lookup.as_ref(),
                request_tag.as_ref(),
                secret.id,
                secret.rev,
                receipt_tag.as_ref()
            ],
        )?;
        db.audit(actor, self.method, Some(&secret.id), "Allowed")?;
        Ok(secret.reference())
    }
}

pub(crate) fn verify(conn: &Connection, keys: &[u8]) -> Result<()> {
    let key = signing_key(keys)?;
    let mut statement = conn.prepare("SELECT * FROM secret_mutations")?;
    let receipts = statement.query_map([], Receipt::from_row)?;
    for receipt in receipts {
        receipt?.verified(conn, &key)?;
    }
    Ok(())
}
