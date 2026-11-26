//! Detached Backup provenance from one stopped authority to one named recipient.
use super::{
    detached::{expected_digest, open_detached},
    intake::{Intake, KIND},
};
use crate::{Config, Result, db, decode, err, policy, require};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use psios_api::{ResourceRef, canonical, sha256};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{Metadata, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use zeroize::Zeroizing;

pub(crate) const FORMAT: u32 = crate::agent::session_inputs::QUEUED_FORMAT + 1;
pub(crate) const SCHEMA: &str = "
CREATE TABLE signed_backup_transfers(
    intake_id TEXT PRIMARY KEY,
    public_key BLOB NOT NULL CHECK(length(public_key)=32)
) STRICT, WITHOUT ROWID;";
const DOMAIN: &[u8] = b"psios/backup-transfer/1\n";
const MAX_SIGNING_KEY_BYTES: u64 = 8192;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Statement {
    format: u32,
    source_authority_id: String,
    source_recovery_incarnation: String,
    recipient_authority_id: String,
    backup_ref: ResourceRef,
    source_volume_ref: ResourceRef,
    data_incarnation: String,
    manifest_digest: String,
    signer_fingerprint: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Attestation {
    attestation: Statement,
    signature_base64: String,
}

pub(super) struct ReceiverAdmission {
    pub(super) envelope: Attestation,
    public_key: [u8; 32],
}

fn exact_id(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        uuid::Uuid::parse_str(suffix).is_ok_and(|id| !id.is_nil() && id.to_string() == suffix)
    })
}

fn version_number(value: &str) -> bool {
    value.strip_prefix('r').is_some_and(|suffix| {
        suffix
            .parse::<u64>()
            .is_ok_and(|num| num > 0 && num.to_string() == suffix)
    })
}

fn private_key_metadata(meta: &Metadata, secret: bool) -> bool {
    let uid = rustix::process::geteuid().as_raw();
    meta.is_file()
        && meta.nlink() == 1
        && if secret {
            meta.uid() == uid && meta.mode() & 0o777 == 0o600
        } else {
            (meta.uid() == uid || meta.uid() == 0) && meta.mode() & 0o022 == 0
        }
}

fn key_bytes(path: &Path, secret: bool, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)
        .map_err(|_| err("PermissionDenied", "cannot open backup transfer key safely"))?;
    let before = file.metadata()?;
    require(
        private_key_metadata(&before, secret),
        "PermissionDenied",
        "backup transfer key file ownership, type, link count or permissions are unsafe",
    )?;
    require(
        (1..=max).contains(&before.len()),
        "InvalidArgument",
        "invalid backup transfer key file size",
    )?;
    let mut bytes = Zeroizing::new(Vec::new());
    (&mut file).take(max + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    require(
        private_key_metadata(&after, secret)
            && bytes.len() as u64 == before.len()
            && after.len() == before.len()
            && after.mtime() == before.mtime()
            && after.mtime_nsec() == before.mtime_nsec()
            && after.ctime() == before.ctime()
            && after.ctime_nsec() == before.ctime_nsec(),
        "Conflict",
        "backup transfer key changed during the read",
    )?;
    Ok(bytes)
}

impl Statement {
    fn validate(&self) -> Result<()> {
        require(
            self.format == 1
                && exact_id(&self.source_authority_id, "node-")
                && exact_id(&self.source_recovery_incarnation, "incarnation-")
                && exact_id(&self.recipient_authority_id, "node-")
                && self.source_authority_id != self.recipient_authority_id
                && exact_id(&self.backup_ref.resource_id, "backup-op-")
                && self.backup_ref.version == "r1"
                && expected_digest(&self.backup_ref.digest)
                && exact_id(&self.source_volume_ref.resource_id, "volume-op-")
                && version_number(&self.source_volume_ref.version)
                && expected_digest(&self.source_volume_ref.digest)
                && exact_id(&self.data_incarnation, "data-op-")
                && expected_digest(&self.manifest_digest)
                && expected_digest(&self.signer_fingerprint),
            "InvalidArgument",
            "invalid backup transfer attestation fields",
        )
    }

    fn signed_bytes(&self) -> Vec<u8> {
        let mut bytes = DOMAIN.to_vec();
        bytes.extend(canonical(&json!(self)));
        bytes
    }
}

impl Attestation {
    fn verify_signature(&self, key: &[u8; 32]) -> Result<()> {
        self.attestation.validate()?;
        require(
            self.attestation.signer_fingerprint == sha256(key),
            "PermissionDenied",
            "backup transfer signer does not match the trusted key",
        )?;
        // A canonical envelope has precisely one signature encoding; retries are exact.
        require(
            self.signature_base64.len() == 88,
            "InvalidArgument",
            "invalid backup transfer signature encoding",
        )?;
        let signature = STANDARD.decode(&self.signature_base64).map_err(|_| {
            err(
                "InvalidArgument",
                "invalid backup transfer signature encoding",
            )
        })?;
        require(
            signature.len() == 64 && STANDARD.encode(&signature) == self.signature_base64,
            "InvalidArgument",
            "invalid backup transfer signature encoding",
        )?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(&self.attestation.signed_bytes(), &signature)
            .map_err(|_| err("PermissionDenied", "backup transfer signature is invalid"))
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.attestation.signer_fingerprint
    }

