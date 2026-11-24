//! Single-node directory volumes. A writer or an uncertain operation retains the volume hold.
pub mod files;
mod staging;
use crate::{
    config::{Config, new_file, private_dir, replace},
    db::{Db, Doc},
    host::{Action, Work},
    policy, *,
};
use psios_api::{VERSION, canonical, sha256};
use rusqlite::{OptionalExtension, params};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags, openat2, statat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub(crate) use staging::reclaim_on_startup;
use std::{
    fs::{self, File, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_BACKUP: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
// A capture permits 33 components of 240 bytes per path. Quotes and backslashes
// can double their JSON representation. Keep room for digests and manifest metadata.
const MAX_MANIFEST: u64 = 72 * 1024 * 1024;
pub fn filename(s: &str) -> Result<()> {
    require(
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.contains('/')
            && s.len() <= 240
            && !s.chars().any(char::is_control),
        "InvalidArgument",
        "expected a single safe file name",
    )
}
fn unknown(e: impl std::fmt::Display) -> Error {
    err(
        "OutcomeUnknown",
        format!("storage receipt unavailable: {e}"),
    )
}
fn registered_root(d: &Doc, meta: &fs::Metadata) -> Result<()> {
    require(
        meta.is_dir()
            && d.spec["device"].as_str().and_then(|v| v.parse().ok()) == Some(meta.dev())
            && d.spec["inode"].as_str().and_then(|v| v.parse().ok()) == Some(meta.ino()),
        "PreconditionFailed",
        "volume root changed",
    )
}

pub fn volume_dir(cfg: &Config, d: &Doc) -> Result<PathBuf> {
    require(
        d.body["lifecycle"] == "Available",
        "PreconditionFailed",
        "volume unavailable",
    )?;
    let dir = cfg.state_dir.join("volumes").join(&d.id);
    private_dir(&dir, false)?;
    let marker = fs::read(dir.join("identity"))?;
    require(
        marker == canonical(&json!([d.id, d.body["data_incarnation"]])),
        "PreconditionFailed",
        "volume identity mismatch",
    )?;
    let data = dir.join("data");
    let meta = fs::symlink_metadata(&data)?;
    registered_root(d, &meta)?;
    Ok(data)
}
pub fn reserve(db: &Db<'_>, w: &Work) -> Result<()> {
    if matches!(w.action, Action::Create { .. } | Action::Restore(_)) {
        require(
            db.get("local")?.body["allocation_enabled"] == true,
            "PreconditionFailed",
            "pool allocation disabled",
        )?;
    }
    if w.target.kind == "Backup" {
        require(
            w.target.body["lifecycle"] == "Stored",
            "PreconditionFailed",
            "backup is not available",
        )?;
        require(
            db.c.execute(
                "INSERT OR IGNORE INTO holds VALUES(?1,?2)",
                params![w.target.id, w.op.id],
            )? == 1,
            "Conflict",
            "backup is held by another operation",
        )?;
    }
    // The application hold survives daemon shutdown and is released only after verified stop.
    if let Some(v) = &w.volume {
        if v.spec["storage_restore_phase"] == "AwaitingActivation" {
            require(
                matches!(
                    w.action,
                    Action::Read(_)
                        | Action::Chunk(_)
                        | Action::Files(_)
                        | Action::Activate(_)
                        | Action::Delete(_)
                ),
                "PreconditionFailed",
                "restored volume requires activation",
            )?;
        }
        if matches!(w.action, Action::Activate(_)) {
            require(
                v.spec["storage_restore_phase"] == "AwaitingActivation",
                "PreconditionFailed",
                "volume is not awaiting activation",
            )?;
        }
        let holder: Option<String> =
            db.c.query_row("SELECT holder FROM holds WHERE volume=?1", [&v.id], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(h) = holder {
            require(
                matches!(
                    w.action,
                    Action::Stop(_) | Action::Health(_) | Action::Logs(_)
                ) && h == w.target.id
                    || w.start_checkpoint.as_ref().is_some_and(|c| {
                        c.body["start_operation_id"] == h && c.body["volume_ref"] == v.reference()
                    }),
                "Conflict",
                "volume held by workload or unresolved operation",
            )?;
        } else {
            db.c.execute("INSERT INTO holds VALUES(?1,?2)", params![v.id, w.op.id])?;
        }
        if matches!(w.action, Action::Delete(_)) {
            let referenced:bool=db.c.query_row("SELECT EXISTS(SELECT 1 FROM current WHERE
              (kind='Backup' AND body->>'lifecycle'!='Deleted' AND json_extract(spec,'$.source_volume')=?1)
              OR (kind='Application' AND json_extract(spec,'$.volume_id')=?1 AND json_extract(body,'$.lifecycle')!='Uninstalled'))",[&v.id],|r|r.get(0))?;
            require(
                !referenced,
                "PreconditionFailed",
                "volume retained by application or backup",
            )?;
        }
    }
    // Serialize mutations of an app even when it has no data volume.
    if w.target.kind == "Application" && !matches!(w.action, Action::Health(_) | Action::Logs(_)) {
        require(
            db.c.execute(
                "INSERT OR IGNORE INTO holds VALUES(?1,?2)",
                params![w.target.id, w.op.id],
            )? == 1,
            "Conflict",
            "application has unresolved effect",
        )?;
    }
    Ok(())
}
fn create_volume(w: &Work, restore: Option<&Doc>) -> Result<Value> {
    let vid = format!("volume-{}", w.op.id);
    let inc = format!("data-{}", w.op.id);
    let parent = w.cfg.state_dir.join("volumes");
    let final_path = parent.join(&vid);
    require(
        !final_path.try_exists()?,
        "Conflict",
        "volume operation identity already present",
    )?;
    let stage = staging::create(&parent)?;
    let data = stage.path().join("data");
    private_dir(&data, true)?;
    if let Some(backup) = restore {
        inspect_backup(&w.cfg, backup, Some(&data))?;
    }
    new_file(
        &stage.path().join("identity"),
        &canonical(&json!([vid, inc])),
    )?;
    let m = fs::symlink_metadata(&data)?;
    let stats = rustix::fs::statvfs(&data).map_err(std::io::Error::from)?;
    let capacity = stats.f_blocks.saturating_mul(stats.f_frsize);
    File::open(&data)?.sync_all()?;
    File::open(stage.path())?.sync_all()?;
    fs::rename(stage.path(), &final_path)?;
    File::open(&parent)?.sync_all().map_err(unknown)?;
    Ok(json!({
        "id": vid,
        "incarnation": inc,
        "device": m.dev().to_string(),
        "inode": m.ino().to_string(),
        "capacity": capacity.to_string()
    }))
}
fn child(dir: &File, name: impl rustix::path::Arg, flags: OFlags) -> Result<File> {
    // Linux openat2 rejects NONBLOCK on O_PATH descriptors.
    let flags = if flags.contains(OFlags::PATH) {
        flags
    } else {
        flags | OFlags::NONBLOCK
    };
    Ok(File::from(
        openat2(
            dir,
            name,
            flags | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(std::io::Error::from)?,
    ))
}
fn regular(m: &fs::Metadata) -> Result<()> {
    require(
        m.is_file() && m.nlink() == 1,
        "UnsupportedCapability",
        "only private regular files are supported",
    )
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    mode: u32,
    bytes: u64,
    digest: Option<String>,
}
// Stored at the manifest top level so older readers can still verify and
// restore the original file/permission profile without decoding new entries.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct ModifiedTime {
    seconds: i64,
    nanoseconds: u32,
}
impl ModifiedTime {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self {
            seconds: meta.mtime(),
            nanoseconds: meta.mtime_nsec() as u32,
        }
    }
    fn restore(self, file: &File) -> Result<()> {
        use rustix::fs::{Timestamps, UTIME_OMIT, futimens};
        use rustix::time::Timespec;
        let times = Timestamps {
            last_access: Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            last_modification: Timespec {
                tv_sec: self.seconds,
                tv_nsec: self.nanoseconds.into(),
            },
        };
        futimens(file, &times).map_err(std::io::Error::from)?;
        require(
            Self::from_metadata(&file.metadata()?) == self,
            "UnsupportedCapability",
            "destination cannot represent the backup modification time",
        )
    }
}
#[derive(Serialize, Deserialize)]
struct BackupTimes {
    root: ModifiedTime,
    entries: Vec<ModifiedTime>,
}

struct Capture<'a> {
    out: &'a Path,
    entries: Vec<(Entry, fs::Metadata)>,
    total: u64,
}
// Reading can advance atime. All identity, content and permission evidence
// used for this local backup profile must still match at publication.
fn same_source(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && (a.mtime(), a.mtime_nsec()) == (b.mtime(), b.mtime_nsec())
        && (a.ctime(), a.ctime_nsec()) == (b.ctime(), b.ctime_nsec())
}
impl Capture<'_> {
    fn walk(&mut self, dir: &File, root: &Path, depth: u32) -> Result<()> {
        require(depth <= 32, "QuotaExceeded", "backup directory depth limit")?;
        let mut names = Vec::new();
        for e in Dir::read_from(dir).map_err(std::io::Error::from)? {
            let e = e.map_err(std::io::Error::from)?;
            let name = e
                .file_name()
                .to_str()
                .map_err(|_| err("UnsupportedCapability", "non-UTF8 backup name"))?;
            if name == "." || name == ".." {
                continue;
            }
            filename(name)?;
            names.push(name.to_owned());
            require(
                names.len() + self.entries.len() <= MAX_ENTRIES,
                "QuotaExceeded",
                "backup entry limit",
            )?;
        }
        names.sort();
        for name in names {
            require(
                self.entries.len() < MAX_ENTRIES,
                "QuotaExceeded",
                "backup entry limit",
            )?;
            let path = root.join(&name);
            let rel = path.to_str().unwrap();
            let stat = statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .map_err(std::io::Error::from)?;
            let ty = FileType::from_raw_mode(stat.st_mode);
            require(
                matches!(ty, FileType::Directory | FileType::RegularFile),
                "UnsupportedCapability",
                "unsupported file type in volume",
            )?;
            let mut src = child(
                dir,
                &name,
                if ty == FileType::Directory {
                    OFlags::RDONLY | OFlags::DIRECTORY
                } else {
                    OFlags::RDONLY
                },
            )?;
            let m = src.metadata()?;
            require(
                m.ino() == stat.st_ino
                    && m.dev() == stat.st_dev
                    && m.uid() == rustix::process::geteuid().as_raw()
                    && m.gid() == rustix::process::getegid().as_raw()
                    && m.mode() & 0o7000 == 0,
                "UnsupportedCapability",
                "unsupported file identity or mode in backup",
            )?;
            use std::os::fd::AsFd;
            require(
                rustix::fs::flistxattr_query(src.as_fd()).is_ok_and(|n| n == 0),
                "UnsupportedCapability",
                "extended attributes require a richer backup profile",
            )?;
            let mut entry = Entry {
                path: rel.into(),
                mode: m.mode() & 0o777,
                bytes: 0,
                digest: None,
            };
            if m.is_dir() {
                self.entries.push((entry, m));
                self.walk(&src, &path, depth + 1)?
            } else {
                regular(&m)?;
                require(
                    m.len() <= MAX_BACKUP - self.total,
                    "QuotaExceeded",
                    "backup exceeds 256 MiB",
                )?;
                let mut dst =
                    File::create_new(self.out.join(format!("blob-{}", self.entries.len())))?;
                let (len, digest) = copy_hash(&mut src, &mut dst, MAX_BACKUP - self.total)?;
                require(
                    len == m.len() && same_source(&m, &src.metadata()?),
                    "Conflict",
                    "volume changed during capture",
                )?;
                dst.sync_all()?;
                entry.digest = Some(digest);
                entry.bytes = len;
                self.total += len;
                self.entries.push((entry, m));
            }
        }
        Ok(())
    }

    fn confirm_source(&self, root: &File, before: &fs::Metadata) -> Result<()> {
        require(
            same_source(before, &root.metadata()?),
            "Conflict",
            "volume directory changed during backup",
        )?;
        for (entry, expected) in &self.entries {
            let (parent, name) = entry_parent(root, &entry.path)?;
            let observed = child(&parent, name, OFlags::PATH)?.metadata()?;
            require(
                same_source(expected, &observed),
                "Conflict",
                "volume entry changed during backup",
            )?;
        }
        require(
            same_source(before, &root.metadata()?),
            "Conflict",
            "volume directory changed during backup",
        )
    }
}
fn copy_hash(r: &mut File, w: &mut impl Write, max: u64) -> Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    let mut len = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        len += n as u64;
        require(len <= max, "QuotaExceeded", "backup byte limit")?;
        hash.update(&buf[..n]);
        w.write_all(&buf[..n])?
    }
    Ok((len, format!("sha256:{:x}", hash.finalize())))
}
fn capture_root(w: &Work) -> Result<File> {
    let path = volume_dir(&w.cfg, &w.target)?;
    let root = File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    registered_root(&w.target, &root.metadata()?)?;
    Ok(root)
}

