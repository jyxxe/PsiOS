//! Private file storage for immutable managed OCI archives. Authority lives in SQLite.

use super::staging;
use crate::{Result, artifact, config::private_dir, err, require};
use psios_api::sha256;
use rustix::fs::{
    AtFlags, Dir, Mode, OFlags, ResolveFlags, mkdirat, open, openat2, statat, unlinkat,
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{File, Metadata, Permissions},
    io::{self, Read, Seek, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(crate) const ROOT: &str = "oci-archives";
const PAYLOAD: &str = "payload";
pub(crate) const DEFAULT_CAPTURE_BYTES: u32 = 8 * 1024 * 1024;
pub(crate) const MAX_BYTES: u32 = 32 * 1024 * 1024;

pub(crate) struct Depot {
    path: PathBuf,
    root: File,
}

pub(crate) struct Staged {
    stage: staging::Stage,
    file: File,
}

pub(crate) struct Sealed {
    stage: staging::Stage,
    payload: PayloadStamp,
    chunks: Vec<String>,
    digest: String,
}

#[derive(PartialEq, Eq)]
struct PayloadStamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl From<Metadata> for PayloadStamp {
    fn from(meta: Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}

fn valid_id(id: &str) -> bool {
    id.strip_prefix("artifact-").is_some_and(|uuid| {
        uuid.len() == 36
            && uuid.bytes().enumerate().all(|(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            })
    })
}

fn require_id(id: &str) -> Result<()> {
    require(
        valid_id(id),
        "InvalidArgument",
        "invalid OCI archive artifact ID",
    )
}

fn require_size(size: u64) -> Result<()> {
    require(
        (1..=u64::from(MAX_BYTES)).contains(&size),
        "QuotaExceeded",
        "OCI archive size is outside the managed payload limit",
    )
}

fn open_relative(parent: &File, name: &str, flags: OFlags, mode: Mode) -> io::Result<File> {
    let file = openat2(
        parent,
        name,
        flags | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        mode,
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )
    .map_err(io::Error::from)?;
    Ok(File::from(file))
}

fn directory(parent: &File, name: &str) -> io::Result<File> {
    open_relative(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
}

fn owned_dir(dir: &File) -> Result<()> {
    let meta = dir.metadata()?;
    require(
        meta.is_dir()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.mode() & 0o077 == 0,
        "PermissionDenied",
        "OCI archive requires a private owned directory",
    )
}

fn state_directory(path: &Path) -> Result<File> {
    private_dir(path, false)?;
    let state = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    owned_dir(&state)?;
    Ok(state)
}

fn matching_entry(parent: &File, name: &str, file: &File) -> Result<()> {
    let expected = file.metadata()?;
    let actual = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    require(
        actual.st_dev == expected.dev() && actual.st_ino == expected.ino(),
        "RecoveryRequired",
        "OCI archive entry changed identity",
    )
}

fn private_payload(file: &File, readonly: bool) -> Result<PayloadStamp> {
    let meta = file.metadata()?;
    let mode = meta.mode() & 0o7777;
    require(
        meta.is_file()
            && meta.uid() == rustix::process::geteuid().as_raw()
            && meta.nlink() == 1
            && (mode == 0o400 || !readonly && mode == 0o600),
        "RecoveryRequired",
        "OCI archive payload must be a private, single-link regular file",
    )?;
    Ok(meta.into())
}

fn payload(dir: &File, size: u64) -> Result<File> {
    let file = open_relative(dir, PAYLOAD, OFlags::RDONLY, Mode::empty()).map_err(|_| {
        err(
            "RecoveryRequired",
            "OCI archive payload is missing or unsafe",
        )
    })?;
    require(
        private_payload(&file, true)?.size == size,
        "RecoveryRequired",
        "OCI archive payload length disagrees with its reference",
    )?;
    Ok(file)
}

// An interrupted creation or cleanup can leave an empty private directory.
fn only_payload(dir: &File) -> Result<bool> {
    let mut found = false;
    for entry in Dir::read_from(dir).map_err(io::Error::from)? {
        let entry = entry.map_err(io::Error::from)?;
        match entry.file_name().to_bytes() {
            b"." | b".." => (),
            name if name == PAYLOAD.as_bytes() && !found => found = true,
            _ => {
                return Err(err(
                    "RecoveryRequired",
                    "OCI archive has an unexpected entry",
                ));
            }
        }
    }
    Ok(found)
}

fn copy_exact(
    mut input: impl Read,
    size: u64,
    digest: &str,
    mut output: impl Write,
) -> Result<Vec<String>> {
    let mut remaining = size;
    let mut buffer = vec![0; (size as usize).min(artifact::CHUNK_BYTES)];
    let mut content = Sha256::new();
    let mut chunks = Vec::new();
    while remaining != 0 {
        let count = (remaining as usize).min(buffer.len());
        let data = &mut buffer[..count];
        input.read_exact(data).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                err("RecoveryRequired", "OCI archive payload was truncated")
            } else {
                e.into()
            }
        })?;
        content.update(&*data);
        chunks.push(sha256(data));
        output.write_all(data)?;
        remaining -= count as u64;
    }
    match input.read_exact(&mut [0]) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => (),
        Err(e) => return Err(e.into()),
        Ok(()) => {
            return Err(err(
                "RecoveryRequired",
                "OCI archive payload exceeds its reference",
            ));
        }
    }
    require(
        format!("sha256:{:x}", content.finalize()) == digest,
        "RecoveryRequired",
        "OCI archive content digest mismatch",
    )?;
    Ok(chunks)
}

