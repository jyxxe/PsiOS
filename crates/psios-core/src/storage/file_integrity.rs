//! Bounded file observations for a pinned volume data incarnation.
use super::{child, files, registered_root, regular, volume_dir};
use crate::{Config, Result, db::Doc, err, host::Target, host::Work, require};
use psios_api::digest;
use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, Metadata},
    io::{ErrorKind, Read},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
};

pub(crate) const TOOL: &str = "volume.verify_file";
pub(crate) const FORMAT: u32 = 48;
const DEFAULT_BYTES: u64 = 32 * 1024 * 1024;
pub(super) const MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Query {
    pub target: Target,
    path: String,
    expected_digest: Option<String>,
    max_bytes: Option<u64>,
}

impl Query {
    pub(crate) fn validate(&self) -> Result<()> {
        files::path(&self.path, false)?;
        require(
            (1..=MAX_BYTES).contains(&self.max_bytes.unwrap_or(DEFAULT_BYTES)),
            "InvalidArgument",
            "file inspection limit must be between 1 byte and 256 MiB",
        )?;
        require(
            self.expected_digest.as_deref().is_none_or(sha256_format),
            "InvalidArgument",
            "expected file digest must be lowercase SHA-256",
        )
    }
}

pub(super) fn sha256_format(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Clone, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Revision {
    device: String,
    inode: String,
    mode: String,
    links: String,
    size_bytes: String,
    modified: [String; 2],
    changed: [String; 2],
}

impl Revision {
    pub(super) fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev().to_string(),
            inode: m.ino().to_string(),
            mode: m.mode().to_string(),
            links: m.nlink().to_string(),
            size_bytes: m.len().to_string(),
            modified: [m.mtime().to_string(), m.mtime_nsec().to_string()],
            changed: [m.ctime().to_string(), m.ctime_nsec().to_string()],
        }
    }

    pub(super) fn same_after_rename(&self, after: &Self) -> bool {
        self.device == after.device
            && self.inode == after.inode
            && self.mode == after.mode
            && self.links == after.links
            && self.size_bytes == after.size_bytes
            && self.modified == after.modified
    }
}

pub(super) fn volume_root(config: &Config, volume: &Doc) -> Result<File> {
    let path = volume_dir(config, volume)?;
    let root = File::from(
        open(
            path,
            OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    registered_root(volume, &root.metadata()?)?;
    Ok(root)
}

pub(super) fn revision(root: &File, path: &str) -> Result<Revision> {
    let entry = child(root, path, OFlags::PATH)?;
    let metadata = entry.metadata()?;
    regular(&metadata)?;
    Ok(Revision::from(&metadata))
}

pub(super) fn open_pinned_entry(entry: &File, revision: &Revision) -> Result<File> {
    // Reopen the exact descriptor after the caller verifies the entry type.
    let file = File::from(
        open(
            format!("/proc/self/fd/{}", entry.as_raw_fd()),
            OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    require(
        *revision == Revision::from(&file.metadata()?),
        "PreconditionFailed",
        "file changed before verification",
    )?;
    Ok(file)
}

pub(super) fn capture(
    root: &File,
    path: &str,
    max_bytes: u64,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(Revision, String)> {
    check()?;
    let entry = child(root, path, OFlags::PATH)?;
    let first = entry.metadata()?;
    regular(&first)?;
    require(
        first.len() <= max_bytes,
        "QuotaExceeded",
        "file exceeds the requested inspection byte limit",
    )?;
    let revision = Revision::from(&first);
    let mut file = open_pinned_entry(&entry, &revision)?;
    let mut remaining = first.len();
    let mut buffer = [0; 128 * 1024];
    let mut hasher = Sha256::new();
    while remaining > 0 {
        check()?;
        let bytes = remaining.min(buffer.len() as u64) as usize;
        if let Err(error) = file.read_exact(&mut buffer[..bytes]) {
            return Err(if error.kind() == ErrorKind::UnexpectedEof {
                err("PreconditionFailed", "file shortened during verification")
            } else {
                error.into()
            });
        }
        hasher.update(&buffer[..bytes]);
        remaining -= bytes as u64;
    }
    check()?;
    require(
        revision == Revision::from(&file.metadata()?) && revision == self::revision(root, path)?,
        "PreconditionFailed",
        "file identity or content changed during verification",
    )?;
    Ok((revision, format!("sha256:{:x}", hasher.finalize())))
}

pub(super) fn observation(
    volume: &Doc,
    path: &str,
    revision: &Revision,
    content_digest: &str,
) -> Result<Value> {
    let mut observation = json!({
        "volume_ref": volume.reference(), "data_incarnation": volume.body["data_incarnation"],
        "path": path, "revision": revision, "content_digest": content_digest,
    });
    observation["observation_digest"] = json!(digest(
        "volume-file-observation",
        "observation_digest",
        &observation
    )?);
    Ok(observation)
}

pub(crate) fn verify(
    work: &Work,
    q: &Query,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    let root = volume_root(&work.cfg, &work.target)?;
    let (captured, sum) = capture(&root, &q.path, q.max_bytes.unwrap_or(DEFAULT_BYTES), check)?;
    let current = volume_root(&work.cfg, &work.target)?;
    require(
        captured == revision(&current, &q.path)?,
        "PreconditionFailed",
        "volume file path changed during verification",
    )?;
    require(
        q.expected_digest
            .as_ref()
            .is_none_or(|expected| *expected == sum),
        "PreconditionFailed",
        "file content digest does not match the request",
    )?;
    observation(&work.target, &q.path, &captured, &sum)
}
