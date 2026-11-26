//! SQLite compaction of a stopped authority preserves its logical commit head.
mod space;

use super::{
    StateLock, Store, checked_head, open_regular,
    verify::{self, DatabaseIdentity},
    witness_bytes,
};
use crate::{Config, Error, Result, config::private_dir, decode, err, require};
use psios_api::{canonical, sha256};
use rusqlite::{Connection, ErrorCode, OpenFlags, config::DbConfig, ffi};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, OsStr, c_char},
    fs::{File, Permissions},
    io::{ErrorKind, Read},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::Duration,
};

const MAIN: &str = "state.db";
const WAL: &str = "state.db-wal";
const JOURNAL: &str = "state.db-journal";

#[derive(Debug, PartialEq, Eq)]
struct FileState {
    device: u64,
    inode: u64,
    bytes: u64,
    blocks: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn managed_file(path: &Path) -> Result<File> {
    let file = open_regular(path)?;
    let meta = file.metadata()?;
    require(
        meta.nlink() == 1 && meta.mode() & 0o077 == 0,
        "PermissionDenied",
        "database and lock files must be private and unaliased",
    )?;
    Ok(file)
}

fn file_state(directory: &Path, name: &str) -> Result<Option<FileState>> {
    let path = directory.join(name);
    match path.symlink_metadata() {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let meta = managed_file(&path)?.metadata()?;
    Ok(Some(FileState {
        device: meta.dev(),
        inode: meta.ino(),
        bytes: meta.len(),
        blocks: meta.blocks(),
        modified: (meta.mtime(), meta.mtime_nsec()),
        changed: (meta.ctime(), meta.ctime_nsec()),
    }))
}

#[derive(PartialEq, Eq)]
struct Source {
    main: FileState,
    wal: Option<FileState>,
    journal: Option<FileState>,
}

impl Source {
    fn capture(directory: &Path) -> Result<Self> {
        let source = Self {
            main: file_state(directory, MAIN)?
                .ok_or_else(|| err("RecoveryRequired", "database is missing"))?,
            wal: file_state(directory, WAL)?,
            journal: file_state(directory, JOURNAL)?,
        };
        require(
            source.journal.as_ref().is_none_or(|file| file.bytes == 0),
            "RecoveryRequired",
            "resolve the rollback journal with normal authority recovery before compaction",
        )?;
        // SQLite opens the WAL index itself. Check its custody, never copy it.
        file_state(directory, "state.db-shm")?;
        Ok(source)
    }

    fn files(&self) -> [(Option<&FileState>, &str); 3] {
        [
            (Some(&self.main), MAIN),
            (self.wal.as_ref(), WAL),
            (self.journal.as_ref(), JOURNAL),
        ]
    }

    fn wal_bytes(&self) -> u64 {
        self.wal.as_ref().map_or(0, |file| file.bytes)
    }

    fn fingerprint(&self, copied: &Path) -> Result<Value> {
        let mut entries = serde_json::Map::new();
        for (source, name) in self.files() {
            if let Some(source) = source {
                let mut input = managed_file(&copied.join(name))?;
                let mut hash = Sha256::new();
                let mut buffer = [0; 65536];
                loop {
                    let read = input.read(&mut buffer)?;
                    if read == 0 {
                        break;
                    }
                    hash.update(&buffer[..read]);
                }
                entries.insert(
                    name.into(),
                    json!({
                        "device":source.device.to_string(),
                        "inode":source.inode.to_string(),
                        "bytes":source.bytes,
                        "sha256":format!("sha256:{:x}", hash.finalize()),
                    }),
                );
            }
        }
        Ok(Value::Object(entries))
    }
}

#[derive(PartialEq, Eq, Serialize)]
struct Statistics {
    main_bytes: u64,
    wal_bytes: u64,
    allocated_file_bytes: u64,
    page_size: u64,
    page_count: u64,
    free_pages: u64,
}

fn pragma_count(conn: &Connection, name: &str) -> Result<u64> {
    let count: i64 = conn.pragma_query_value(None, name, |row| row.get(0))?;
    u64::try_from(count).map_err(|_| err("RecoveryRequired", "invalid database page count"))
}

impl Statistics {
    fn read(conn: &Connection, source: &Source) -> Result<Self> {
        let page_size = pragma_count(conn, "page_size")?;
        let page_count = pragma_count(conn, "page_count")?;
        let free_pages = pragma_count(conn, "freelist_count")?;
        require(
            (512..=65536).contains(&page_size)
                && page_size.is_power_of_two()
                && page_count != 0
                && free_pages < page_count,
            "RecoveryRequired",
            "invalid SQLite file geometry",
        )?;
        Ok(Self {
            main_bytes: source.main.bytes,
            wal_bytes: source.wal_bytes(),
            allocated_file_bytes: source
                .main
                .blocks
                .saturating_add(source.wal.as_ref().map_or(0, |file| file.blocks))
                .saturating_mul(512),
            page_size,
            page_count,
            free_pages,
        })
    }

    fn file_bytes(&self) -> u64 {
        self.main_bytes.saturating_add(self.wal_bytes)
    }
}

struct Snapshot {
    identity: DatabaseIdentity,
    statistics: Statistics,
    source: Source,
    files: Value,
    witness_head_digest: String,
    vacuum_temp: Option<PathBuf>,
}

fn committed_witness(conn: &Connection, cfg: &Config) -> Result<String> {
    let (witness, rotation) = verify::verified_witness(conn, cfg)?;
    require(
        witness == "Confirmed" && rotation == "None",
        "RecoveryRequired",
        "resolve the pending witness or owner rotation before compaction",
    )?;
    Ok(sha256(&witness_bytes(&cfg.witness_dir.join("head"), 4096)?))
}

fn snapshot(cfg: &Config, scratch: &Path, max_bytes: u64, prepare_apply: bool) -> Result<Snapshot> {
    let parent = verify::scratch_parent(scratch)?;
    verify::outside_authority(cfg, &parent)?;
    let source = Source::capture(&cfg.state_dir)?;
    let sizes = source
        .files()
        .into_iter()
        .filter_map(|(file, _)| file.map(|file| file.bytes));
    require(
        sizes
            .clone()
            .try_fold(0u64, u64::checked_add)
            .is_some_and(|total| total <= max_bytes),
        "QuotaExceeded",
        "database and journals exceed compaction copy limit",
    )?;
    cfg.check_managed_space(&File::open(&parent)?, sizes, 6)?;
    let scratch = tempfile::Builder::new()
        .prefix("psios-compact-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(parent)?;
    verify::copy_database(&cfg.state_dir, scratch.path(), max_bytes)?;
    require(
        Source::capture(&cfg.state_dir)? == source,
        "Conflict",
        "database files changed during the offline capture",
    )?;
    let files = source.fingerprint(scratch.path())?;
    let (copy, identity) = verify::checked_copy(scratch.path())?;
    let witness_head_digest = committed_witness(&copy, cfg)?;
    verify::check_artifacts(&copy, &identity, &cfg.state_dir)?;
    let journal: String = copy.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    require(
        journal == "wal",
        "UnsupportedCapability",
        "compaction requires SQLite WAL mode",
    )?;
    let statistics = Statistics::read(&copy, &source)?;
    let vacuum_temp = if prepare_apply && statistics.free_pages != 0 {
        Some(sqlite_temp_directory(&copy)?)
    } else {
        None
    };
    Ok(Snapshot {
        identity,
        statistics,
        source,
        files,
        witness_head_digest,
        vacuum_temp,
    })
}

impl Snapshot {
    fn plan(&self) -> Value {
        let identity = &self.identity;
        let bytes = &self.statistics;
        json!({
            "schema":"psios.state-compaction.v1",
            "node_id":identity.node_id,
            "recovery_incarnation":identity.recovery_incarnation,
            "head":identity.head,
            "database_format":identity.database_format,
            "main_bytes":bytes.main_bytes,
            "wal_bytes":bytes.wal_bytes,
            "page_size":bytes.page_size,
            "page_count":bytes.page_count,
            "free_pages":bytes.free_pages,
            "source_files":self.files,
            "witness_head_digest":self.witness_head_digest,
        })
    }
}

fn plan_digest(plan: &Value) -> String {
    sha256(&canonical(&json!(["psios-state-compaction-plan-v1", plan])))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    plan: Value,
    plan_digest: String,
}

fn lock_for_apply(cfg: &Config) -> Result<StateLock> {
    cfg.validate()?;
    private_dir(&cfg.state_dir, false)?;
    private_dir(&cfg.witness_dir, false)?;
    let lock = managed_file(&cfg.state_dir.join("daemon.lock"))?;
    lock.try_lock()
        .map_err(|_| err("Conflict", "stop the authority before state compaction"))?;
    Ok(StateLock(lock))
}

fn sqlite_conflict(error: rusqlite::Error) -> Error {
    if matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    ) {
        err("Conflict", "SQLite state database is still in use")
    } else {
        error.into()
    }
}

fn writable_database(cfg: &Config, before: &Snapshot) -> Result<Connection> {
    require(
        Source::capture(&cfg.state_dir)? == before.source,
        "Conflict",
        "database files changed after compaction preflight",
    )?;
    let conn = Connection::open_with_flags_and_vfs(
        cfg.state_dir.join(MAIN),
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        "unix",
    )?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    conn.busy_timeout(Duration::from_secs(1))?;
    conn.execute_batch(
        "PRAGMA locking_mode=EXCLUSIVE; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF;
         PRAGMA foreign_keys=ON; PRAGMA temp_store=FILE; PRAGMA cache_size=-8192;
         PRAGMA wal_autocheckpoint=0; BEGIN EXCLUSIVE",
    )
    .map_err(sqlite_conflict)?;
    let checked = checked_head(&conn, cfg)?;
    let meta = |key| {
        conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |row| {
            row.get::<_, String>(0)
        })
    };
    let journal: String = conn.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    require(
        checked.head == before.identity.head
            && checked.version == before.identity.database_format
            && meta("authority")? == before.identity.node_id
            && meta("incarnation")? == before.identity.recovery_incarnation
            && committed_witness(&conn, cfg)? == before.witness_head_digest
            && journal == "wal"
            && Statistics::read(&conn, &before.source)? == before.statistics,
        "Conflict",
        "authority changed after compaction preflight",
    )?;
    Ok(conn)
}

fn sqlite_temp_directory(conn: &Connection) -> Result<PathBuf> {
    require(
        !conn.query_row(
            "SELECT sqlite_compileoption_used('TEMP_STORE=3')",
            [],
            |row| row.get::<_, bool>(0),
        )?,
        "UnsupportedCapability",
        "SQLite cannot provide a disk-backed temporary database",
    )?;
    let mut name = std::ptr::null_mut::<c_char>();
    // The Unix VFS allocates a NUL-terminated filename with SQLite's allocator.
    // This call uses the same temp-name path as the temporary VACUUM database.
    let rc = unsafe {
        ffi::sqlite3_file_control(
            conn.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_TEMPFILENAME,
            (&mut name as *mut *mut c_char).cast(),
        )
    };
    let path = if name.is_null() {
        PathBuf::new()
    } else {
        // Copy the bytes before releasing the VFS-owned allocation.
        unsafe {
            let path = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(name).to_bytes()));
            ffi::sqlite3_free(name.cast());
            path
        }
    };
    require(
        rc == ffi::SQLITE_OK && path.is_absolute() && path.file_name().is_some(),
        "StorageUnavailable",
        "SQLite did not identify an absolute temporary database directory",
    )?;
    verify::scratch_parent(path.parent().unwrap())
}

