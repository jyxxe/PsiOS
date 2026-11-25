use crate::{
    config::{new_file, private_dir, replace},
    *,
};
use psios_api::{ResourceRef, VERSION, canonical, contract, id, sha256};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};
use zeroize::Zeroizing;

mod space;
pub(crate) use space::check_restore_capacity;
mod verify;

pub struct Store {
    pub(crate) config: Config,
    conn: Connection,
    _lock: StateLock,
    keys: Zeroizing<Vec<u8>>,
    sealed: bool,
    pub(crate) changes: std::sync::Arc<watch::Changes>,
    pub(crate) export_cache: std::sync::Arc<std::sync::Mutex<storage::export::Cache>>,
}
pub(crate) struct Db<'a> {
    pub c: &'a Connection,
    pub cfg: &'a Config,
    pub keys: &'a [u8],
    pub correlation: &'a str,
    pub incarnation: String,
}
#[derive(Clone, Debug)]
pub(crate) struct Doc {
    pub id: String,
    pub rev: i64,
    pub generation: i64,
    pub kind: String,
    pub owner: String,
    pub spec: Value,
    pub body: Value,
    pub created: String,
    pub stamp: String,
}
impl Doc {
    pub fn version(&self) -> String {
        format!("r{}", self.rev)
    }
    pub fn reference(&self) -> Value {
        let digest = artifact::stored_digest(self)
            .cloned()
            .unwrap_or_else(|| json!(sha256(&canonical(&self.body))));
        json!({
            "resource_id": self.id,
            "version": self.version(),
            "digest": digest
        })
    }
    pub fn envelope(&self, domain: &str) -> Value {
        json!({
            "api_version": "psios.v1",
            "schema_version": VERSION,
            "kind": self.kind,
            "metadata": {
                "resource_id": self.id,
                "identity_domain_id": domain,
                "authority_id": domain,
                "security_domain_id": domain,
                "owner_principal_id": self.owner,
                "resource_version": self.version(),
                "spec_generation": self.generation.to_string(),
                "created_at": self.created,
                "updated_at": self.stamp
            },
            "spec": self.spec,
            "status": {
                "observed_generation": self.generation.to_string(),
                "lifecycle": self.body.get("lifecycle").and_then(Value::as_str).unwrap_or("Active"),
                "conditions": self.body.get("conditions").cloned().unwrap_or_else(||json!([]))
            }
        })
    }
    pub fn cas(&self, expected: &str) -> Result<()> {
        require(
            self.version() == expected,
            "Conflict",
            "resource version changed",
        )
    }
}
pub(crate) fn row(r: &Row<'_>) -> rusqlite::Result<Doc> {
    Ok(Doc {
        id: r.get("id")?,
        rev: r.get("rev")?,
        generation: r.get("gen")?,
        kind: r.get("kind")?,
        owner: r.get("owner")?,
        spec: r.get("spec")?,
        body: r.get("body")?,
        created: r.get("created")?,
        stamp: r.get("stamp")?,
    })
}
pub(crate) fn find(conn: &Connection, id: &str) -> Result<Option<Doc>> {
    Ok(conn
        .query_row(
            "SELECT * FROM versions WHERE id=?1 ORDER BY rev DESC LIMIT 1",
            [id],
            row,
        )
        .optional()?)
}
fn token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
pub(crate) fn credential() -> String {
    token()
}
struct StateLock(File);
impl Drop for StateLock {
    fn drop(&mut self) {
        // A concurrent fork can retain the open file description until exec.
        // The store drops its SQLite connection before dropping this guard.
        let _ = self.0.unlock();
    }
}
fn lock(c: &Config) -> Result<StateLock> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(c.state_dir.join("daemon.lock"))?;
    f.try_lock()
        .map_err(|_| err("Conflict", "another authority holds the state lock"))?;
    Ok(StateLock(f))
}
fn setup(c: &Connection) -> Result<()> {
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA trusted_schema=OFF;")?;
    Ok(())
}
fn open_regular(path: &Path) -> std::io::Result<File> {
    let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(flags.bits() as i32)
        .open(path)?;
    let m = f.metadata()?;
    if !m.is_file() || m.uid() != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "expected a private regular file",
        ));
    }
    Ok(f)
}