impl Depot {
    /// Open the depot for reading, without creating or recovering anything.
    pub(crate) fn open(state: &Path) -> Result<Option<Self>> {
        let parent = state_directory(state)?;
        let root = match directory(&parent, ROOT) {
            Ok(root) => root,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        owned_dir(&root)?;
        Ok(Some(Self {
            path: state.join(ROOT),
            root,
        }))
    }

    /// Ensure the depot exists under a private state directory and sync its parent.
    pub(crate) fn create(state: &Path) -> Result<Self> {
        let parent = state_directory(state)?;
        match mkdirat(&parent, ROOT, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => (),
            Err(e) => return Err(io::Error::from(e).into()),
        }
        let root = directory(&parent, ROOT)?;
        owned_dir(&root)?;
        parent.sync_all()?;
        Ok(Self {
            path: state.join(ROOT),
            root,
        })
    }

    pub(crate) fn stage(&self, preflight: impl FnOnce(&File) -> Result<()>) -> Result<Staged> {
        let expected = self.root.metadata()?;
        let stage = staging::create(&self.path, |root| {
            let found = root.metadata()?;
            require(
                (found.dev(), found.ino()) == (expected.dev(), expected.ino()),
                "RecoveryRequired",
                "OCI archive depot changed identity",
            )?;
            preflight(root)
        })?;
        let file = open_relative(
            stage.directory(),
            PAYLOAD,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        )?;
        Ok(Staged { stage, file })
    }

    fn archive(&self, artifact: &str) -> Result<File> {
        require_id(artifact)?;
        let dir = directory(&self.root, artifact).map_err(|_| {
            err(
                "RecoveryRequired",
                "OCI archive directory is missing or unsafe",
            )
        })?;
        owned_dir(&dir)?;
        Ok(dir)
    }

    pub(crate) fn open_payload(&self, artifact: &str, size: u64) -> Result<File> {
        require_size(size)?;
        payload(&self.archive(artifact)?, size)
    }

    /// Stream an exact, verified copy into a staging writer. On error the writer
    /// may contain partial bytes and must not be published by the caller.
    pub(crate) fn copy_payload(
        &self,
        artifact: &str,
        size: u64,
        digest: &str,
        output: impl Write,
    ) -> Result<Vec<String>> {
        require_size(size)?;
        let dir = self.archive(artifact)?;
        require(
            only_payload(&dir)?,
            "RecoveryRequired",
            "OCI archive payload is missing",
        )?;
        let mut input = payload(&dir, size)?;
        let before = private_payload(&input, true)?;
        let chunks = copy_exact(&mut input, size, digest, output)?;
        require(
            private_payload(&input, true)? == before,
            "RecoveryRequired",
            "OCI archive payload changed during verification",
        )?;
        matching_entry(&dir, PAYLOAD, &input)?;
        matching_entry(&self.root, artifact, &dir)?;
        Ok(chunks)
    }

    /// Detached checkpoints have a closed set of archive IDs. Live depots may
    /// contain abandoned stages or already-withdrawn payloads after a crash.
    pub(crate) fn check_exact(&self, live: &BTreeSet<String>) -> Result<()> {
        let mut observed = BTreeSet::new();
        for entry in Dir::read_from(&self.root).map_err(io::Error::from)? {
            let entry = entry.map_err(io::Error::from)?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| err("RecoveryRequired", "invalid checkpoint archive name"))?;
            if !matches!(name, "." | "..") {
                require(
                    valid_id(name) && live.contains(name) && observed.insert(name.to_owned()),
                    "RecoveryRequired",
                    "unlisted archive in checkpoint depot",
                )?;
                let dir = self.archive(name)?;
                require(
                    only_payload(&dir)?,
                    "RecoveryRequired",
                    "checkpoint archive payload is missing",
                )?;
                let file = inspect_payload(&dir, true)?;
                matching_entry(&dir, PAYLOAD, &file)?;
                matching_entry(&self.root, name, &dir)?;
            }
        }
        require(
            &observed == live,
            "RecoveryRequired",
            "checkpoint archive set is incomplete",
        )
    }

