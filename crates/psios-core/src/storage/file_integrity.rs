//! Bounded file observations for a pinned volume data incarnation.
use super::{child, files, registered_root, regular};
use crate::{Result, err, host::Target, host::Work, require};
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
const MAX_BYTES: u64 = 256 * 1024 * 1024;

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
            self.expected_digest.as_deref().is_none_or(|sum| {
                sum.strip_prefix("sha256:").is_some_and(|hex| {
                    hex.len() == 64
                        && hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
            }),
            "InvalidArgument",
            "expected file digest must be lowercase SHA-256",
        )
    }
}

#[derive(PartialEq, Eq, Serialize)]
struct Revision {
    device: String,
    inode: String,
    mode: String,
    links: String,
    size_bytes: String,
    modified: [String; 2],
    changed: [String; 2],
}

impl Revision {
    fn from(m: &Metadata) -> Self {
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
}

fn volume_root(work: &Work) -> Result<File> {
    let path = work
        .data
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "volume data path missing"))?;
    let root = File::from(
        open(
            path,
            OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    registered_root(&work.target, &root.metadata()?)?;
    Ok(root)
}

pub(crate) fn verify(
    work: &Work,
    q: &Query,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    check()?;
    let root = volume_root(work)?;
    let entry = child(&root, &q.path, OFlags::PATH)?;
    let first = entry.metadata()?;
    regular(&first)?;
    require(
        first.len() <= q.max_bytes.unwrap_or(DEFAULT_BYTES),
        "QuotaExceeded",
        "file exceeds the requested inspection byte limit",
    )?;
    let revision = Revision::from(&first);
    // Reopen the exact descriptor after verifying the entry type.
    let mut file = File::from(
        open(
            format!("/proc/self/fd/{}", entry.as_raw_fd()),
            OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    require(
        revision == Revision::from(&file.metadata()?),
        "PreconditionFailed",
        "file changed before verification",
    )?;
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
    let rebound = child(&volume_root(work)?, &q.path, OFlags::PATH)?;
    require(
        revision == Revision::from(&file.metadata()?)
            && revision == Revision::from(&rebound.metadata()?),
        "PreconditionFailed",
        "file identity or content changed during verification",
    )?;
    let sum = format!("sha256:{:x}", hasher.finalize());
    require(
        q.expected_digest
            .as_ref()
            .is_none_or(|expected| *expected == sum),
        "PreconditionFailed",
        "file content digest does not match the request",
    )?;
    let mut observation = json!({
        "volume_ref": work.target.reference(), "data_incarnation": work.target.body["data_incarnation"],
        "path": q.path, "revision": revision, "content_digest": sum,
    });
    observation["observation_digest"] = json!(digest(
        "volume-file-observation",
        "observation_digest",
        &observation
    )?);
    Ok(observation)
}