fn backup(w: &Work) -> Result<Value> {
    let parent = w.cfg.state_dir.join("backups");
    let bid = format!("backup-{}", w.op.id);
    let stage = staging::create(&parent)?;
    let started = now();
    let mut capture = Capture {
        out: stage.path(),
        entries: vec![],
        total: 0,
    };
    let source = capture_root(w)?;
    let before = source.metadata()?;
    capture.walk(&source, Path::new(""), 0)?;
    let manifest = json!({
        "schema_version": VERSION,
        "backup_id": bid,
        "source": w.target.reference(),
        "data_incarnation": w.target.body["data_incarnation"],
        "scope": "single_volume",
        "consistency": "crash_consistent",
        "coordination": "exclusive offline volume hold",
        "started_at": started,
        "completed_at": now(),
        "failure_domain": "local-filesystem",
        "tool_version": env!("CARGO_PKG_VERSION"),
        "mtime_unix_v1": BackupTimes {
            root: ModifiedTime::from_metadata(&before),
            entries: capture.entries.iter().map(|(_, meta)| ModifiedTime::from_metadata(meta)).collect()
        },
        "entries": capture.entries.iter().map(|(entry, _)| entry).collect::<Vec<_>>(),
        "total_bytes": capture.total.to_string()
    });
    let bytes = canonical(&manifest);
    new_file(&stage.path().join("manifest.json"), &bytes)?;
    let current = capture_root(w)?;
    capture.confirm_source(&current, &before)?;
    fs::rename(stage.path(), parent.join(&bid))?;
    File::open(&parent)?.sync_all().map_err(unknown)?;
    Ok(json!({
        "id": bid,
        "manifest_digest": sha256(&bytes),
        "bytes": capture.total.to_string(),
        "entries": capture.entries.len()
    }))
}
#[derive(Deserialize)]
struct BackupManifest {
    backup_id: String,
    source: Value,
    entries: Vec<Entry>,
    total_bytes: String,
    mtime_unix_v1: Option<BackupTimes>,
}