    pub(super) fn matches(&self, intake: &Intake) -> bool {
        self.attestation.backup_ref.resource_id == intake.backup_id
            && self.attestation.source_volume_ref == intake.source
            && self.attestation.data_incarnation == intake.data_incarnation
            && self.attestation.manifest_digest == intake.manifest_digest
    }
}

impl ReceiverAdmission {
    pub(super) fn read(
        envelope: &Value,
        trusted_key: &Path,
        source: &str,
        digest: &str,
    ) -> Result<Self> {
        let envelope: Attestation = decode(envelope)?;
        require(
            exact_id(source, "node-"),
            "InvalidArgument",
            "invalid expected source authority",
        )?;
        require(
            envelope.attestation.source_authority_id == source,
            "PermissionDenied",
            "backup transfer source authority is not trusted",
        )?;
        require(
            envelope.attestation.manifest_digest == digest,
            "PreconditionFailed",
            "backup transfer does not cover the requested manifest",
        )?;
        let bytes = key_bytes(trusted_key, false, 32)?;
        let public_key = bytes.as_slice().try_into().map_err(|_| {
            err(
                "InvalidArgument",
                "trusted Ed25519 public key must be 32 bytes",
            )
        })?;
        envelope.verify_signature(&public_key)?;
        Ok(Self {
            envelope,
            public_key,
        })
    }

    pub(super) fn check_recipient(&self, recipient: &str) -> Result<()> {
        require(
            self.envelope.attestation.recipient_authority_id == recipient,
            "PermissionDenied",
            "backup transfer was signed for another recipient authority",
        )
    }

    pub(super) fn persist(&self, conn: &Connection, intake_id: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO signed_backup_transfers VALUES(?1,?2)",
            params![intake_id, self.public_key.as_slice()],
        )?;
        Ok(())
    }

    pub(super) fn verify_existing(
        conn: &Connection,
        intake_id: &str,
        info: &Intake,
        recipient: &str,
        requested: Option<&Self>,
    ) -> Result<()> {
        let key = verify_retained(conn, intake_id, info, recipient, true)?;
        require(
            info.transfer.as_ref() == requested.map(|admission| &admission.envelope)
                && key.as_ref() == requested.map(|request| &request.public_key),
            "Conflict",
            "backup intake is already bound to a different transfer proof",
        )
    }
}

pub(super) fn verify_retained(
    conn: &Connection,
    intake_id: &str,
    info: &Intake,
    recipient: &str,
    signed_format: bool,
) -> Result<Option<[u8; 32]>> {
    let key = if signed_format {
        stored_key(conn, intake_id)?
    } else {
        None
    };
    require(
        info.transfer.is_some() == key.is_some(),
        "RecoveryRequired",
        "backup transfer lost its witnessed signer custody",
    )?;
    if let Some((envelope, key)) = info.transfer.as_ref().zip(key.as_ref()) {
        require(
            envelope.attestation.recipient_authority_id == recipient
                && envelope.matches(info)
                && envelope.verify_signature(key).is_ok(),
            "RecoveryRequired",
            "stored backup transfer signer custody is invalid",
        )?;
    }
    Ok(key)
}

fn stored_key(conn: &Connection, intake_id: &str) -> Result<Option<[u8; 32]>> {
    conn.query_row(
        "SELECT public_key FROM signed_backup_transfers WHERE intake_id=?1",
        [intake_id],
        |row| row.get::<_, Vec<u8>>(0),
    )
    .optional()?
    .map(|bytes| {
        bytes.try_into().map_err(|_| {
            err(
                "RecoveryRequired",
                "invalid stored backup transfer public key",
            )
        })
    })
    .transpose()
}

