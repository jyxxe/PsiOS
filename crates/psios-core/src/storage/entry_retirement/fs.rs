//! Filesystem evidence for one privately retained volume entry.
use super::{MAX_BYTES, operation_name};
use crate::{
    Config, Result,
    db::Doc,
    err, require,
    storage::{child, files, registered_root, volume_dir},
};
use rustix::{
    fs::{
        AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, ResolveFlags, mkdirat, open, openat2,
        renameat_with, statat, unlinkat,
    },
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, Metadata},
    io,
    os::unix::fs::MetadataExt,
};

const DEPOT: &str = ".entry-retirements";
type Identity = [u64; 2];

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    identity: Identity,
    mode: u32,
    owner: [u32; 2],
    links: u64,
    size: u64,
    modified: [i64; 2],
    changed: [i64; 2],
}

impl Entry {
    fn new(meta: &Metadata) -> Self {
        Self {
            identity: [meta.dev(), meta.ino()],
            mode: meta.mode(),
            owner: [meta.uid(), meta.gid()],
            links: meta.nlink(),
            size: meta.len(),
            modified: [meta.mtime(), meta.mtime_nsec()],
            changed: [meta.ctime(), meta.ctime_nsec()],
        }
    }

    fn is_dir(&self) -> bool {
        FileType::from_raw_mode(self.mode) == FileType::Directory
    }

    fn supported(&self) -> bool {
        self.is_dir()
            || (FileType::from_raw_mode(self.mode) == FileType::RegularFile && self.links == 1)
    }

    fn bytes(&self) -> u64 {
        if self.is_dir() { 0 } else { self.size }
    }

