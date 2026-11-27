use super::{CURRENT_FORMAT, StateLock, Store, checked_head, open_regular, witness_bytes};
use crate::{
    Config, Result, artifact,
    config::{new_file, private_dir},
    err, policy, require,
};
use psios_api::sha256;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions, Permissions},
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

// Never open the original database with SQLite. Even a read-only SQLite
// connection can create or update the WAL shared-memory index.
pub(super) const DATABASE_FILES: &[&str] = &["state.db", "state.db-wal", "state.db-journal"];

pub(super) fn copy_database(source: &Path, target: &Path, limit: u64) -> Result<u64> {
    let mut total = 0;
    for &name in DATABASE_FILES {
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

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct DatabaseIdentity {
    pub node_id: String,
    pub recovery_incarnation: String,
    pub head: String,
    pub database_format: u32,
}

pub(super) fn lock_offline(cfg: &Config) -> Result<StateLock> {
    cfg.validate()?;
    private_dir(&cfg.state_dir, false)?;
    private_dir(&cfg.witness_dir, false)?;
    let lock = open_regular(&cfg.state_dir.join("daemon.lock"))?;
    lock.try_lock_shared()
        .map_err(|_| err("Conflict", "stop the authority before offline verification"))?;
    Ok(StateLock(lock))
}

pub(super) fn scratch_parent(scratch: &Path) -> Result<PathBuf> {
    let parent = scratch.canonicalize()?;
    let meta = parent.metadata()?;
    require(
        meta.is_dir()
            && (meta.uid() == 0 || meta.uid() == rustix::process::geteuid().as_raw())
            && (meta.mode() & 0o022 == 0 || meta.mode() & 0o1000 != 0),
        "PermissionDenied",
        "verification scratch directory must protect its entries",
    )?;
    Ok(parent)
}

pub(super) fn outside_authority(cfg: &Config, path: &Path) -> Result<()> {
    require(
        !path.starts_with(&cfg.state_dir) && !path.starts_with(&cfg.witness_dir),
        "InvalidArgument",
        "offline copy destination must be outside state and witness",
    )
}

pub(super) fn checked_copy(directory: &Path) -> Result<(Connection, DatabaseIdentity)> {
    let copy = Connection::open_with_flags(
        directory.join("state.db"),
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
    let database_format = copy.pragma_query_value(None, "user_version", |r| r.get(0))?;
    require(
        (1..=super::CURRENT_FORMAT).contains(&database_format),
        "SchemaVersionUnsupported",
        "unsupported state database version",
    )?;
    let meta = |key| copy.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0));
    let identity = DatabaseIdentity {
        node_id: meta("authority")?,
        recovery_incarnation: meta("incarnation")?,
        head: meta("head")?,
        database_format,
    };
    Ok((copy, identity))
}

pub(super) fn check_artifacts(
    conn: &Connection,
    identity: &DatabaseIdentity,
    state: &Path,
) -> Result<()> {
    if identity.database_format >= crate::storage::retirement::FORMAT {
        crate::storage::retirement::verify(conn)?;
    }
    if identity.database_format >= crate::storage::entry_retirement::FORMAT {
        crate::storage::entry_retirement::verify(conn)?;
    }
    if identity.database_format >= crate::storage::cloning::FORMAT {
        crate::storage::restore_publication::verify_clones(conn)?;
    }
    if identity.database_format >= crate::storage::restore_publication::CREATE_FORMAT {
        crate::storage::restore_publication::verify_creates(conn)?;
    }
    if identity.database_format >= crate::storage::directory_create::FORMAT {
        crate::storage::directory_create::verify(conn)?;
    }
    if identity.database_format >= crate::apps::volume_rebind::FORMAT {
        crate::apps::volume_rebind::verify::check(conn, identity.database_format)?;
    }
    if identity.database_format >= crate::apps::mount_observation::FORMAT {
        crate::apps::mount_observation::verify(conn)?;
    }
    if identity.database_format >= crate::node::block_device::FORMAT {
        crate::node::block_device::verify(conn, identity.database_format)?;
    }
    if identity.database_format >= artifact::files::FORMAT {
        artifact::files::verify_registry(conn)?;
    }
    if identity.database_format >= artifact::FORMAT {
        artifact::verify(conn, Some(state))?;
    }
    if identity.database_format >= artifact::ingress::FORMAT {
        artifact::ingress::verify(conn)?;
    }
    if identity.database_format >= crate::apps::artifact_install::FORMAT {
        crate::apps::artifact_install::verify(conn)?;
    }
    if identity.database_format >= crate::apps::release_retirement::FORMAT {
        crate::apps::release_retirement::verify(conn)?;
    }
    if identity.database_format >= artifact::session_output::FORMAT {
        artifact::session_output::verify(conn)?;
    }
    if identity.database_format >= crate::apps::publisher::FORMAT {
        crate::apps::publisher::verify(conn)?;
    }
    if identity.database_format >= crate::apps::capture::FORMAT {
        crate::apps::capture::verify(conn)?;
    }
    if identity.database_format >= crate::apps::release_archive::FORMAT {
        crate::apps::release_archive::verify(conn)?;
    }
    if identity.database_format >= crate::apps::restore_archive::FORMAT {
        crate::apps::capture::verify_restore_compatibility(conn, state)?;
    }
    if identity.database_format >= crate::apps::restore_image::FORMAT {
        crate::apps::restore_image::verify(conn, state)?;
    }
    if identity.database_format >= crate::agent::tool_context::FORMAT {
        crate::agent::tool_context::verify(conn, identity.database_format)?;
    }
    if identity.database_format >= crate::agent::inference::FORMAT {
        crate::agent::inference::verify(conn)?;
    }
    if identity.database_format >= crate::agent::provider_authority::FORMAT {
        crate::agent::provider_authority::verify(conn)?;
        crate::agent::fallback::verify_not_started(conn)?;
    }
    if identity.database_format >= crate::agent::late_decision::FORMAT {
        crate::agent::late_decision::verify(conn)?;
    }
    if identity.database_format >= crate::broker::preparation::FORMAT {
        crate::broker::preparation::verify(conn)?;
    }
    if identity.database_format >= crate::artifact::ingress::authority::FORMAT {
        crate::artifact::ingress::authority::verify(conn)?;
    }
    if identity.database_format >= crate::workflow::admission_delay::FORMAT {
        crate::workflow::admission_delay::verify(conn)?;
    }
    if identity.database_format >= crate::workflow::session_join::FORMAT {
        crate::workflow::session_join::verify(conn)?;
    }
    if identity.database_format >= crate::agent::session_inputs::FORMAT {
        crate::agent::session_inputs::verify(conn, identity.database_format)?;
    }
    if identity.database_format >= crate::schedule::admission_delay::FORMAT {
        crate::schedule::admission_delay::verify(conn)?;
    }
    if identity.database_format >= crate::schedule::workflow_overlap::FORMAT {
        crate::schedule::workflow_overlap::verify(
            conn,
            identity.database_format >= crate::schedule::event_workflow_overlap::FORMAT,
        )?;
    }
    if identity.database_format >= crate::schedule::event_workflow_overlap::FORMAT {
        crate::schedule::event_workflow_overlap::verify(conn)?;
    }
    if identity.database_format >= crate::mcp_tasks::FORMAT {
        crate::mcp_tasks::verify(conn)?;
    }
    if identity.database_format >= crate::schedule::coalescence::FORMAT {
        crate::schedule::coalescence::verify(conn)?;
    }
    if identity.database_format >= crate::schedule::renewal::FORMAT {
        crate::schedule::renewal::verify(conn)?;
    }
    if identity.database_format >= crate::project::FORMAT {
        crate::project::verify(conn, identity.database_format)?;
        crate::project::evidence::verify(conn)?;
    }
    if identity.database_format >= crate::project::review_signal::FORMAT {
        crate::project::review_signal::verify(conn)?;
    }
    if identity.database_format >= crate::project::milestone::FORMAT {
        crate::project::milestone::verify::check(conn)?;
    }
    if identity.database_format >= crate::project::hierarchy::FORMAT {
        crate::project::hierarchy::verify(conn)?;
    }
    if identity.database_format >= artifact::retention::batch::FORMAT {
        artifact::retention::batch::verify::check(conn)?;
    }
    if identity.database_format >= artifact::retention::delayed::FORMAT {
        artifact::retention::delayed::verify(conn)?;
    }
    if identity.database_format >= crate::node::work_admission::FORMAT {
        crate::node::work_admission::verify(conn)?;
    }
    if identity.database_format >= crate::node::service_control::FORMAT {
        crate::node::service_control::verify(conn)?;
    }
    if identity.database_format >= crate::apps::create_recovery::FORMAT {
        crate::apps::create_recovery::verify(conn)?;
    }
    if identity.database_format >= crate::apps::stop::FORMAT {
        crate::apps::stop::verify(conn)?;
    }
    if identity.database_format >= crate::apps::restart::FORMAT {
        crate::apps::restart::verify(conn, identity.database_format)?;
    }
    if identity.database_format >= crate::node::network::probe::state::FORMAT {
        crate::node::network::probe::state::verify(conn)?;
    }
    if identity.database_format >= crate::apps::health::FORMAT {
        crate::apps::health::verify(conn)?;
    }
    if identity.database_format >= crate::node::network::trial::FORMAT {
        crate::node::network::trial::verify_active(conn, identity.database_format)?;
    }
    if identity.database_format >= crate::node::network::trial::ROLLBACK_FORMAT {
        crate::node::network::trial::rollback_verify::check(conn)?;
    }
    if identity.database_format >= crate::storage::restore_publication::DISPOSAL_FORMAT {
        crate::storage::restore_publication::verify(conn)?;
    }
    if identity.database_format >= crate::storage::publication::FORMAT {
        crate::storage::publication::verify(conn)?;
    }
    if identity.database_format >= crate::storage::transfer::FORMAT {
        crate::storage::transfer::verify(conn, &identity.node_id)?;
    }
    Ok(())
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

pub(super) fn verified_witness(
    copy: &Connection,
    cfg: &Config,
) -> Result<(&'static str, &'static str)> {
    let checked = checked_head(copy, cfg)?;
    let next = cfg.witness_dir.join("owner.next");
    let rotation = match fs::symlink_metadata(&next) {
        Err(e) if e.kind() == ErrorKind::NotFound => "None",
        Err(e) => return Err(e.into()),
        Ok(_) if owner_matches(copy, &next)? => "PendingPromotion",
        Ok(_) => "PendingDiscard",
    };
    require(
        rotation == "PendingPromotion"
            || owner_matches(copy, &cfg.witness_dir.join("owner.token"))?,
        "RecoveryRequired",
        "local owner credential does not match the authority",
    )?;
    let witness = match checked.witness.get("pending") {
        None => "Confirmed",
        Some(v) if *v == checked.head => "PendingCommitFinalization",
        Some(_) => "PendingIntentDiscard",
    };
    Ok((witness, rotation))
}

fn private_database_copy(
    cfg: &Config,
    scratch: &Path,
    max_bytes: u64,
) -> Result<(tempfile::TempDir, PathBuf, u64)> {
    let parent = scratch_parent(scratch)?;
    outside_authority(cfg, &parent)?;
    let tmp = tempfile::Builder::new()
        .prefix("psios-verify-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(parent)?;
    let state = tmp.path().join("state");
    private_dir(&state, true)?;
    let bytes = copy_database(&cfg.state_dir, &state, max_bytes)?;
    Ok((tmp, state, bytes))
}

impl Store {
    pub fn verify(cfg: &Config, scratch: &Path, max_bytes: u64) -> Result<Value> {
        Self::verify_copy(cfg, scratch, max_bytes, false)
    }

    pub fn rehearse_migration(cfg: &Config, scratch: &Path, max_bytes: u64) -> Result<Value> {
        Self::verify_copy(cfg, scratch, max_bytes, true)
    }

    fn verify_copy(cfg: &Config, scratch: &Path, max_bytes: u64, rehearse: bool) -> Result<Value> {
        let _lock = lock_offline(cfg)?;
        let (tmp, state, bytes) = private_database_copy(cfg, scratch, max_bytes)?;
        let (copy, identity) = checked_copy(&state)?;
        let (witness, rotation) = verified_witness(&copy, cfg)?;
        check_artifacts(&copy, &identity, &cfg.state_dir)?;
        drop(copy);
        let mut report = json!({"node_id":identity.node_id, "database_format":identity.database_format, "migration_required":identity.database_format < CURRENT_FORMAT,
            "sqlite_integrity":"Verified","foreign_keys":"Verified","witness":witness,
            "owner_rotation":rotation,"copied_bytes":bytes});
        if rehearse {
            let witness = tmp.path().join("witness");
            private_dir(&witness, true)?;
            for (name, limit) in [
                ("head", 4096),
                ("keys", 64),
                ("owner.token", 128),
                ("owner.next", 128),
            ] {
                let path = cfg.witness_dir.join(name);
                if (name == "owner.next"
                    || (name == "owner.token" && rotation == "PendingPromotion"))
                    && let Err(e) = fs::symlink_metadata(&path)
                    && e.kind() == ErrorKind::NotFound
                {
                    continue;
                }
                new_file(&witness.join(name), &witness_bytes(&path, limit)?)?;
            }
            let mut rehearsal = cfg.clone();
            rehearsal.state_dir = state;
            rehearsal.witness_dir = witness;
            rehearsal.socket = tmp.path().join("unused-api.sock");
            rehearsal.docker_socket = tmp.path().join("unused-docker.sock");
            report["migration_rehearsal"] = run_rehearsal(rehearsal, &identity, &cfg.state_dir)
                .map_err(|e| {
                    err(
                        e.0.code.as_str(),
                        format!("migration rehearsal failed: {}", e.0.message),
                    )
                })?;
        }
        Ok(report)
    }

    /// Inspect one witnessed intake and its final path without opening the authority for writes.
    pub fn inspect_backup_intake(
        cfg: &Config,
        scratch: &Path,
        max_copy_bytes: u64,
        digest: &str,
        max_bytes: u64,
    ) -> Result<Value> {
        require(
            (1..=1024 * 1024 * 1024).contains(&max_copy_bytes),
            "InvalidArgument",
            "invalid backup intake verification copy limit",
        )?;
        let _lock = lock_offline(cfg)?;
        let (_tmp, state, _) = private_database_copy(cfg, scratch, max_copy_bytes)?;
        let (copy, identity) = checked_copy(&state)?;
        let (witness, rotation) = verified_witness(&copy, cfg)?;
        require(
            witness == "Confirmed" && rotation == "None",
            "RecoveryRequired",
            "backup intake inspection needs a confirmed witness and owner credential",
        )?;
        crate::storage::inspect_backup_intake(
            &copy,
            cfg,
            identity.database_format,
            &identity.node_id,
            &identity.recovery_incarnation,
            digest,
            max_bytes,
        )
    }

    /// Sign one exact native Backup for another authority without opening the source for writes.
    pub fn attest_backup_transfer(
        cfg: &Config,
        scratch: &Path,
        max_copy_bytes: u64,
        reference: &Value,
        recipient: &str,
        signing_key: &Path,
    ) -> Result<Value> {
        require(
            (1..=1024 * 1024 * 1024).contains(&max_copy_bytes),
            "InvalidArgument",
            "invalid backup transfer verification copy limit",
        )?;
        let _lock = lock_offline(cfg)?;
        let (_tmp, state, _) = private_database_copy(cfg, scratch, max_copy_bytes)?;
        let (copy, identity) = checked_copy(&state)?;
        let (witness, rotation) = verified_witness(&copy, cfg)?;
        require(
            witness == "Confirmed" && rotation == "None",
            "RecoveryRequired",
            "backup transfer attestation needs a confirmed witness and owner credential",
        )?;
        crate::storage::transfer::attest(
            &copy,
            cfg,
            &identity.node_id,
            &identity.recovery_incarnation,
            reference,
            recipient,
            signing_key,
        )
    }

    pub fn inspect_retained_inputs(
        cfg: &Config,
        scratch: &Path,
        max_copy_bytes: u64,
        max_scanned_bytes: u64,
        references: &Value,
    ) -> Result<Value> {
        require(
            (1..=256 * 1024 * 1024).contains(&max_copy_bytes)
                && (1..=512 * 1024 * 1024).contains(&max_scanned_bytes),
            "InvalidArgument",
            "retained input inventory limits are outside the supported range",
        )?;
        let _lock = lock_offline(cfg)?;
        let (_tmp, state, copied_bytes) = private_database_copy(cfg, scratch, max_copy_bytes)?;
        let (copy, identity) = checked_copy(&state)?;
        let (witness, rotation) = verified_witness(&copy, cfg)?;
        require(
            witness == "Confirmed" && rotation == "None",
            "RecoveryRequired",
            "retained input inventory needs a confirmed authority witness and owner credential",
        )?;
        let observed = super::input_inventory::scan(&copy, references, max_scanned_bytes)?;
        Ok(json!({
            "scope":"StoredSqliteValues",
            "node_id":identity.node_id,
            "database_format":identity.database_format,
            "recovery_incarnation":identity.recovery_incarnation,
            "head":identity.head,
            "sqlite_integrity":"Verified",
            "foreign_keys":"Verified",
            "witness":witness,
            "copied_bytes":copied_bytes,
            "inputs":observed["inputs"],
            "scan":observed["scan"],
        }))
    }
}

fn run_rehearsal(cfg: Config, before: &DatabaseIdentity, payload_root: &Path) -> Result<Value> {
    // Store::open finalizes pending witness writes and executes the production migration.
    // Its paths lead only to the temporary clone. No server, broker or effect recovery starts.
    drop(Store::open(cfg.clone())?);
    let (copy, after) = checked_copy(&cfg.state_dir)?;
    let (witness, rotation) = verified_witness(&copy, &cfg)?;
    require(
        after.database_format == CURRENT_FORMAT
            && after.node_id == before.node_id
            && after.recovery_incarnation == before.recovery_incarnation
            && witness == "Confirmed"
            && rotation == "None",
        "RecoveryRequired",
        "migrated copy did not preserve the verified authority",
    )?;
    check_artifacts(&copy, &after, payload_root)?;
    Ok(
        json!({"status":"Verified", "source_format":before.database_format, "target_format":after.database_format}),
    )
}
