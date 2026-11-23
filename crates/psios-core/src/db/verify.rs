use super::{Store, checked_head, open_regular};
use crate::{Config, Result, config::private_dir, err, policy, require};
use psios_api::sha256;
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions, Permissions},
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
use zeroize::Zeroizing;

// Never open the original database with SQLite. Even a read-only SQLite
// connection can create or update the WAL shared-memory index.
fn copy_database(source: &Path, target: &Path, limit: u64) -> Result<u64> {
    let mut total = 0;
    for name in ["state.db", "state.db-wal", "state.db-journal"] {
        let file = match open_regular(&source.join(name)) {
            Err(e) if e.kind() == ErrorKind::NotFound && name != "state.db" => continue,
            other => other?,
        };
        let size = file.metadata()?.len();
        require(
            size <= limit - total,
            "QuotaExceeded",
            "database and journals exceed verification copy limit",
        )?;
        let mut out = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(target.join(name))?;
        require(
            std::io::copy(&mut file.take(size + 1), &mut out)? == size,
            "Conflict",
            "database changed during offline verification",
        )?;
        total += size;
    }
    Ok(total)
}

fn owner_matches(conn: &Connection, path: &Path) -> Result<bool> {
    let f = match open_regular(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut token = Zeroizing::new(Vec::new());
    f.take(129).read_to_end(&mut token)?;
    require(
        token.len() <= 128,
        "RecoveryRequired",
        "invalid local owner credential file",
    )?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM credentials WHERE principal=?1 AND digest=?2)",
        params![policy::OWNER, sha256(&token)],
        |r| r.get(0),
    )?)
}

impl Store {
    pub fn verify(cfg: &Config, scratch: &Path, max_bytes: u64) -> Result<Value> {
        cfg.validate()?;
        private_dir(&cfg.state_dir, false)?;
        private_dir(&cfg.witness_dir, false)?;
        let lock = open_regular(&cfg.state_dir.join("daemon.lock"))?;
        lock.try_lock_shared()
            .map_err(|_| err("Conflict", "stop the authority before offline verification"))?;
        let parent = scratch.canonicalize()?;
        require(
            !parent.starts_with(&cfg.state_dir) && !parent.starts_with(&cfg.witness_dir),
            "InvalidArgument",
            "verification scratch must be outside state and witness",
        )?;
        let meta = parent.metadata()?;
        require(
            meta.is_dir()
                && (meta.uid() == 0 || meta.uid() == rustix::process::geteuid().as_raw())
                && (meta.mode() & 0o022 == 0 || meta.mode() & 0o1000 != 0),
            "PermissionDenied",
            "verification scratch directory must protect its entries",
        )?;
        let tmp = tempfile::Builder::new()
            .prefix("psios-verify-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(parent)?;
        let bytes = copy_database(&cfg.state_dir, tmp.path(), max_bytes)?;
        let copy = Connection::open_with_flags(
            tmp.path().join("state.db"),
            OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        copy.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
        let check: String = copy.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))?;
        require(
            check == "ok",
            "RecoveryRequired",
            "database integrity check failed",
        )?;
        require(
            !copy.prepare("PRAGMA foreign_key_check")?.exists([])?,
            "RecoveryRequired",
            "foreign key integrity check failed",
        )?;
        let checked = checked_head(&copy, cfg)?;
        let next = cfg.witness_dir.join("owner.next");
        let rotation = match fs::symlink_metadata(&next) {
            Err(e) if e.kind() == ErrorKind::NotFound => "None",
            Err(e) => return Err(e.into()),
            Ok(_) if owner_matches(&copy, &next)? => "PendingPromotion",
            Ok(_) => "PendingDiscard",
        };
        require(
            rotation == "PendingPromotion"
                || owner_matches(&copy, &cfg.witness_dir.join("owner.token"))?,
            "RecoveryRequired",
            "local owner credential does not match the authority",
        )?;
        let witness = match checked.witness.get("pending") {
            None => "Confirmed",
            Some(v) if *v == checked.head => "PendingCommitFinalization",
            Some(_) => "PendingIntentDiscard",
        };
        let node: String =
            copy.query_row("SELECT value FROM meta WHERE key='authority'", [], |r| {
                r.get(0)
            })?;
        Ok(
            json!({"node_id":node, "database_format":checked.version, "migration_required":checked.version == 1,
            "sqlite_integrity":"Verified","foreign_keys":"Verified","witness":witness,
            "owner_rotation":rotation,"copied_bytes":bytes}),
        )
    }
}