    fn same_after_move(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.mode == other.mode
            && self.owner == other.owner
            && self.links == other.links
            && self.size == other.size
            && self.modified == other.modified
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Location {
    wrapper: Identity,
    root: Identity,
    source_parent: Identity,
    depot: Identity,
    entry: Entry,
}

impl Location {
    pub(super) fn check(&self) -> Result<()> {
        let device = self.root[0];
        require(
            [
                self.wrapper,
                self.root,
                self.source_parent,
                self.depot,
                self.entry.identity,
            ]
            .iter()
            .all(|identity| identity[0] == device && identity[1] != 0)
                && self.entry.supported()
                && self.entry.bytes() <= MAX_BYTES,
            "RecoveryRequired",
            "invalid retirement inode evidence",
        )
    }

    pub(super) fn bytes(&self) -> u64 {
        self.entry.bytes()
    }

    pub(super) fn check_volume(&self, volume: &Doc) -> Result<()> {
        require(
            ["device", "inode"]
                .map(|field| volume.spec[field].as_str().and_then(|n| n.parse().ok()))
                == self.root.map(Some),
            "RecoveryRequired",
            "retirement location contradicts the registered Volume root",
        )
    }

    pub(super) fn check_retired(&self, entry: &Entry) -> Result<()> {
        require(
            self.entry.same_after_move(entry),
            "RecoveryRequired",
            "retained entry contradicts the rename witness",
        )
    }
}

#[derive(PartialEq, Eq)]
pub(super) enum Placement {
    Original,
    Retired(Entry),
}

fn identity(file: &File) -> Result<Identity> {
    let meta = file.metadata()?;
    Ok([meta.dev(), meta.ino()])
}

struct Depot {
    wrapper: File,
    root: File,
    directory: File,
}

impl Depot {
    fn open(cfg: &Config, volume: &Doc, create: bool) -> Result<Self> {
        let path = volume_dir(cfg, volume)?;
        let parent = path
            .parent()
            .ok_or_else(|| err("RecoveryRequired", "missing volume wrapper"))?;
        let wrapper = File::from(
            open(
                parent,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let meta = wrapper.metadata()?;
        require(
            meta.uid() == rustix::process::geteuid().as_raw() && meta.mode() & 0o077 == 0,
            "PreconditionFailed",
            "volume wrapper is not private and authority-owned",
        )?;
        let root = child(&wrapper, "data", OFlags::RDONLY | OFlags::DIRECTORY)?;
        registered_root(volume, &root.metadata()?)?;
        if create {
            match mkdirat(&wrapper, DEPOT, Mode::RWXU) {
                Ok(()) => wrapper.sync_all()?,
                Err(Errno::EXIST) => {}
                Err(e) => return Err(io::Error::from(e).into()),
            }
        }
        let directory = open_depot(&wrapper)?;
        Ok(Self {
            wrapper,
            root,
            directory,
        })
    }

    fn check(&self, evidence: &Location) -> Result<()> {
        require(
            identity(&self.wrapper)? == evidence.wrapper
                && identity(&self.root)? == evidence.root
                && identity(&self.directory)? == evidence.depot,
            "PreconditionFailed",
            "entry retirement storage identity changed",
        )
    }
}

fn open_depot(wrapper: &File) -> Result<File> {
    let directory = child(wrapper, DEPOT, OFlags::RDONLY | OFlags::DIRECTORY)?;
    let info = directory.metadata()?;
    require(
        info.mode() & 0o7777 == 0o700 && info.uid() == rustix::process::geteuid().as_raw(),
        "PreconditionFailed",
        "entry retirement directory is not private and authority-owned",
    )?;
    Ok(directory)
}

pub(super) fn guard_volume_retirement(wrapper: &File) -> Result<()> {
    let stat = match statat(wrapper, DEPOT, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => return Ok(()),
        Err(e) => return Err(io::Error::from(e).into()),
        Ok(stat) => stat,
    };
    let depot = open_depot(wrapper)?;
    require(
        identity(&depot)? == [stat.st_dev, stat.st_ino] && !contains_entries(&depot)?,
        "Conflict",
        "volume private retirement directory retains an unaccounted entry",
    )
}

fn sample(parent: &File, name: &str) -> Result<Option<Entry>> {
    let stat = match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => return Ok(None),
        Err(e) => return Err(io::Error::from(e).into()),
        Ok(stat) => stat,
    };
    require(
        matches!(
            FileType::from_raw_mode(stat.st_mode),
            FileType::RegularFile | FileType::Directory
        ),
        "UnsupportedCapability",
        "only private files and empty directories can be removed",
    )?;
    let file = child(parent, name, OFlags::PATH)?;
    let entry = Entry::new(&file.metadata()?);
    require(
        entry.identity == [stat.st_dev, stat.st_ino],
        "PreconditionFailed",
        "volume entry identity changed while opening its descriptor",
    )?;
    Ok(Some(entry))
}

// No recursive read is needed. A single non-dot entry proves a refusal.
fn empty(parent: &File, name: &str, entry: &Entry) -> Result<()> {
    if !entry.is_dir() {
        return Ok(());
    }
    let dir = File::from(
        openat2(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(|error| match error {
            Errno::ACCESS | Errno::PERM => err(
                "UnsupportedCapability",
                "entry retirement requires a readable empty directory",
            ),
            _ => io::Error::from(error).into(),
        })?,
    );
    require(
        Entry::new(&dir.metadata()?) == *entry,
        "PreconditionFailed",
        "entry directory changed before emptiness inspection",
    )?;
    require(
        !contains_entries(&dir)?,
        "PreconditionFailed",
        "only empty directories can be retired",
    )?;
    require(
        Entry::new(&dir.metadata()?) == *entry,
        "PreconditionFailed",
        "entry directory changed during emptiness inspection",
    )
}

fn contains_entries(dir: &File) -> Result<bool> {
    for name in Dir::read_from(dir).map_err(io::Error::from)? {
        let name = name.map_err(io::Error::from)?;
        if !matches!(name.file_name().to_bytes(), b"." | b"..") {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) struct Paths {
    depot: Depot,
    parent: File,
    source: String,
    retired: String,
}

impl Paths {
    pub(super) fn open(
        cfg: &Config,
        volume: &Doc,
        path: &str,
        operation: &str,
        create: bool,
    ) -> Result<Self> {
        files::path(path, false)?;
        let depot = Depot::open(cfg, volume, create)?;
        let (parent, source) = files::parent(&depot.root, path)?;
        let retired = operation_name(operation)?;
        Ok(Self {
            depot,
            parent,
            source,
            retired,
        })
    }

    pub(super) fn prepare(&self) -> Result<Location> {
        let entry = sample(&self.parent, &self.source)?
            .ok_or_else(|| err("NotFound", "volume entry is missing"))?;
        require(
            entry.supported(),
            "UnsupportedCapability",
            "only private files and empty directories can be removed",
        )?;
        require(
            entry.bytes() <= MAX_BYTES,
            "QuotaExceeded",
            "entry exceeds retirement byte capacity",
        )?;
        require(
            sample(&self.depot.directory, &self.retired)?.is_none(),
            "Conflict",
            "entry retirement name already exists",
        )?;
        empty(&self.parent, &self.source, &entry)?;
        let evidence = Location {
            wrapper: identity(&self.depot.wrapper)?,
            root: identity(&self.depot.root)?,
            source_parent: identity(&self.parent)?,
            depot: identity(&self.depot.directory)?,
            entry,
        };
        evidence.check()?;
        self.sync()?;
        Ok(evidence)
    }

    pub(super) fn observe(&self, evidence: &Location) -> Result<Placement> {
        self.depot.check(evidence)?;
        require(
            identity(&self.parent)? == evidence.source_parent,
            "PreconditionFailed",
            "source parent identity changed",
        )?;
        match (
            sample(&self.parent, &self.source)?,
            sample(&self.depot.directory, &self.retired)?,
        ) {
            (Some(source), None) if source == evidence.entry => {
                empty(&self.parent, &self.source, &source)?;
                Ok(Placement::Original)
            }
            (None, Some(retired)) if evidence.entry.same_after_move(&retired) => {
                empty(&self.depot.directory, &self.retired, &retired)?;
                Ok(Placement::Retired(retired))
            }
            _ => Err(err(
                "PreconditionFailed",
                "entry retirement placement cannot be verified",
            )),
        }
    }

    pub(super) fn retire(&self, evidence: &Location) -> Result<()> {
        require(
            self.observe(evidence)? == Placement::Original,
            "PreconditionFailed",
            "source entry is no longer in its approved position",
        )?;
        renameat_with(
            &self.parent,
            self.source.as_str(),
            &self.depot.directory,
            self.retired.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(|e| match e {
            Errno::EXIST => err("Conflict", "entry retirement destination already exists"),
            Errno::XDEV | Errno::NOSYS | Errno::OPNOTSUPP | Errno::INVAL => err(
                "UnsupportedCapability",
                "filesystem cannot retire the entry atomically on the same filesystem",
            ),
            _ => super::super::unknown(e),
        })
    }

    pub(super) fn sync(&self) -> Result<()> {
        self.parent.sync_all()?;
        self.depot.directory.sync_all()?;
        Ok(())
    }
}

// Disposal does not open the old user pathname. New data may use it.
pub(super) fn purge(
    cfg: &Config,
    volume: &Doc,
    operation: &str,
    evidence: &Location,
    retired: &Entry,
) -> Result<bool> {
    let depot = Depot::open(cfg, volume, false)?;
    depot.check(evidence)?;
    let name = operation_name(operation)?;
    let removed = if let Some(found) = sample(&depot.directory, &name)? {
        require(
            found == *retired,
            "PreconditionFailed",
            "retained entry revision changed before disposal",
        )?;
        empty(&depot.directory, &name, &found)?;
        let flags = if found.is_dir() {
            AtFlags::REMOVEDIR
        } else {
            AtFlags::empty()
        };
        unlinkat(&depot.directory, name.as_str(), flags).map_err(io::Error::from)?;
        true
    } else {
        false
    };
    depot.directory.sync_all()?;
    Ok(removed)
}