fn backup_files(cfg: &Config, b: &Doc) -> Result<(File, Vec<u8>)> {
    let parent = cfg.state_dir.join("backups");
    private_dir(&parent, false)?;
    let parent = File::open(parent)?;
    let root = child(&parent, &b.id, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let manifest = child(&root, "manifest.json", OFlags::RDONLY)?;
    let meta = manifest.metadata()?;
    regular(&meta)?;
    require(
        meta.len() <= MAX_MANIFEST,
        "QuotaExceeded",
        "backup manifest size limit",
    )?;
    let mut bytes = Vec::new();
    manifest.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= MAX_MANIFEST,
        "QuotaExceeded",
        "backup manifest size limit",
    )?;
    require(
        sha256(&bytes) == b.body["manifest_digest"],
        "PreconditionFailed",
        "backup manifest integrity failed",
    )?;
    Ok((root, bytes))
}

// The workload may have created a valid hierarchy whose full name exceeds PATH_MAX.
fn entry_parent<'a>(root: &File, relative: &'a str) -> Result<(File, &'a str)> {
    let mut parts = relative.split('/');
    let name = parts.next_back().unwrap();
    let mut dir = root.try_clone()?;
    for part in parts {
        dir = child(&dir, part, OFlags::RDONLY | OFlags::DIRECTORY)?;
    }
    Ok((dir, name))
}
// Verification and restore read exactly the same authority-bound recovery set.
fn inspect_backup(cfg: &Config, b: &Doc, destination: Option<&Path>) -> Result<Value> {
    let (root, bytes) = backup_files(cfg, b)?;
    let m: BackupManifest = serde_json::from_slice(&bytes)?;
    require(
        m.backup_id == b.id
            && m.source == b.spec["source_ref"]
            && m.entries.len() <= MAX_ENTRIES
            && b.body["entries"] == m.entries.len()
            && b.body["bytes"] == m.total_bytes,
        "PreconditionFailed",
        "backup manifest disagrees with authority",
    )?;
    if let Some(times) = &m.mtime_unix_v1 {
        require(
            times.entries.len() == m.entries.len()
                && std::iter::once(&times.root)
                    .chain(&times.entries)
                    .all(|t| t.nanoseconds < 1_000_000_000),
            "PreconditionFailed",
            "invalid backup modification times",
        )?;
    }
    let declared = m
        .total_bytes
        .parse::<u64>()
        .ok()
        .filter(|n| *n <= MAX_BACKUP)
        .ok_or_else(|| err("PreconditionFailed", "invalid backup byte count"))?;
    let file_sizes = m
        .entries
        .iter()
        .filter(|entry| entry.digest.is_some())
        .map(|entry| entry.bytes);
    require(
        file_sizes.clone().try_fold(0u64, u64::checked_add) == Some(declared),
        "PreconditionFailed",
        "backup length mismatch",
    )?;
    let destination = destination.map(File::open).transpose()?;
    if let Some(dir) = &destination {
        cfg.check_restore_space(dir, file_sizes, m.entries.len() as u64)?;
    }
    let mut dirs = vec![];
    let mut total = 0;
    for (i, e) in m.entries.iter().enumerate() {
        let mtime = m.mtime_unix_v1.as_ref().map(|times| times.entries[i]);
        for part in e.path.split('/') {
            filename(part)?
        }
        let target = destination
            .as_ref()
            .map(|dir| entry_parent(dir, &e.path))
            .transpose()?;
        if let Some(expected) = &e.digest {
            let mut original = child(&root, format!("blob-{i}"), OFlags::RDONLY)?;
            let meta = original.metadata()?;
            regular(&meta)?;
            require(
                meta.len() == e.bytes && e.bytes <= MAX_BACKUP - total,
                "PreconditionFailed",
                "backup blob length mismatch",
            )?;
            let mut output = if let Some((dir, name)) = target {
                let fd = rustix::fs::openat(
                    &dir,
                    name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::CLOEXEC
                        | OFlags::NOFOLLOW,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(std::io::Error::from)?;
                Some(File::from(fd))
            } else {
                None
            };
            let (len, hash) = match &mut output {
                Some(file) => copy_hash(&mut original, file, e.bytes)?,
                None => copy_hash(&mut original, &mut std::io::sink(), e.bytes)?,
            };
            require(
                hash == *expected && len == e.bytes,
                "PreconditionFailed",
                "backup content integrity failed",
            )?;
            total += len;
            if let Some(f) = output {
                f.set_permissions(Permissions::from_mode(e.mode))?;
                if let Some(time) = mtime {
                    time.restore(&f)?;
                }
                f.sync_all()?;
            }
        } else if let Some((dir, name)) = target {
            rustix::fs::mkdirat(dir, name, Mode::RWXU).map_err(std::io::Error::from)?;
            dirs.push((&e.path, e.mode, mtime))
        }
    }
    require(
        total == declared,
        "PreconditionFailed",
        "backup length mismatch",
    )?;
    if let Some(root) = destination {
        for (path, mode, mtime) in dirs.into_iter().rev() {
            let (parent, name) = entry_parent(&root, path)?;
            let dir = child(&parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
            dir.set_permissions(Permissions::from_mode(mode))?;
            if let Some(time) = mtime {
                time.restore(&dir)?;
            }
            dir.sync_all()?;
        }
        if let Some(times) = &m.mtime_unix_v1 {
            times.root.restore(&root)?;
            root.sync_all()?;
        }
        cfg.check_restore_space(&root, std::iter::empty(), 0)?;
    }
    Ok(
        json!({"backup_ref":b.reference(), "manifest_digest":b.body["manifest_digest"],
              "verified_at":now(), "entries":m.entries.len(), "bytes":m.total_bytes}),
    )
}
// Names are resolved beneath a pinned directory. A nested mount is never traversed.
fn remove_contents(dir: &File, depth: usize, staging: bool) -> Result<()> {
    require(depth < 128, "QuotaExceeded", "delete depth limit")?;
    if staging {
        let meta = dir.metadata()?;
        require(
            meta.uid() == rustix::process::geteuid().as_raw(),
            "PermissionDenied",
            "staged directory ownership changed",
        )?;
        // A restore can have applied a captured read-only mode before it stopped.
        if meta.mode() & 0o700 != 0o700 {
            dir.set_permissions(Permissions::from_mode(0o700))?;
        }
    }
    for entry in Dir::read_from(dir).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let st = statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        let flags = if FileType::from_raw_mode(st.st_mode) == FileType::Directory {
            let subdir = child(dir, name, OFlags::DIRECTORY | OFlags::RDONLY)?;
            remove_contents(&subdir, depth + 1, staging)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(dir, name, flags).map_err(std::io::Error::from)?;
    }
    Ok(())
}
pub fn confirm_write(cfg: &Config, volume: &Doc, name: &str, expected: &str) -> Result<Value> {
    filename(name)?;
    let dir = File::open(volume_dir(cfg, volume)?)?;
    let file = child(&dir, name, OFlags::RDONLY)?;
    regular(&file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    require(
        bytes == expected.as_bytes(),
        "OutcomeUnknown",
        "observed content does not confirm the approved postcondition",
    )?;
    Ok(json!({"name": name, "digest": sha256(&bytes), "verified_at": now()}))
}
pub fn perform(w: &Work) -> Result<Value> {
    match &w.action {
        Action::Files(_)
        | Action::Chunk(_)
        | Action::Append(_)
        | Action::MakeDir(_)
        | Action::RemoveEntry(_) => files::perform(w),
        Action::Activate(_) => Ok(json!({"validated": true})),
        Action::Create { .. } => create_volume(w, None),
        Action::Restore(_) => create_volume(w, Some(&w.target)),
        Action::Backup(_) => backup(w),
        Action::VerifyBackup(_) => inspect_backup(&w.cfg, &w.target, None),
        Action::DeleteBackup(_) => {
            let root = w.cfg.state_dir.join("backups").join(&w.target.id);
            let (dir, _) = backup_files(&w.cfg, &w.target)?;
            remove_contents(&dir, 0, false).map_err(unknown)?;
            fs::remove_dir(&root).map_err(unknown)?;
            File::open(root.parent().unwrap())?
                .sync_all()
                .map_err(unknown)?;
            Ok(json!({"removed": true}))
        }
        Action::Read(f) => {
            let dir = File::open(w.data.as_ref().unwrap())?;
            let file = child(&dir, &f.name, OFlags::RDONLY)?;
            regular(&file.metadata()?)?;
            let mut content = Vec::new();
            file.take(65537).read_to_end(&mut content)?;
            require(
                content.len() <= 65536,
                "QuotaExceeded",
                "file exceeds API read limit",
            )?;
            let text = String::from_utf8(content)
                .map_err(|_| err("UnsupportedCapability", "file read requires UTF-8 data"))?;
            Ok(json!({"name":f.name,"content":text}))
        }
        Action::Write(f) => {
            let path = w.data.as_ref().unwrap().join(&f.name);
            match fs::symlink_metadata(&path) {
                Ok(m) => regular(&m)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            replace(&path, f.content.as_bytes()).map_err(unknown)?;
            Ok(json!({"name": f.name, "digest": sha256(f.content.as_bytes())}))
        }
        Action::Delete(_) => {
            let root = w.data.as_ref().unwrap().parent().unwrap();
            remove_contents(&File::open(root)?, 0, false).map_err(unknown)?;
            fs::remove_dir(root).map_err(unknown)?;
            File::open(root.parent().unwrap())?
                .sync_all()
                .map_err(unknown)?;
            Ok(json!({}))
        }
        _ => Err(err(
            "UnsupportedCapability",
            "unregistered storage operation",
        )),
    }
}
pub fn apply(db: &Db<'_>, w: &Work, r: &Value) -> Result<Value> {
    let actor = &w.task.owner;
    match &w.action {
        Action::Create { .. } | Action::Restore(_) => {
            let profile = db.get(policy::PROFILE)?.reference();
            let id = r["id"].as_str().unwrap();
            let mut spec = json!({"device": r["device"], "inode": r["inode"], "pool_id": "local"});
            match &w.action {
                Action::Create { name } => spec["name"] = json!(name),
                _ => {
                    spec["restored_from"] = w.target.reference();
                    spec["storage_restore_phase"] = json!("AwaitingActivation");
                }
            }
            let b = json!({
                "schema_version": VERSION,
                "volume_id": id,
                "data_incarnation": r["incarnation"],
                "pool_id": "local",
                "access_mode": "single_node_rw",
                "capacity_bytes": r["capacity"],
                "driver_profile_ref": profile,
                "backup_policy_ref": profile,
                "lifecycle": "Available",
                "health": "Healthy",
                "attachment_refs": []
            });
            Ok(db.create("Volume", id, actor, spec, b)?.reference())
        }
        Action::Backup(_) => {
            let b = json!({
                "lifecycle": "Stored",
                "manifest_digest": r["manifest_digest"],
                "entries": r["entries"],
                "bytes": r["bytes"],
                "consistency": "crash_consistent",
                "failure_domain": "local-filesystem",
                "coordination": "exclusive offline volume hold"
            });
            Ok(db
                .create(
                    "Backup",
                    r["id"].as_str().unwrap(),
                    actor,
                    json!({"source_ref": w.target.reference(), "source_volume": w.target.id}),
                    b,
                )?
                .reference())
        }
        Action::Activate(_) => {
            let mut spec = w.target.spec.clone();
            spec["storage_restore_phase"] = json!("Activated");
            Ok(db
                .update(&w.target, spec, w.target.body.clone())?
                .reference())
        }
        Action::DeleteBackup(_) => {
            let mut b = w.target.body.clone();
            b["lifecycle"] = json!("Deleted");
            b["deleted_at"] = json!(now());
            Ok(db.status(&w.target, b)?.reference())
        }
        Action::Delete(_)
        | Action::Write(_)
        | Action::Append(_)
        | Action::MakeDir(_)
        | Action::RemoveEntry(_) => {
            let mut b = w.target.body.clone();
            if matches!(w.action, Action::Delete(_)) {
                b["lifecycle"] = json!("Deleted")
            }
            Ok(json!({"volume_ref": db.status(&w.target,b)?.reference(), "receipt": r}))
        }
        _ => Ok(r.clone()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolConfig {
    id: String,
    expected_resource_version: String,
    allocation_enabled: bool,
}
pub fn pool_configure(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: PoolConfig = decode(p)?;
    let pool = db.of(&p.id, "StoragePool")?;
    pool.cas(&p.expected_resource_version)?;
    let mut b = pool.body.clone();
    b["allocation_enabled"] = json!(p.allocation_enabled);
    Ok(db.update(&pool, b.clone(), b)?.reference())
}
pub fn pool_status(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::can(db, actor, "resource.read")?;
    let p: ById = decode(p)?;
    let pool = db.of(&p.id, "StoragePool")?;
    let stat =
        rustix::fs::statvfs(db.cfg.state_dir.join("volumes")).map_err(std::io::Error::from)?;
    Ok(json!({
        "pool_ref": pool.reference(),
        "allocation_enabled": pool.body["allocation_enabled"],
        "observed_at": now(),
        "capacity_bytes": stat.f_blocks.saturating_mul(stat.f_frsize).to_string(),
        "available_bytes": stat.f_bavail.saturating_mul(stat.f_frsize).to_string(),
        "failure_domain": "local-filesystem",
        "hard_volume_quota": "Unsupported"
    }))
}

#[cfg(test)]
mod capture_tests {
    use super::*;
    use std::{
        fs::FileTimes,
        time::{Duration, UNIX_EPOCH},
    };

    #[test]
    fn publication_rechecks_each_previously_copied_entry() {
        let source = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let root = File::open(source.path()).unwrap();
        let nested = source.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("record");
        fs::write(&path, b"old").unwrap();
        let original_time = UNIX_EPOCH + Duration::new(1_500_000_000, 123_000_000);
        let record = File::options().write(true).open(&path).unwrap();
        record
            .set_times(FileTimes::new().set_modified(original_time))
            .unwrap();
        let before = root.metadata().unwrap();
        let mut capture = Capture {
            out: staging.path(),
            entries: vec![],
            total: 0,
        };
        capture.walk(&root, Path::new(""), 0).unwrap();
        capture.confirm_source(&root, &before).unwrap();

        fs::write(&path, b"new").unwrap();
        record
            .set_times(FileTimes::new().set_modified(original_time + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(
            record.metadata().unwrap().mtime_nsec(),
            capture.entries[1].1.mtime_nsec()
        );
        assert_eq!(
            capture.confirm_source(&root, &before).unwrap_err().0.code,
            "Conflict"
        );
    }

    #[test]
    fn directory_membership_is_rechecked_even_for_empty_directories() {
        for nested_change in [true, false] {
            let source = tempfile::tempdir().unwrap();
            let staging = tempfile::tempdir().unwrap();
            let nested = source.path().join("empty");
            fs::create_dir(&nested).unwrap();
            let root = File::open(source.path()).unwrap();
            let changed = if nested_change {
                nested.as_path()
            } else {
                source.path()
            };
            File::open(changed)
                .unwrap()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_500_000_000))
                .unwrap();
            let before = root.metadata().unwrap();
            let mut capture = Capture {
                out: staging.path(),
                entries: vec![],
                total: 0,
            };
            capture.walk(&root, Path::new(""), 0).unwrap();
            capture.confirm_source(&root, &before).unwrap();
            fs::write(changed.join("added"), b"new").unwrap();
            assert_eq!(
                capture.confirm_source(&root, &before).unwrap_err().0.code,
                "Conflict"
            );
        }
    }
}