fn witness_bytes(path: &Path, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    open_regular(path)?.take(max + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= max,
        "RecoveryRequired",
        "invalid witness file size",
    )?;
    Ok(bytes)
}
// Older writers cannot dispatch workflow steps with pinned input mappings.
pub(super) const CURRENT_FORMAT: u32 = 32;
struct CheckedHead {
    version: u32,
    head: String,
    witness: Value,
    keys: Zeroizing<Vec<u8>>,
}
fn checked_head(conn: &Connection, cfg: &Config) -> Result<CheckedHead> {
    let ver: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    require(
        (1..=CURRENT_FORMAT).contains(&ver),
        "SchemaVersionUnsupported",
        "unsupported state database version",
    )?;
    let head: String =
        conn.query_row("SELECT value FROM meta WHERE key='head'", [], |r| r.get(0))?;
    let path = cfg.witness_dir.join("head");
    let witness: Value = witness_bytes(&path, 4096)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    require(
        witness["committed"] == head || witness["pending"] == head,
        "RecoveryRequired",
        "state and external witness disagree; writes quarantined",
    )?;
    let keys = witness_bytes(&cfg.witness_dir.join("keys"), 64)
        .map_err(|_| err("RecoveryRequired", "vault and cursor keys unavailable"))?;
    let expected: String =
        conn.query_row("SELECT value FROM meta WHERE key='keys_digest'", [], |r| {
            r.get(0)
        })?;
    require(
        keys.len() == 64 && sha256(&keys) == expected,
        "RecoveryRequired",
        "local key material mismatch",
    )?;
    Ok(CheckedHead {
        version: ver,
        head,
        witness,
        keys,
    })
}
impl Store {
    pub fn init(cfg: &Config) -> Result<()> {
        cfg.validate()?;
        private_dir(&cfg.state_dir, true)?;
        private_dir(&cfg.witness_dir, true)?;
        let _lock = lock(cfg)?;
        let file = cfg.state_dir.join("state.db");
        require(
            !file.try_exists()?
                && fs::read_dir(&cfg.witness_dir)?.next().is_none()
                && fs::read_dir(&cfg.state_dir)?
                    .all(|e| e.is_ok_and(|f| f.file_name() == "daemon.lock")),
            "Conflict",
            "initialization requires empty state and witness directories",
        )?;
        new_file(&file, &[])?;
        let mut conn = Connection::open(&file)?;
        setup(&conn)?;
        let tx = conn.transaction()?;
        tx.execute_batch(include_str!("state.sql"))?;
        tx.execute_batch(include_str!("policy/deadlines.sql"))?;
        let mut key = [0u8; 64];
        use ring::rand::{SecureRandom, SystemRandom};
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| err("InternalError", "entropy unavailable"))?;
        let inc = id("incarnation");
        let head = id("head");
        for (k, v) in [
            ("incarnation", inc.as_str()),
            ("head", &head),
            ("epoch", "1"),
            ("keys_digest", &sha256(&key)),
            ("authority", &id("node")),
        ] {
            tx.execute("INSERT INTO meta VALUES(?1,?2)", [k, v])?;
        }
        let db = Db {
            c: &tx,
            cfg,
            keys: &key,
            correlation: "bootstrap",
            incarnation: inc,
        };
        let owner_token = token();
        policy::bootstrap(&db, &owner_token)?;
        for dir in ["volumes", "backups"] {
            private_dir(&cfg.state_dir.join(dir), true)?
        }
        new_file(&cfg.witness_dir.join("keys"), &key)?;
        new_file(&cfg.witness_dir.join("owner.token"), owner_token.as_bytes())?;
        new_file(
            &cfg.witness_dir.join("head"),
            &canonical(&json!({"committed": head})),
        )?;
        tx.commit()?;
        File::open(&cfg.state_dir)?.sync_all()?;
        Ok(())
    }
    pub fn open(cfg: Config) -> Result<Self> {
        cfg.validate()?;
        private_dir(&cfg.state_dir, false)?;
        private_dir(&cfg.witness_dir, false)?;
        let lock = lock(&cfg)?;
        let conn = Connection::open_with_flags(
            cfg.state_dir.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )?;
        let CheckedHead {
            version: ver,
            head,
            witness,
            keys,
        } = checked_head(&conn, &cfg)?;
        let path = cfg.witness_dir.join("head");
        if witness.get("pending").is_some() {
            replace(&path, &canonical(&json!({"committed": head})))?;
        }
        // Complete an operator's interrupted credential rotation only when the DB committed it.
        let next = cfg.witness_dir.join("owner.next");
        if next.try_exists()? {
            let bytes = witness_bytes(&next, 128)?;
            let active: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM credentials WHERE principal=?1 AND digest=?2)",
                params![policy::OWNER, sha256(&bytes)],
                |r| r.get(0),
            )?;
            if active {
                fs::rename(&next, cfg.witness_dir.join("owner.token"))?
            } else {
                fs::remove_file(&next)?
            }
            File::open(&cfg.witness_dir)?.sync_all()?;
        }
        setup(&conn)?;
        let mut store = Self {
            config: cfg,
            conn,
            _lock: lock,
            keys,
            sealed: false,
            changes: Default::default(),
            export_cache: Default::default(),
        };
        if ver < CURRENT_FORMAT {
            store.transaction(&format!("core-upgrade-v{CURRENT_FORMAT}"), |db| {
                if ver == 1 {
                    db.c.execute_batch(include_str!("upgrade.sql"))?;
                    policy::upgrade(db)?;
                    db.audit(policy::SYSTEM, "Core.Upgrade", None, "Version2")?;
                }
                if ver < 5 {
                    db.c.execute_batch(
                        "ALTER TABLE session_turns ADD COLUMN boot_id TEXT;
                     ALTER TABLE session_turns ADD COLUMN deadline_boot_ms INTEGER;
                     CREATE INDEX pending_model_turns ON session_turns(session) WHERE receipt IS NULL;",
                    )?;
                }
                if ver < 9 {
                    db.c.execute_batch(include_str!("schedule.sql"))?;
                }
                if ver < 12 {
                    db.c.execute_batch(include_str!("workflow.sql"))?;
                }
                if ver < 13 {
                    db.c.execute_batch(include_str!("chat.sql"))?;
                }
                if ver < 17 {
                    db.c.execute_batch(include_str!("agent/queue.sql"))?;
                }
                if ver < 20 {
                    db.c.execute_batch(include_str!("chat/rooms.sql"))?;
                }
                if ver < 25 {
                    db.c.execute_batch(include_str!("policy/deadlines.sql"))?;
                }
                if ver < artifact::FORMAT {
                    db.c.execute_batch(include_str!("artifact.sql"))?;
                }
                if ver < artifact::retention::FORMAT {
                    db.c.execute_batch(include_str!("artifact/retention.sql"))?;
                }
                if ver < 32 {
                    db.c.execute_batch(include_str!("schedule/resource_event.sql"))?;
                }
                policy::upgrade_tool_catalog(db, CURRENT_FORMAT)
            })?;
        }
        store.transaction("node-config", node::sync)?;
        Ok(store)
    }
    /// Offline recovery requires the Linux service account, exclusive lock and a fresh witness.
    pub fn recover_owner(&mut self) -> Result<()> {
        let fresh = Zeroizing::new(credential());
        let next = self.config.witness_dir.join("owner.next");
        new_file(&next, fresh.as_bytes())?;
        self.transaction("local-owner-recovery", |db| {
            let n = db.c.execute(
                "UPDATE credentials SET digest=?1 WHERE principal=?2",
                params![sha256(fresh.as_bytes()), policy::OWNER],
            )?;
            require(
                n == 1,
                "RecoveryRequired",
                "bootstrap principal unavailable",
            )?;
            db.c.execute(
                "UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='epoch'",
                [],
            )?;
            db.c.execute(
                "DELETE FROM chat_send_windows WHERE sender=?1 OR conversation IN
                    (SELECT id FROM direct_chats WHERE inviter=?1 OR invitee=?1)",
                [policy::OWNER],
            )?;
            db.audit(
                policy::OWNER,
                "Principal.RecoverOwner",
                Some(policy::OWNER),
                "Rotated",
            )
        })?;
        fs::rename(next, self.config.witness_dir.join("owner.token"))?;
        File::open(&self.config.witness_dir)?.sync_all()?;
        Ok(())
    }
    pub(crate) fn transaction<T>(
        &mut self,
        correlation: &str,
        f: impl FnOnce(&Db<'_>) -> Result<T>,
    ) -> Result<T> {
        require(!self.sealed, "RecoveryRequired", "authority is quarantined")?;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changes = tx.total_changes();
        let incarnation =
            tx.query_row("SELECT value FROM meta WHERE key='incarnation'", [], |r| {
                r.get(0)
            })?;
        let db = Db {
            c: &tx,
            cfg: &self.config,
            keys: &self.keys,
            correlation,
            incarnation,
        };
        let result = f(&db)?;
        if tx.total_changes() == changes {
            tx.commit()?;
            return Ok(result);
        }
        let head = id("head");
        let previous = db.meta("head")?;
        let path = self.config.witness_dir.join("head");
        tx.execute("UPDATE meta SET value=?1 WHERE key='head'", [&head])?;
        // Never acknowledge a request or cross the host boundary while a witness intent is pending.
        // On restart SQLite can select either side of this one unacknowledged transaction.
        self.sealed = true;
        replace(
            &path,
            &canonical(&json!({"committed": previous, "pending": head})),
        )?;
        tx.commit()?;
        replace(&path, &canonical(&json!({"committed": head})))?;
        self.sealed = false;
        self.changes.notify();
        Ok(result)
    }
}
impl Db<'_> {
    pub fn meta(&self, key: &str) -> Result<String> {
        Ok(self
            .c
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))?)
    }
    pub fn find(&self, id: &str) -> Result<Option<Doc>> {
        find(self.c, id)
    }
    pub fn get(&self, id: &str) -> Result<Doc> {
        self.find(id)?
            .ok_or_else(|| err("NotFound", "resource unavailable"))
    }
    pub fn of(&self, id: &str, kind: &str) -> Result<Doc> {
        let d = self.get(id)?;
        require(
            d.kind == kind,
            "InvalidArgument",
            "unexpected resource kind",
        )?;
        Ok(d)
    }
    pub fn resolve(&self, v: &Value) -> Result<Doc> {
        let rf: ResourceRef = decode(v)?;
        let d = self
            .c
            .query_row(
                "SELECT * FROM versions WHERE id=?1 AND 'r'||rev=?2",
                params![rf.resource_id, rf.version],
                row,
            )
            .optional()?
            .ok_or_else(|| err("PreconditionFailed", "resource version unavailable"))?;
        require(
            d.reference() == *v,
            "PreconditionFailed",
            "reference digest mismatch",
        )?;
        Ok(d)
    }
    pub fn create(
        &self,
        kind: &str,
        id: &str,
        owner: &str,
        spec: Value,
        body: Value,
    ) -> Result<Doc> {
        let stamp = now();
        self.insert(Doc {
            id: id.into(),
            rev: 1,
            generation: 1,
            kind: kind.into(),
            owner: owner.into(),
            spec,
            body,
            created: stamp.clone(),
            stamp,
        })
    }
    pub fn record(&self, kind: &str, owner: &str, body: Value) -> Result<Doc> {
        self.create(
            kind,
            &id(&kind.to_ascii_lowercase()),
            owner,
            body.clone(),
            body,
        )
    }
    pub fn update(&self, d: &Doc, spec: Value, body: Value) -> Result<Doc> {
        self.get(&d.id)?.cas(&d.version())?;
        self.insert(Doc {
            rev: d.rev + 1,
            generation: d.generation + i64::from(d.spec != spec),
            stamp: now(),
            spec,
            body,
            ..d.clone()
        })
    }
    pub fn status(&self, d: &Doc, body: Value) -> Result<Doc> {
        self.update(d, d.spec.clone(), body)
    }
    fn insert(&self, mut d: Doc) -> Result<Doc> {
        let (schema, version) = match d.kind.as_str() {
            "Task" => ("task", "task_revision"),
            "Run" => ("run", ""),
            "Volume" => ("volume", "resource_version"),
            "VolumeAttachment" => ("volume-attachment", ""),
            "Operation" => ("operation", "operation_revision"),
            "OperationPlan" => ("operation-plan", ""),
            "Effect" => ("effect-record", ""),
            "ExecutionPermit" => ("execution-permit", ""),
            "ApprovalRequest" => ("approval-request", ""),
            "ApprovalDecision" => ("approval-decision", ""),
            "ApprovalUse" => ("approval-use", "resource_version"),
            "Capability" => ("capability-descriptor", ""),
            "ApplicationRelease" => ("application-manifest", ""),
            "ApplicationReleaseSpec" => ("application-release-spec", ""),
            "Revocation" => ("revocation-result", ""),
            _ => ("", ""),
        };
        if !version.is_empty() {
            d.body[version] = json!(d.version())
        }
        if !schema.is_empty() {
            contract(schema, &d.body)?
        }
        self.c.execute("INSERT INTO versions(id,rev,gen,kind,owner,spec,body,created,stamp,event_id,correlation,incarnation)
          VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![d.id,d.rev,d.generation,d.kind,d.owner,d.spec,d.body,d.created,d.stamp,id("event"),self.correlation,self.incarnation])?;
        Ok(d)
    }
    pub fn audit(
        &self,
        actor: &str,
        method: &str,
        target: Option<&str>,
        decision: &str,
    ) -> Result<()> {
        self.c.execute("INSERT INTO audit(at,actor,method,resource,decision,request) VALUES(?1,?2,?3,?4,?5,?6)",params![now(),actor,method,target,decision,self.correlation])?;
        Ok(())
    }
}

#[cfg(test)]
mod state_lock_tests {
    use super::*;

    #[test]
    fn closing_store_releases_lock_even_if_a_fork_retains_the_file_description() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::local(&dir.path().join("state"));
        Store::init(&cfg).unwrap();
        let store = Store::open(cfg.clone()).unwrap();
        let inherited = store._lock.0.try_clone().unwrap();
        assert!(matches!(Store::open(cfg.clone()), Err(e) if e.0.code == "Conflict"));
        drop(store);
        let reopened = Store::open(cfg.clone()).unwrap();
        drop(inherited);
        assert!(matches!(Store::open(cfg), Err(e) if e.0.code == "Conflict"));
        drop(reopened);
    }
}