fn vacuum_and_checkpoint(conn: Connection, vacuum: bool) -> Result<()> {
    // The exclusive locking mode retains the database's lock across COMMIT;
    // VACUUM cannot execute with an application transaction open.
    conn.execute_batch("COMMIT").map_err(sqlite_conflict)?;
    if vacuum {
        conn.execute_batch("VACUUM main")?;
    }
    let progress: (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    require(
        progress == (0, 0, 0),
        "StorageUnavailable",
        "SQLite did not complete WAL truncation",
    )?;
    conn.close().map_err(|(_, error)| error)?;
    Ok(())
}

fn verified_after(
    cfg: &Config,
    scratch: &Path,
    max_bytes: u64,
    state: &File,
    before: &Snapshot,
    vacuum: bool,
) -> Result<Statistics> {
    managed_file(&cfg.state_dir.join(MAIN))?.sync_all()?;
    state.sync_all()?;
    let after = snapshot(cfg, scratch, max_bytes, false)?;
    require(
        after.identity == before.identity
            && after.witness_head_digest == before.witness_head_digest
            && after.source.main.device == before.source.main.device
            && after.source.main.inode == before.source.main.inode
            && after.statistics.page_size == before.statistics.page_size
            && after.statistics.wal_bytes == 0
            && (!vacuum || after.statistics.free_pages == 0),
        "RecoveryRequired",
        "could not confirm compacted state and original authority identity",
    )?;
    Ok(after.statistics)
}

fn report(
    before: &Snapshot,
    after: &Statistics,
    digest: &str,
    vacuum: bool,
    changed: bool,
) -> Value {
    let identity = &before.identity;
    json!({
        "outcome":if changed {"Compacted"} else {"AlreadyCompact"},
        "node_id":identity.node_id,
        "recovery_incarnation":identity.recovery_incarnation,
        "head":identity.head,
        "database_format":identity.database_format,
        "plan_digest":digest,
        "vacuum_performed":vacuum,
        "before":before.statistics,
        "after":after,
        "reclaimed_file_bytes":before.statistics.file_bytes().saturating_sub(after.file_bytes()),
        "reclaimed_allocated_bytes":before.statistics.allocated_file_bytes.saturating_sub(after.allocated_file_bytes),
    })
}

impl Store {
    pub fn plan_state_compaction(cfg: &Config, scratch: &Path, max_bytes: u64) -> Result<Value> {
        let _lock = verify::lock_offline(cfg)?;
        let plan = snapshot(cfg, scratch, max_bytes, false)?.plan();
        Ok(json!({"plan_digest":plan_digest(&plan),"plan":plan}))
    }

    pub fn compact_state(
        cfg: &Config,
        scratch: &Path,
        max_bytes: u64,
        approval: &Value,
    ) -> Result<Value> {
        let approval: Approval = decode(approval)?;
        require(
            plan_digest(&approval.plan) == approval.plan_digest,
            "InvalidArgument",
            "state compaction plan digest mismatch",
        )?;
        let _lock = lock_for_apply(cfg)?;
        let before = snapshot(cfg, scratch, max_bytes, true)?;
        require(
            before.plan() == approval.plan,
            "Conflict",
            "state compaction plan is stale",
        )?;
        let vacuum = before.statistics.free_pages != 0;
        if !vacuum && before.statistics.wal_bytes == 0 {
            return Ok(report(
                &before,
                &before.statistics,
                &approval.plan_digest,
                false,
                false,
            ));
        }
        cfg.check_admission_space()?;
        let state = File::open(&cfg.state_dir)?;
        let temp = match &before.vacuum_temp {
            Some(path) => File::open(path)?,
            None => state.try_clone()?,
        };
        space::check(
            cfg,
            &state,
            &temp,
            before.statistics.main_bytes,
            before.statistics.wal_bytes,
            before.statistics.page_size,
            vacuum,
        )?;
        let conn = writable_database(cfg, &before)?;
        if vacuum {
            let actual = File::open(sqlite_temp_directory(&conn)?)?.metadata()?;
            let expected = temp.metadata()?;
            require(
                pragma_count(&conn, "temp_store")? == 1
                    && (actual.dev(), actual.ino()) == (expected.dev(), expected.ino()),
                "Conflict",
                "SQLite temporary database directory changed after preflight",
            )?;
        }
        let after = vacuum_and_checkpoint(conn, vacuum)
            .and_then(|()| verified_after(cfg, scratch, max_bytes, &state, &before, vacuum))
            .map_err(|error| {
                err(
                    "OutcomeUnknown",
                    format!("state compaction was not confirmed: {}", error.0.message),
                )
            })?;
        Ok(report(&before, &after, &approval.plan_digest, vacuum, true))
    }
}
