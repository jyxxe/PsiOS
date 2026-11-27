//! Single-node directory volumes. A writer or an uncertain operation retains the volume hold.
pub(crate) mod cloning;
mod detached;
pub(crate) mod directory_create;
pub(crate) mod entry_retirement;
pub(crate) mod estimate;
pub(crate) mod export;
pub(crate) mod file_integrity;
pub(crate) mod file_move;
pub mod files;
mod intake;
pub(crate) mod oci_archive;
pub(crate) mod publication;
pub(crate) mod read_holds;
pub(crate) mod reclamation;
pub(crate) mod reconcile_file;
mod restore_proof;
pub(crate) mod restore_publication;
pub(crate) mod retirement;
mod staging;
pub(crate) mod transfer;
pub(crate) mod usage;
use crate::{
    config::{Config, MAX_BACKUP_ENTRIES as MAX_ENTRIES, new_file, private_dir, replace},
    db::{Db, Doc},
    host::{Action, Work},
    policy, *,
};
pub use detached::{inspect_detached_backup, inspect_detached_backup_with_limit};
pub(crate) use intake::recovery::inspect as inspect_backup_intake;
use psios_api::{VERSION, canonical, sha256};
use rusqlite::{OptionalExtension, params};
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, ResolveFlags, open, openat2, statat};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub(crate) use staging::reclaim_on_startup;
use std::{
    collections::BTreeMap,
    fs::{self, File, Permissions},
    io::{Read, Write},
    os::{
        fd::AsFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

// Earlier writers ignore the additive archive field and could certify a
// widened root using a legacy restore proof.
pub(crate) const ROOT_MODE_FORMAT: u32 = crate::node::block_device::retirement::FORMAT + 1;

// Predecessor readers refuse manifests above their fixed 4096-entry limit.
pub(crate) const ENTRY_CAPACITY_FORMAT: u32 =
    crate::apps::restore_archive::MANIFEST_IDENTITY_FORMAT + 1;

const MAX_CAPTURE_DEPTH: u32 = 32;
const MAX_MANIFEST: u64 = 72 * 1024 * 1024;
// Each captured entry also stores its mode, length, digest, and modification time.
const MANIFEST_ENTRY_ALLOWANCE: u64 = 256;
const MANIFEST_HEADER_ALLOWANCE: u64 = 8 * 1024;

fn manifest_entry_bound(path: &str) -> u64 {
    let escapes = path
        .bytes()
        .filter(|byte| matches!(byte, b'"' | b'\\'))
        .count();
    (path.len() + escapes) as u64 + MANIFEST_ENTRY_ALLOWANCE
}

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

fn checked_root_metadata(root: &File, failure: &str) -> Result<fs::Metadata> {
    let metadata = root.metadata()?;
    require(
        metadata.is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.gid() == rustix::process::getegid().as_raw()
            && metadata.mode() & 0o7000 == 0
            && rustix::fs::flistxattr_query(root.as_fd()).is_ok_and(|length| length == 0),
        failure,
        "volume root requires authority ownership, ordinary mode and no extended attributes",
    )?;
    Ok(metadata)
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
    // These operations use original, exact custody instead of acquiring a competing hold.
    if matches!(
        w.action,
        Action::InspectFileRename(_) | Action::DiscardOciCreate(_)
    ) {
        return Ok(());
    }
    if matches!(
        w.action,
        Action::Create { .. } | Action::Restore(_) | Action::CloneVolume(_)
    ) {
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
            let can_read_staged =
                read_holds::reader(&w.action) && !matches!(w.action, Action::EstimateBackup(_));
            require(
                can_read_staged || matches!(w.action, Action::Activate(_) | Action::Delete(_)),
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
        if read_holds::reader(&w.action) {
            require(
                holder.is_none(),
                "Conflict",
                "volume held by workload or unresolved operation",
            )?;
            return read_holds::acquire(db, &v.id, &w.task.id);
        }
        read_holds::ensure_no_readers(db, &v.id)?;
        if matches!(
            w.action,
            Action::RestartApp(_) | Action::InspectOciInstance(_)
        ) {
            require(
                holder.as_deref() == Some(w.target.id.as_str()),
                "RecoveryRequired",
                "running application volume attachment hold is unavailable",
            )?;
        }
        if let Some(h) = holder {
            require(
                matches!(
                    w.action,
                    Action::Stop(_)
                        | Action::ContinueOciStop(_)
                        | Action::Health(_)
                        | Action::Logs(_)
                        | Action::RestartApp(_)
                        | Action::InspectOciInstance(_)
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
            entry_retirement::ensure_no_pending(db, v)?;
            directory_create::ensure_no_pending(db, v)?;
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
    if w.target.kind == "Application"
        && !matches!(
            w.action,
            Action::Health(_)
                | Action::ContinueOciStop(_)
                | Action::Logs(_)
                | Action::InspectOciInstance(_)
                | Action::ListAppReleases(_)
        )
    {
        require(
            db.c.execute(
                "INSERT OR IGNORE INTO holds VALUES(?1,?2)",
                params![w.target.id, w.op.id],
            )? == 1,
            "Conflict",
            "application has unresolved effect",
        )?;
    }
    if let Some(rebind) = &w.volume_rebind {
        read_holds::ensure_no_readers(db, &rebind.restored.id)?;
        require(
            db.c.execute(
                "INSERT OR IGNORE INTO holds VALUES(?1,?2)",
                params![rebind.restored.id, w.op.id],
            )? == 1,
            "Conflict",
            "restored volume held by another operation or workload",
        )?;
    }
    Ok(())
}
pub(crate) fn create_volume(
    w: &Work,
    initialize: Option<impl FnOnce(&File) -> Result<Value>>,
    checkpoint: impl FnOnce(&Value, &restore_publication::Placement) -> Result<()>,
) -> Result<Value> {
    let vid = format!("volume-{}", w.op.id);
    let inc = format!("data-{}", w.op.id);
    let parent = w.cfg.state_dir.join("volumes");
    let final_path = parent.join(&vid);
    require(
        !final_path.try_exists()?,
        "Conflict",
        "volume operation identity already present",
    )?;
    let identity = canonical(&json!([vid, inc]));
    let mut stage = staging::create(&parent, |root| {
        w.cfg
            .check_managed_space(root, std::iter::once(identity.len() as u64), 3)
    })?;
    private_dir(&stage.path().join("data"), true)?;
    let data = child(
        stage.directory(),
        "data",
        OFlags::RDONLY | OFlags::DIRECTORY,
    )?;
    new_file(&stage.path().join("identity"), &identity)?;
    let proof = initialize
        .map(|fill| fill(&data))
        .transpose()?
        .unwrap_or(Value::Null);
    let m = data.metadata()?;
    let stats = rustix::fs::fstatvfs(&data).map_err(std::io::Error::from)?;
    let capacity = stats.f_blocks.saturating_mul(stats.f_frsize);
    data.sync_all()?;
    let receipt = json!({
        "id": vid,
        "incarnation": inc,
        "device": m.dev().to_string(),
        "inode": m.ino().to_string(),
        "capacity": capacity.to_string(),
        "restore_proof": proof
    });
    // The data root must still be accessible with the pinned identity and
    // applied permissions before authority commits the publication checkpoint.
    let current = child(
        stage.directory(),
        "data",
        OFlags::RDONLY | OFlags::DIRECTORY,
    )?;
    require(
        same_source(&m, &current.metadata()?),
        "PreconditionFailed",
        "volume data root changed before publication",
    )?;
    if matches!(w.action, Action::Create { .. }) {
        restore_publication::check_empty_create(&current)?;
    }
    let placement = restore_publication::Placement::staged(&stage)?;
    stage.preserve_for_checkpoint()?;
    if let Err(error) = checkpoint(&receipt, &placement) {
        if error.0.code != "OutcomeUnknown" {
            stage.discard_on_failure();
        }
        return Err(error);
    }
    stage.publish(&vid).map_err(unknown)?;
    Ok(receipt)
}

pub(crate) fn restore(
    work: &Work,
    checkpoint: impl FnOnce(&Value, &restore_publication::Placement) -> Result<()>,
) -> Result<Value> {
    create_volume(
        work,
        Some(|data: &File| {
            Ok(inspect_backup(&work.cfg, &work.target, Some(data))?["restore_proof"].clone())
        }),
        checkpoint,
    )
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

enum CaptureOutput<'a> {
    Archive(RestoreDestination<'a>),
    Tree(RestoreDestination<'a>),
}

impl CaptureOutput<'_> {
    fn destination(&self) -> &RestoreDestination<'_> {
        match self {
            Self::Archive(destination) | Self::Tree(destination) => destination,
        }
    }

    fn file(&self, path: &str, index: usize) -> Result<File> {
        let (parent, name) = match self {
            Self::Archive(output) => (output.directory.try_clone()?, format!("blob-{index}")),
            Self::Tree(output) => {
                let (directory, name) = entry_parent(output.directory, path)?;
                (directory, name.to_owned())
            }
        };
        let fd = rustix::fs::openat(
            &parent,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(std::io::Error::from)?;
        Ok(File::from(fd))
    }
}

enum CaptureMode<'a> {
    Copy(CaptureOutput<'a>),
    DigestOnly,
    MetadataOnly,
}

struct Capture<'a> {
    mode: CaptureMode<'a>,
    entries: Vec<(Entry, fs::Metadata)>,
    total: u64,
    max_bytes: u64,
    max_entries: usize,
    manifest_bound: u64,
    check: &'a mut dyn FnMut() -> Result<()>,
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
impl<'a> Capture<'a> {
    fn new(
        mode: CaptureMode<'a>,
        max_bytes: u64,
        max_entries: usize,
        check: &'a mut dyn FnMut() -> Result<()>,
    ) -> Self {
        Self {
            mode,
            entries: Vec::new(),
            total: 0,
            max_bytes,
            max_entries,
            manifest_bound: MANIFEST_HEADER_ALLOWANCE,
            check,
        }
    }

    fn walk(&mut self, dir: &File, root: &Path, depth: u32) -> Result<()> {
        (self.check)()?;
        require(
            depth <= MAX_CAPTURE_DEPTH,
            "QuotaExceeded",
            "backup directory depth limit",
        )?;
        let mut names = Vec::new();
        for e in Dir::read_from(dir).map_err(std::io::Error::from)? {
            (self.check)()?;
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
                names.len() + self.entries.len() <= self.max_entries,
                "QuotaExceeded",
                "backup entry limit",
            )?;
        }
        names.sort();
        for name in names {
            (self.check)()?;
            require(
                self.entries.len() < self.max_entries,
                "QuotaExceeded",
                "backup entry limit",
            )?;
            let path = root.join(&name);
            let rel = path.to_str().unwrap();
            self.manifest_bound += manifest_entry_bound(rel);
            require(
                self.manifest_bound <= MAX_MANIFEST,
                "QuotaExceeded",
                "backup paths exceed the manifest size limit",
            )?;
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
                let mut copy = None;
                if let CaptureMode::Copy(CaptureOutput::Tree(output)) = &self.mode {
                    require_managed_root_mode(entry.mode)?;
                    output.check_space(std::iter::empty(), 1)?;
                    let (directory, name) = entry_parent(output.directory, rel)?;
                    rustix::fs::mkdirat(&directory, name, Mode::RWXU)
                        .map_err(std::io::Error::from)?;
                    let destination = child(&directory, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
                    copy = Some((destination, entry.mode, ModifiedTime::from_metadata(&m)));
                }
                self.entries.push((entry, m));
                self.walk(&src, &path, depth + 1)?;
                if let Some((destination, mode, time)) = copy {
                    cloning::preserve_metadata(&destination, mode, time)?;
                }
            } else {
                regular(&m)?;
                require(
                    m.len() <= self.max_bytes - self.total,
                    "QuotaExceeded",
                    "backup exceeds its byte limit",
                )?;
                let copied = match &self.mode {
                    CaptureMode::Copy(out) => {
                        let destination = out.destination();
                        destination.check_space(std::iter::once(m.len()), 1)?;
                        let mut dst = out.file(rel, self.entries.len())?;
                        let copied = copy_captured_blob(&mut src, &mut dst, m.len(), self.check)?;
                        if matches!(out, CaptureOutput::Tree(_)) {
                            cloning::preserve_metadata(
                                &dst,
                                entry.mode,
                                ModifiedTime::from_metadata(&m),
                            )?;
                        } else {
                            dst.sync_all()?;
                        }
                        destination.check_space(std::iter::empty(), 0)?;
                        Some(copied)
                    }
                    CaptureMode::DigestOnly => Some(copy_captured_blob(
                        &mut src,
                        &mut std::io::sink(),
                        m.len(),
                        self.check,
                    )?),
                    CaptureMode::MetadataOnly => None,
                };
                let (len, digest) =
                    copied.map_or((m.len(), None), |(len, digest)| (len, Some(digest)));
                require(
                    len == m.len() && same_source(&m, &src.metadata()?),
                    "Conflict",
                    "volume changed during capture",
                )?;
                entry.digest = digest;
                entry.bytes = len;
                self.total += len;
                self.entries.push((entry, m));
            }
        }
        Ok(())
    }

    fn confirm_source(&mut self, root: &File, before: &fs::Metadata) -> Result<()> {
        (self.check)()?;
        require(
            same_source(before, &root.metadata()?),
            "Conflict",
            "volume directory changed during backup",
        )?;
        for (entry, expected) in &self.entries {
            (self.check)()?;
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
fn copy_captured_blob(
    source: &mut File,
    output: &mut impl Write,
    size: u64,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<(u64, String)> {
    copy_hash_checked(source, output, size, check).map_err(|error| {
        if error.0.code == "QuotaExceeded" {
            err("Conflict", "volume file grew during capture")
        } else {
            error
        }
    })
}

pub(crate) fn copy_hash(r: &mut File, w: &mut impl Write, max: u64) -> Result<(u64, String)> {
    copy_hash_checked(r, w, max, &mut || Ok(()))
}

fn copy_hash_checked(
    r: &mut File,
    w: &mut impl Write,
    max: u64,
    check: &mut dyn FnMut() -> Result<()>,
) -> Result<(u64, String)> {
    let mut hash = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    let mut len = 0;
    loop {
        check()?;
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
    open_volume_root(&w.cfg, &w.target)
}

fn open_volume_root(cfg: &Config, volume: &Doc) -> Result<File> {
    let path = volume_dir(cfg, volume)?;
    let root = File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    registered_root(volume, &root.metadata()?)?;
    Ok(root)
}

pub(crate) fn restored_fingerprint(cfg: &Config, volume: &Doc) -> Result<String> {
    usage::fingerprint(&open_rebind_root(cfg, volume)?)
}

fn open_rebind_root(cfg: &Config, volume: &Doc) -> Result<File> {
    let root = open_volume_root(cfg, volume)?;
    if volume.spec.get("cloned_from").is_some() {
        checked_root_metadata(&root, "PreconditionFailed")?;
    }
    Ok(root)
}

pub(crate) fn verify_restored_for_rebind(
    cfg: &Config,
    volume: &Doc,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<String> {
    let root = open_rebind_root(cfg, volume)?;
    let before = usage::fingerprint(&root)?;
    restore_proof::verify_tree_checked(
        &root,
        &volume.spec["restore_proof"],
        || open_rebind_root(cfg, volume),
        check,
    )?;
    require(
        restored_fingerprint(cfg, volume)? == before,
        "Conflict",
        "restored volume changed while checking its backup tree",
    )?;
    Ok(before)
}

pub(crate) fn backup(
    w: &Work,
    checkpoint: impl FnOnce(&Value, &publication::Placement) -> Result<()>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    check()?;
    let source = capture_root(w)?;
    let before = checked_root_metadata(&source, "UnsupportedCapability")?;
    let root_mode = before.mode() & 0o777;
    require_managed_root_mode(root_mode)?;
    let parent = w.cfg.state_dir.join("backups");
    let bid = format!("backup-{}", w.op.id);
    let mut stage = staging::create(&parent, |root| {
        w.cfg.check_managed_space(root, std::iter::empty(), 1)
    })?;
    let started = now();
    let mut capture = Capture::new(
        CaptureMode::Copy(CaptureOutput::Archive(RestoreDestination {
            directory: stage.directory(),
            authority: Some(&w.cfg),
        })),
        w.cfg.backup_max_bytes,
        w.cfg.backup_max_entries,
        check,
    );
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
        "root_mode_unix_v1": root_mode,
        "mtime_unix_v1": BackupTimes {
            root: ModifiedTime::from_metadata(&before),
            entries: capture.entries.iter().map(|(_, meta)| ModifiedTime::from_metadata(meta)).collect()
        },
        "entries": capture.entries.iter().map(|(entry, _)| entry).collect::<Vec<_>>(),
        "total_bytes": capture.total.to_string()
    });
    let bytes = canonical(&manifest);
    require(
        bytes.len() as u64 <= MAX_MANIFEST,
        "QuotaExceeded",
        "backup manifest exceeds its byte limit",
    )?;
    (capture.check)()?;
    w.cfg
        .check_managed_space(stage.directory(), std::iter::once(bytes.len() as u64), 1)?;
    new_file(&stage.path().join("manifest.json"), &bytes)?;
    w.cfg
        .check_managed_space(stage.directory(), std::iter::empty(), 0)?;
    let current = capture_root(w)?;
    checked_root_metadata(&current, "Conflict")?;
    capture.confirm_source(&current, &before)?;
    let receipt = json!({
        "id": bid,
        "manifest_digest": sha256(&bytes),
        "bytes": capture.total.to_string(),
        "entries": capture.entries.len()
    });
    (capture.check)()?;
    let placement = publication::Placement::staged(&stage)?;
    stage.preserve_for_checkpoint()?;
    if let Err(error) = checkpoint(&receipt, &placement) {
        if error.0.code != "OutcomeUnknown" {
            stage.discard_on_failure();
        }
        return Err(error);
    }
    stage.publish(&bid).map_err(unknown)?;
    Ok(receipt)
}
#[derive(Deserialize)]
struct BackupManifest {
    backup_id: String,
    source: Value,
    entries: Vec<Entry>,
    total_bytes: String,
    mtime_unix_v1: Option<BackupTimes>,
    #[serde(default, deserialize_with = "decode_root_mode")]
    root_mode_unix_v1: Option<u32>,
}

fn decode_root_mode<'de, D>(deserializer: D) -> std::result::Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u32::deserialize(deserializer).map(Some)
}

fn open_backup_parent(cfg: &Config) -> Result<File> {
    let path = cfg.state_dir.join("backups");
    private_dir(&path, false)?;
    Ok(File::from(
        open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    ))
}

fn open_backup(cfg: &Config, id: &str) -> Result<(File, File)> {
    let parent = open_backup_parent(cfg)?;
    let root = child(&parent, id, OFlags::RDONLY | OFlags::DIRECTORY)?;
    Ok((parent, root))
}

fn open_manifest(root: &File) -> Result<File> {
    let manifest = child(root, "manifest.json", OFlags::RDONLY)?;
    let meta = manifest.metadata()?;
    regular(&meta)?;
    require(
        meta.len() <= MAX_MANIFEST,
        "QuotaExceeded",
        "backup manifest size limit",
    )?;
    Ok(manifest)
}

fn read_manifest(manifest: &File, digest: &Value) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    manifest.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= MAX_MANIFEST,
        "QuotaExceeded",
        "backup manifest size limit",
    )?;
    require(
        sha256(&bytes) == *digest,
        "PreconditionFailed",
        "backup manifest integrity failed",
    )?;
    Ok(bytes)
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
fn inspect_backup(cfg: &Config, b: &Doc, destination: Option<&File>) -> Result<Value> {
    let (_, root) = open_backup(cfg, &b.id)?;
    let manifest = authority_manifest(
        &open_manifest(&root)?,
        &b.id,
        &b.spec["source_ref"],
        &b.body,
    )?;
    let mut report = inspect_members(
        &root,
        &manifest,
        &b.body["manifest_digest"],
        destination.map(|directory| RestoreDestination {
            directory,
            authority: Some(cfg),
        }),
        MAX_BACKUP_BYTES,
    )?;
    report["backup_ref"] = b.reference();
    Ok(report)
}

fn authority_manifest(
    manifest: &File,
    id: &str,
    source_ref: &Value,
    expected: &Value,
) -> Result<BackupManifest> {
    let bytes = read_manifest(manifest, &expected["manifest_digest"])?;
    let m: BackupManifest = serde_json::from_slice(&bytes)?;
    require(
        m.backup_id == id
            && m.source == *source_ref
            && expected["entries"] == m.entries.len()
            && expected["bytes"] == m.total_bytes,
        "PreconditionFailed",
        "backup manifest disagrees with authority",
    )?;
    Ok(m)
}

struct RestoreDestination<'a> {
    directory: &'a File,
    authority: Option<&'a Config>,
}

impl RestoreDestination<'_> {
    fn check_space(&self, file_sizes: impl Iterator<Item = u64>, entries: u64) -> Result<()> {
        match self.authority {
            Some(config) => config.check_managed_space(self.directory, file_sizes, entries),
            None => db::check_restore_capacity(self.directory, file_sizes, entries, 0),
        }
    }
}

fn file_sizes(m: &BackupManifest) -> impl Iterator<Item = u64> + Clone + '_ {
    m.entries
        .iter()
        .filter(|entry| entry.digest.is_some())
        .map(|entry| entry.bytes)
}

fn validate_members(m: &BackupManifest, max_bytes: u64) -> Result<u64> {
    require(
        m.entries.len() <= MAX_ENTRIES,
        "QuotaExceeded",
        "backup entry limit",
    )?;
    require(
        m.root_mode_unix_v1.is_none_or(|mode| mode <= 0o777),
        "PreconditionFailed",
        "backup data root mode exceeds ordinary permission bits",
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
        .filter(|n| *n <= MAX_BACKUP_BYTES)
        .ok_or_else(|| err("PreconditionFailed", "invalid backup byte count"))?;
    require(
        declared <= max_bytes,
        "QuotaExceeded",
        "backup exceeds its byte limit",
    )?;
    require(
        file_sizes(m).try_fold(0u64, u64::checked_add) == Some(declared),
        "PreconditionFailed",
        "backup length mismatch",
    )?;
    let mut kinds = BTreeMap::new();
    let mut manifest_bound = MANIFEST_HEADER_ALLOWANCE;
    for entry in &m.entries {
        let mut depth = 0;
        for part in entry.path.split('/') {
            filename(part)?;
            depth += 1;
        }
        let directory = entry.digest.is_none();
        require(
            depth <= MAX_CAPTURE_DEPTH + 1
                && entry.mode <= 0o777
                && (!directory || entry.bytes == 0)
                && entry
                    .path
                    .rsplit_once('/')
                    .is_none_or(|(parent, _)| kinds.get(parent) == Some(&true))
                && kinds.insert(entry.path.as_str(), directory).is_none(),
            "PreconditionFailed",
            "invalid backup entry tree",
        )?;
        manifest_bound += manifest_entry_bound(&entry.path);
        require(
            manifest_bound <= MAX_MANIFEST,
            "QuotaExceeded",
            "backup paths exceed the manifest size limit",
        )?;
    }
    Ok(declared)
}

fn backup_blob(root: &File, index: usize, bytes: u64) -> Result<File> {
    let file = child(root, format!("blob-{index}"), OFlags::RDONLY)?;
    let meta = file.metadata()?;
    regular(&meta)?;
    require(
        meta.len() == bytes,
        "PreconditionFailed",
        "backup blob length mismatch",
    )?;
    Ok(file)
}

fn require_managed_root_mode(mode: u32) -> Result<()> {
    // The authority owns staged data and must be able to reopen the root for
    // activation, interrupted publication inspection, and stage disposal.
    require(
        mode & 0o500 == 0o500,
        "UnsupportedCapability",
        "managed data root mode requires owner read and search permissions",
    )
}

fn inspect_members(
    root: &File,
    m: &BackupManifest,
    digest: &Value,
    destination: Option<RestoreDestination<'_>>,
    max_bytes: u64,
) -> Result<Value> {
    let declared = validate_members(m, max_bytes)?;
    if let Some(destination) = &destination {
        if destination.authority.is_some()
            && let Some(mode) = m.root_mode_unix_v1
        {
            require_managed_root_mode(mode)?;
        }
        destination.check_space(file_sizes(m), m.entries.len() as u64)?;
    }
    let mut dirs = vec![];
    let mut total = 0;
    for (i, e) in m.entries.iter().enumerate() {
        let mtime = m.mtime_unix_v1.as_ref().map(|times| times.entries[i]);
        let target = destination
            .as_ref()
            .map(|destination| entry_parent(destination.directory, &e.path))
            .transpose()?;
        if let Some(expected) = &e.digest {
            let mut original = backup_blob(root, i, e.bytes)?;
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
    let mut report = json!({"manifest_digest":digest,
                            "verified_at":now(), "entries":m.entries.len(), "bytes":m.total_bytes});
    if let Some(destination) = destination {
        let root = destination.directory;
        for (path, mode, mtime) in dirs.into_iter().rev() {
            let (parent, name) = entry_parent(root, path)?;
            let dir = child(&parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
            dir.set_permissions(Permissions::from_mode(mode))?;
            if let Some(time) = mtime {
                time.restore(&dir)?;
            }
            dir.sync_all()?;
        }
        if let Some(mode) = m.root_mode_unix_v1 {
            root.set_permissions(Permissions::from_mode(mode))?;
            require(
                root.metadata()?.mode() & 0o7777 == mode,
                "UnsupportedCapability",
                "restore destination did not apply the exact data root mode",
            )?;
        }
        if let Some(times) = &m.mtime_unix_v1 {
            times.root.restore(root)?;
        }
        if m.root_mode_unix_v1.is_some() || m.mtime_unix_v1.is_some() {
            root.sync_all()?;
        }
        destination.check_space(std::iter::empty(), 0)?;
        report["restore_proof"] = serde_json::to_value(restore_proof::from_manifest(m)?)?;
    }
    Ok(report)
}
// Names are resolved beneath a pinned directory. A nested mount is never traversed.
fn remove_contents(dir: &File, depth: usize) -> Result<()> {
    require(depth < 128, "QuotaExceeded", "delete depth limit")?;
    let meta = dir.metadata()?;
    require(
        meta.uid() == rustix::process::geteuid().as_raw(),
        "PermissionDenied",
        "storage cleanup directory ownership changed",
    )?;
    // A restored directory can have retained a captured read-only mode.
    if meta.mode() & 0o700 != 0o700 {
        dir.set_permissions(Permissions::from_mode(0o700))?;
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
            remove_contents(&subdir, depth + 1)?;
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        rustix::fs::unlinkat(dir, name, flags).map_err(std::io::Error::from)?;
    }
    Ok(())
}
pub fn perform(w: &Work) -> Result<Value> {
    match &w.action {
        Action::Files(_) | Action::Chunk(_) | Action::Append(_) => files::perform(w),
        Action::VolumeUsage(query) => usage::measure(w, query),
        Action::Activate(_) => restore_proof::verify(w),
        Action::InspectBackupPublication(_) => publication::observe(w),
        Action::InspectRestoredVolume(_) => restore_publication::observe(w),
        Action::VerifyBackup(_) => inspect_backup(&w.cfg, &w.target, None),
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
            let root = w.data.as_ref().unwrap();
            let directory = File::open(root)?;
            registered_root(&w.target, &directory.metadata()?)?;
            let path = root.join(&f.name);
            match fs::symlink_metadata(&path) {
                Ok(m) => regular(&m)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            w.cfg
                .check_managed_space(&directory, std::iter::once(f.content.len() as u64), 1)?;
            replace(&path, f.content.as_bytes()).map_err(unknown)?;
            Ok(json!({"name": f.name, "digest": sha256(f.content.as_bytes())}))
        }
        _ => Err(err(
            "UnsupportedCapability",
            "unregistered storage operation",
        )),
    }
}
fn catalog_volume(db: &Db<'_>, owner: &str, receipt: &Value, mut spec: Value) -> Result<Value> {
    let profile = db.get(policy::PROFILE)?.reference();
    let id = receipt["id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "volume receipt has no identity"))?;
    spec["device"] = receipt["device"].clone();
    spec["inode"] = receipt["inode"].clone();
    spec["pool_id"] = json!("local");
    let body = json!({
                "schema_version": VERSION,
                "volume_id": id,
                "data_incarnation": receipt["incarnation"],
                "pool_id": "local",
                "access_mode": "single_node_rw",
                "capacity_bytes": receipt["capacity"],
                "driver_profile_ref": profile,
                "backup_policy_ref": profile,
                "lifecycle": "Available",
                "health": "Healthy",
                "attachment_refs": []
    });
    Ok(db.create("Volume", id, owner, spec, body)?.reference())
}

pub fn apply(db: &Db<'_>, w: &Work, r: &Value) -> Result<Value> {
    let actor = &w.task.owner;
    match &w.action {
        Action::Create { .. } | Action::Restore(_) | Action::CloneVolume(_) => {
            restore_publication::settle(db, w, r)
        }
        Action::Backup(_) => publication::settle(db, &w.op, &w.target, r),
        Action::Activate(_) => {
            let mut spec = w.target.spec.clone();
            spec["storage_restore_phase"] = json!("Activated");
            if r["verified_at"].is_string() {
                let mut evidence = json!({
                    "operation_ref": w.op.reference(),
                    "volume_ref": w.target.reference(),
                    "tree_digest": spec["restore_proof"]["tree_digest"],
                    "verified_at": r["verified_at"]
                });
                if let Some(source) = spec.get("cloned_from") {
                    evidence["cloned_from"] = source.clone();
                } else {
                    evidence["backup_ref"] = spec["restored_from"].clone();
                }
                let verification = db.record("RecoveryEvidence", actor, evidence)?;
                spec["restore_verification_ref"] = verification.reference();
            }
            Ok(db
                .update(&w.target, spec, w.target.body.clone())?
                .reference())
        }
        Action::DeleteBackup(_) | Action::Delete(_) => retirement::settle(db, &w.op, &w.target, r),
        Action::RemoveEntry(request) => entry_retirement::settle(db, w, &request.path, r),
        Action::MakeDir(request) => directory_create::settle(db, w, &request.path, r),
        Action::Write(_) | Action::Append(_) | Action::RenameFile(_) => Ok(
            json!({"volume_ref": db.status(&w.target,w.target.body.clone())?.reference(), "receipt": r}),
        ),
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
        "backup_max_bytes": db.cfg.backup_max_bytes.to_string(),
        "backup_max_entries": db.cfg.backup_max_entries,
        "backup_capture_timeout_seconds": db.cfg.backup_capture_timeout_seconds,
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
    fn copying_a_growing_volume_file_stays_within_its_reserved_length() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data");
        fs::write(&path, vec![b'x'; 40 * 1024]).unwrap();
        let mut source = File::open(&path).unwrap();
        let reserved = source.metadata().unwrap().len();
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[b'y'; 1024])
            .unwrap();
        let mut staged = tempfile::tempfile().unwrap();
        let refusal =
            copy_captured_blob(&mut source, &mut staged, reserved, &mut || Ok(())).unwrap_err();
        assert_eq!(refusal.0.code, "Conflict");
        assert!(staged.metadata().unwrap().len() <= reserved);
    }

    #[test]
    fn publication_rechecks_each_previously_copied_entry() {
        let source = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let out = File::open(staging.path()).unwrap();
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
        let mut check = || Ok(());
        let mut capture = Capture::new(
            CaptureMode::Copy(CaptureOutput::Archive(RestoreDestination {
                directory: &out,
                authority: None,
            })),
            MAX_BACKUP_BYTES,
            MAX_ENTRIES,
            &mut check,
        );
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
            let out = File::open(staging.path()).unwrap();
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
            let mut check = || Ok(());
            let mut capture = Capture::new(
                CaptureMode::Copy(CaptureOutput::Archive(RestoreDestination {
                    directory: &out,
                    authority: None,
                })),
                MAX_BACKUP_BYTES,
                MAX_ENTRIES,
                &mut check,
            );
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