    /// Reclaim only with an exact live set selected by witness recovery and
    /// exclusive authority access before workers are allowed to stage archives.
    pub(crate) fn reclaim(&self, live: &BTreeSet<String>) -> Result<usize> {
        for id in live {
            require_id(id)?;
        }
        let mut candidates = Vec::new();
        for entry in Dir::read_from(&self.root).map_err(io::Error::from)? {
            let entry = entry.map_err(io::Error::from)?;
            let Ok(name) = entry.file_name().to_str() else {
                continue;
            };
            if stage_name(name) || valid_id(name) && !live.contains(name) {
                require(
                    candidates.len() < 16_384,
                    "QuotaExceeded",
                    "too many abandoned OCI archives",
                )?;
                candidates.push(name.to_owned());
            }
        }
        let mut removed = 0;
        for name in &candidates {
            match self.remove_entry(name, !stage_name(name)) {
                Ok(cleaned) => removed += usize::from(cleaned),
                Err(e) if matches!(e.0.code.as_str(), "PermissionDenied" | "RecoveryRequired") => {
                    eprintln!(
                        "psiosd skipped unsafe OCI archive entry {name}: {}",
                        e.0.message
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }

    /// Remove the exact archive only after witnessed logical withdrawal committed.
    pub(crate) fn retire(&self, artifact: &str) -> Result<()> {
        require_id(artifact)?;
        self.remove_entry(artifact, true)?;
        Ok(())
    }

    fn remove_entry(&self, name: &str, readonly: bool) -> Result<bool> {
        let dir = match directory(&self.root, name) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.root.sync_all()?;
                return Ok(false);
            }
            Err(e) => return Err(cleanup_open_error(e)),
        };
        owned_dir(&dir)?;
        matching_entry(&self.root, name, &dir)?;
        if only_payload(&dir)? {
            let file = inspect_payload(&dir, readonly)?;
            matching_entry(&dir, PAYLOAD, &file)?;
            unlinkat(&dir, PAYLOAD, AtFlags::empty()).map_err(io::Error::from)?;
        }
        dir.sync_all()?;
        matching_entry(&self.root, name, &dir)?;
        unlinkat(&self.root, name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        self.root.sync_all()?;
        Ok(true)
    }
}

fn inspect_payload(dir: &File, readonly: bool) -> Result<File> {
    let file = File::from(
        openat2(
            dir,
            PAYLOAD,
            OFlags::PATH | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        )
        .map_err(io::Error::from)
        .map_err(cleanup_open_error)?,
    );
    private_payload(&file, readonly)?;
    Ok(file)
}

fn cleanup_open_error(error: io::Error) -> crate::Error {
    use rustix::io::Errno;
    if matches!(
        error.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::LOOP | Errno::NOTDIR | Errno::XDEV | Errno::ACCESS | Errno::PERM)
    ) {
        err("RecoveryRequired", "OCI archive cleanup entry is unsafe")
    } else {
        error.into()
    }
}

fn stage_name(name: &str) -> bool {
    name.strip_prefix(".stage-").is_some_and(|suffix| {
        suffix.len() == 6 && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

impl Staged {
    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    /// Check and sync the bytes before moving into the authority transaction.
    pub(crate) fn seal(mut self, size: u64, digest: &str) -> Result<Sealed> {
        require_size(size)?;
        let before = private_payload(&self.file, false)?;
        require(
            before.size == size,
            "RecoveryRequired",
            "staged OCI archive length changed",
        )?;
        self.file.rewind()?;
        let chunks = copy_exact(&mut self.file, size, digest, io::sink())?;
        require(
            private_payload(&self.file, false)? == before,
            "RecoveryRequired",
            "staged OCI archive changed during verification",
        )?;
        self.file.set_permissions(Permissions::from_mode(0o400))?;
        self.file.sync_all()?;
        matching_entry(self.stage.directory(), PAYLOAD, &self.file)?;
        let payload = private_payload(&self.file, true)?;
        require(
            payload.size == before.size && payload.modified == before.modified,
            "RecoveryRequired",
            "staged OCI archive changed before sealing",
        )?;
        Ok(Sealed {
            stage: self.stage,
            payload,
            chunks,
            digest: digest.to_owned(),
        })
    }
}

impl Sealed {
    pub(crate) fn size(&self) -> u32 {
        self.payload.size as u32
    }

    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }

    pub(crate) fn chunks(&self) -> &[String] {
        &self.chunks
    }

    /// Publish only after the caller checks current authority. If a subsequent
    /// metadata transaction is lost, recovery decides whether to retain this file.
    pub(crate) fn publish(self, artifact: &str) -> Result<()> {
        require_id(artifact)?;
        require(
            only_payload(self.stage.directory())?,
            "RecoveryRequired",
            "staged OCI archive payload is missing",
        )?;
        let staged = payload(self.stage.directory(), self.payload.size)?;
        require(
            private_payload(&staged, true)? == self.payload,
            "RecoveryRequired",
            "staged OCI archive changed after sealing",
        )?;
        self.stage.publish(artifact)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    const LIVE: &str = "artifact-00000000-0000-4000-8000-000000000001";
    const ORPHAN: &str = "artifact-00000000-0000-4000-8000-000000000002";

    fn private_state(root: &Path, name: &str) -> PathBuf {
        let path = root.canonicalize().unwrap().join(name);
        private_dir(&path, true).unwrap();
        path
    }

    fn sealed(depot: &Depot, bytes: &[u8]) -> Sealed {
        let mut stage = depot.stage(|_| Ok(())).unwrap();
        stage.file().write_all(bytes).unwrap();
        stage.seal(bytes.len() as u64, &sha256(bytes)).unwrap()
    }

    #[test]
    fn publishes_exact_chunks_and_copies_to_a_detached_depot() {
        let temp = tempfile::tempdir().unwrap();
        let state = private_state(temp.path(), "state");
        let detached = private_state(temp.path(), "detached");
        assert!(Depot::open(&state).unwrap().is_none());
        assert!(!state.join(ROOT).exists());

        let depot = Depot::create(&state).unwrap();
        let data = vec![0x5a; artifact::CHUNK_BYTES + 7];
        let size = data.len() as u64;
        let digest = sha256(&data);
        let expected: Vec<_> = data.chunks(artifact::CHUNK_BYTES).map(sha256).collect();
        let archive = sealed(&depot, &data);
        assert_eq!(archive.chunks(), expected);
        assert!(!state.join(ROOT).join(LIVE).exists());
        archive.publish(LIVE).unwrap();

        let opened = Depot::open(&state).unwrap().unwrap();
        let copy = Depot::create(&detached).unwrap();
        let mut stage = copy.stage(|_| Ok(())).unwrap();
        let observed = opened
            .copy_payload(LIVE, size, &digest, stage.file())
            .unwrap();
        assert_eq!(observed, expected);
        stage.seal(size, &digest).unwrap().publish(LIVE).unwrap();
        copy.check_exact(&BTreeSet::from([LIVE.into()])).unwrap();
        let mut payload = copy.open_payload(LIVE, size).unwrap();
        let mut restored = Vec::new();
        payload.read_to_end(&mut restored).unwrap();
        assert_eq!(restored, data);
        assert_eq!(
            payload.metadata().unwrap().permissions().mode() & 0o7777,
            0o400
        );
    }

    #[test]
    fn refuses_conflicting_paths_and_damaged_or_linked_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let state = private_state(temp.path(), "state");
        let depot = Depot::create(&state).unwrap();
        sealed(&depot, b"exact").publish(LIVE).unwrap();
        let path = state.join(ROOT).join(LIVE).join(PAYLOAD);
        let identity = path.metadata().unwrap().ino();
        assert_eq!(
            sealed(&depot, b"other").publish(LIVE).unwrap_err().0.code,
            "Conflict"
        );
        assert_eq!(path.metadata().unwrap().ino(), identity);
        assert_eq!(fs::read(&path).unwrap(), b"exact");
        assert!(depot.open_payload(LIVE, 6).is_err());
        assert!(depot.open_payload("../outside", 5).is_err());

        let copy = state.join("alias");
        fs::hard_link(&path, &copy).unwrap();
        assert!(depot.open_payload(LIVE, 5).is_err());
        fs::remove_file(&copy).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"other").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o400)).unwrap();
        assert!(
            depot
                .copy_payload(LIVE, 5, &sha256(b"exact"), io::sink())
                .is_err()
        );

        fs::rename(&path, &copy).unwrap();
        symlink(&copy, &path).unwrap();
        assert!(depot.open_payload(LIVE, 5).is_err());
        assert!(depot.retire(LIVE).is_err());
        assert!(path.is_symlink());
        assert_eq!(fs::read(&copy).unwrap(), b"other");
    }

    #[test]
    fn detached_archive_set_rejects_unlisted_children_and_unprotected_directories() {
        let temp = tempfile::tempdir().unwrap();
        let state = private_state(temp.path(), "state");
        let depot = Depot::create(&state).unwrap();
        sealed(&depot, b"selected").publish(LIVE).unwrap();
        let live = BTreeSet::from([LIVE.into()]);
        let directory = state.join(ROOT).join(LIVE);
        let extra = directory.join("unlisted");
        fs::write(&extra, b"unlisted data").unwrap();
        assert!(depot.check_exact(&live).is_err());
        fs::remove_file(extra).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o755)).unwrap();
        assert!(depot.check_exact(&live).is_err());
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        depot.check_exact(&live).unwrap();
        let payload = directory.join(PAYLOAD);
        fs::remove_file(&payload).unwrap();
        assert!(depot.check_exact(&live).is_err());
        symlink(&directory, payload).unwrap();
        assert!(depot.check_exact(&live).is_err());
    }

    #[test]
    fn reclaims_only_stages_and_unselected_generated_archives() {
        let temp = tempfile::tempdir().unwrap();
        let state = private_state(temp.path(), "state");
        let depot = Depot::create(&state).unwrap();
        let other = private_state(&state.join(ROOT), "unrelated");
        let unsafe_stage = private_state(&state.join(ROOT), ".stage-qW1eR2");
        crate::config::new_file(&unsafe_stage.join("unlisted"), b"not an archive").unwrap();
        let unsafe_link = state
            .join(ROOT)
            .join("artifact-00000000-0000-4000-8000-000000000003");
        symlink(&other, &unsafe_link).unwrap();
        sealed(&depot, b"selected").publish(LIVE).unwrap();
        sealed(&depot, b"uncommitted").publish(ORPHAN).unwrap();
        let abandoned = private_state(&state.join(ROOT), ".stage-aB1cD2");
        crate::config::new_file(&abandoned.join(PAYLOAD), b"interrupted").unwrap();
        let live = BTreeSet::from([LIVE.into()]);

        let depot = Depot::open(&state).unwrap().unwrap();
        assert!(abandoned.exists());
        assert!(depot.check_exact(&live).is_err());
        assert_eq!(depot.reclaim(&live).unwrap(), 2);
        assert_eq!(depot.reclaim(&live).unwrap(), 0);
        assert!(!abandoned.exists());
        assert!(other.exists());
        assert!(unsafe_link.is_symlink());
        assert_eq!(
            fs::read(unsafe_stage.join("unlisted")).unwrap(),
            b"not an archive"
        );
        assert!(depot.open_payload(ORPHAN, 11).is_err());
        let mut payload = Vec::new();
        depot
            .copy_payload(LIVE, 8, &sha256(b"selected"), &mut payload)
            .unwrap();
        assert_eq!(payload, b"selected");
        depot.retire(LIVE).unwrap();
        depot.retire(LIVE).unwrap();
        assert!(other.exists());
        assert!(depot.open_payload(LIVE, 8).is_err());
    }
}