pub(crate) fn attest(
    conn: &Connection,
    config: &Config,
    authority: &str,
    incarnation: &str,
    reference: &Value,
    recipient: &str,
    key_file: &Path,
) -> Result<Value> {
    require(
        exact_id(recipient, "node-") && authority != recipient,
        "InvalidArgument",
        "backup transfer needs a different, exact recipient authority",
    )?;
    let backup = db::resolve(conn, reference)?;
    require(
        backup.kind == "Backup"
            && backup.body["lifecycle"] == "Stored"
            && backup.body["coordination"] == "exclusive offline volume hold"
            && backup.spec.get("intake_ref").is_none()
            && db::find(conn, &backup.id)?.is_some_and(|live| live.reference() == *reference),
        "PreconditionFailed",
        "attestation needs an exact current native Stored Backup",
    )?;
    let source = db::resolve(conn, &backup.spec["source_ref"])?;
    require(
        source.kind == "Volume" && backup.spec["source_volume"] == source.id,
        "RecoveryRequired",
        "native backup source provenance is inconsistent",
    )?;
    let path = config.state_dir.join("backups").join(&backup.id);
    super::filename(&backup.id)?;
    let digest = backup.body["manifest_digest"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "native backup has no manifest digest"))?;
    let (archive, raw) = open_detached(&path, digest)?;
    let info = Intake::from_archive(&archive, &raw, digest)?;
    require(
        info.backup_id == backup.id
            && json!(info.source) == source.reference()
            && source.body["data_incarnation"] == info.data_incarnation
            && backup.body["entries"] == info.entries
            && backup.body["bytes"] == info.bytes,
        "RecoveryRequired",
        "native backup archive differs from its catalog provenance",
    )?;
    info.verify_archive(&archive)?;

    let key = key_bytes(key_file, true, MAX_SIGNING_KEY_BYTES)?;
    let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&key)
        .map_err(|_| err("InvalidArgument", "signing key must be Ed25519 PKCS#8 DER"))?;
    let statement = Statement {
        format: 1,
        source_authority_id: authority.to_owned(),
        source_recovery_incarnation: incarnation.to_owned(),
        recipient_authority_id: recipient.to_owned(),
        backup_ref: decode(reference)?,
        source_volume_ref: info.source,
        data_incarnation: info.data_incarnation,
        manifest_digest: info.manifest_digest,
        signer_fingerprint: sha256(pair.public_key().as_ref()),
    };
    statement.validate()?;
    let signature = pair.sign(&statement.signed_bytes());
    Ok(json!(Attestation {
        attestation: statement,
        signature_base64: STANDARD.encode(signature.as_ref()),
    }))
}

pub(crate) fn verify(conn: &Connection, authority: &str) -> Result<()> {
    // Bidirectional custody: a verifier entry must have a witnessed signed intake,
    // and an intake marked signed must have its exact public verifier.
    let mut rows = conn.prepare(
        "SELECT id FROM versions
          WHERE kind='BackupIntake' AND json_type(spec,'$.transfer') IS NOT NULL
         UNION SELECT intake_id FROM signed_backup_transfers
         ORDER BY 1",
    )?;
    let ids = rows.query_map([], |row| row.get::<_, String>(0))?;
    for id in ids {
        verify_intake(conn, authority, &id?)?;
    }
    Ok(())
}

fn verify_intake(conn: &Connection, authority: &str, id: &str) -> Result<()> {
    let custody_error = || {
        err(
            "RecoveryRequired",
            "signed backup transfer custody is inconsistent",
        )
    };
    let key = stored_key(conn, id)?.ok_or_else(custody_error)?;
    let mut history = conn.prepare("SELECT * FROM versions WHERE id=?1 ORDER BY rev")?;
    let mut revisions = history.query([id])?;
    let first = db::row(revisions.next()?.ok_or_else(custody_error)?)?;
    let info: Intake = decode(&first.spec).map_err(|_| custody_error())?;
    let transfer = info.transfer.as_ref().ok_or_else(custody_error)?;
    transfer
        .verify_signature(&key)
        .map_err(|_| custody_error())?;
    let valid_prepared = |intake: &db::Doc| {
        intake.kind == KIND
            && intake.owner == policy::OWNER
            && intake.generation == 1
            && intake.spec == first.spec
            && intake.body["lifecycle"] == "Prepared"
    };
    require(
        first.rev == 1
            && valid_prepared(&first)
            && json!(info) == first.spec
            && expected_digest(&info.manifest_digest)
            && id == super::intake::intake_id(&info.manifest_digest)
            && transfer.matches(&info)
            && transfer.attestation.recipient_authority_id == authority,
        "RecoveryRequired",
        "signed backup transfer custody is inconsistent",
    )?;
    let mut prepared = first.clone();
    while let Some(row) = revisions.next()? {
        let entry = db::row(row)?;
        require(
            entry.rev == prepared.rev + 1
                && entry.kind == KIND
                && entry.owner == policy::OWNER
                && entry.generation == 1
                && entry.spec == first.spec,
            "RecoveryRequired",
            "signed backup transfer revisions changed custody",
        )?;
        if valid_prepared(&entry) {
            prepared = entry;
            continue;
        }
        require(
            entry.body["lifecycle"] == "Settled"
                && prepared.body["directory"] == entry.body["directory"]
                && revisions.next()?.is_none(),
            "RecoveryRequired",
            "invalid signed backup intake settlement",
        )?;
        let backup = db::resolve(conn, &entry.body["backup_ref"]).map_err(|_| custody_error())?;
        require(
            backup.kind == "Backup"
                && backup.rev == 1
                && backup.owner == policy::OWNER
                && backup.id == info.backup_id
                && backup.spec["intake_ref"] == prepared.reference()
                && backup.spec["source_ref"] == json!(info.source)
                && backup.spec["source_data_incarnation"] == info.data_incarnation
                && backup.body["lifecycle"] == "Stored"
                && backup.body["manifest_digest"] == info.manifest_digest
                && backup.body["entries"] == info.entries
                && backup.body["bytes"] == info.bytes,
            "RecoveryRequired",
            "signed backup transfer settlement has no exact imported Backup",
        )?;
        break;
    }
    Ok(())
}
