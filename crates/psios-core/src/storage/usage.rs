//! Bounded inode accounting for local volumes held exclusively by the Core.
use super::{child, files, parent_from_components, registered_root, same_source, volume_dir};
use crate::{
    Result, err,
    host::{Target, Work},
    now, require,
};
use rustix::fs::{Dir, Mode, OFlags, open};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs::{File, Metadata},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

const ENTRY_LIMIT: usize = 4096;
const DEPTH_LIMIT: u32 = 32;
const BLOCK_BYTES: u128 = 512;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub target: Target,
    #[serde(default)]
    path: String,
}

impl Query {
    pub fn validate(&self) -> Result<()> {
        files::path(&self.path, true)
    }
}

#[derive(Default)]
struct Usage {
    entries: Vec<(PathBuf, Metadata)>,
    files: usize,
    directories: usize,
    file_bytes: u128,
    file_blocks: u128,
    directory_blocks: u128,
}

impl Usage {
    fn walk(&mut self, dir: &File, prefix: &Path, depth: u32, entry_limit: usize) -> Result<()> {
        require(
            depth <= DEPTH_LIMIT,
            "QuotaExceeded",
            "volume usage directory depth limit",
        )?;
        for entry in Dir::read_from(dir).map_err(std::io::Error::from)? {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            require(
                self.entries.len() < entry_limit,
                "QuotaExceeded",
                "volume usage entry limit",
            )?;
            let relative = prefix.join(OsStr::from_bytes(name.to_bytes()));
            let entry = child(dir, name, OFlags::PATH)?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                self.directories += 1;
                self.directory_blocks += u128::from(meta.blocks());
                let directory = child(&entry, ".", OFlags::RDONLY | OFlags::DIRECTORY)?;
                self.entries.push((relative.clone(), meta));
                self.walk(&directory, &relative, depth + 1, entry_limit)?;
            } else {
                super::regular(&meta)?;
                self.files += 1;
                self.file_bytes += u128::from(meta.len());
                self.file_blocks += u128::from(meta.blocks());
                self.entries.push((relative, meta));
            }
        }
        Ok(())
    }

    fn confirm(&self, dir: &File) -> Result<()> {
        for (path, expected) in &self.entries {
            let (parent, leaf) = parent_from_components(dir, path.iter())?;
            let observed = child(&parent, leaf, OFlags::PATH)?.metadata()?;
            require(
                same_observation(expected, &observed),
                "Conflict",
                "volume changed during usage measurement",
            )?;
        }
        Ok(())
    }
}

fn same_observation(a: &Metadata, b: &Metadata) -> bool {
    same_source(a, b) && a.blocks() == b.blocks()
}

fn stamp(meta: &Metadata) -> Value {
    json!([
        meta.dev(),
        meta.ino(),
        meta.mode(),
        meta.uid(),
        meta.gid(),
        meta.nlink(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec()
    ])
}

pub(super) fn fingerprint(root: &File) -> Result<String> {
    let before = root.metadata()?;
    let mut scan = Usage::default();
    scan.walk(root, Path::new(""), 0, super::MAX_ENTRIES)?;
    scan.confirm(root)?;
    require(
        same_observation(&before, &root.metadata()?),
        "Conflict",
        "restored tree changed during metadata inspection",
    )?;
    // The v1 receipt sorts complete UTF-8 path strings, including the slash.
    // Path::cmp instead sorts components and would change existing receipts.
    scan.entries
        .sort_unstable_by(|(a, _), (b, _)| a.as_os_str().cmp(b.as_os_str()));
    let mut digest = Sha256::new();
    digest.update(b"psios-restore-metadata-v1\n");
    digest.update(serde_json::to_vec(&stamp(&before))?);
    for (path, meta) in scan.entries {
        let path = path.to_str().ok_or_else(|| {
            err(
                "UnsupportedCapability",
                "restore metadata requires UTF-8 entry names",
            )
        })?;
        digest.update(b"\n");
        digest.update(serde_json::to_vec(&json!([path, stamp(&meta)]))?);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn open_volume(w: &Work) -> Result<File> {
    let path = volume_dir(&w.cfg, &w.target)?;
    let root = File::from(
        open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(std::io::Error::from)?,
    );
    registered_root(&w.target, &root.metadata()?)?;
    Ok(root)
}

fn subtree(root: &File, path: &str) -> Result<File> {
    if path.is_empty() {
        return Ok(root.try_clone()?);
    }
    child(root, path, OFlags::RDONLY | OFlags::DIRECTORY)
}

pub(super) fn measure(w: &Work, query: &Query) -> Result<Value> {
    let dir = subtree(&open_volume(w)?, &query.path)?;
    let before = dir.metadata()?;
    let mut usage = Usage {
        directories: 1,
        directory_blocks: u128::from(before.blocks()),
        ..Usage::default()
    };
    // Include the measured root in the limit.
    usage.walk(&dir, Path::new(""), 0, ENTRY_LIMIT - 1)?;
    usage.confirm(&dir)?;
    let current = subtree(&open_volume(w)?, &query.path)?;
    require(
        same_observation(&before, &current.metadata()?),
        "Conflict",
        "volume root changed during usage measurement",
    )?;
    Ok(json!({
        "volume_ref": w.target.reference(), "data_incarnation": w.target.body["data_incarnation"],
        "path": query.path, "observed_at": now(), "files": usage.files, "directories": usage.directories,
        "logical_file_bytes": usage.file_bytes.to_string(),
        "allocated_file_bytes": (usage.file_blocks * BLOCK_BYTES).to_string(),
        "allocated_directory_bytes": (usage.directory_blocks * BLOCK_BYTES).to_string(),
        "allocated_total_bytes": ((usage.file_blocks + usage.directory_blocks) * BLOCK_BYTES).to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    #[test]
    fn restore_fingerprints_preserve_v1_path_order_and_name_encoding() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        for path in ["a/file", "a-early"] {
            fs::write(dir.path().join(path), b"data").unwrap();
        }
        let root = File::open(dir.path()).unwrap();
        let mut original = Sha256::new();
        original.update(b"psios-restore-metadata-v1\n");
        original.update(serde_json::to_vec(&stamp(&root.metadata().unwrap())).unwrap());
        for name in ["a", "a-early", "a/file"] {
            let meta = fs::metadata(dir.path().join(name)).unwrap();
            original.update(b"\n");
            original.update(serde_json::to_vec(&json!([name, stamp(&meta)])).unwrap());
        }
        assert_eq!(
            fingerprint(&root).unwrap(),
            format!("sha256:{:x}", original.finalize())
        );
        fs::write(dir.path().join(OsStr::from_bytes(b"raw-\xff")), b"data").unwrap();
        assert_eq!(
            fingerprint(&root).unwrap_err().0.code,
            "UnsupportedCapability"
        );
    }

    #[test]
    fn a_file_changed_after_enumeration_invalidates_the_observation() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join(OsStr::from_bytes(b"file-\xff"));
        fs::write(&path, b"first").unwrap();
        let root = File::open(dir.path()).unwrap();
        let mut recorded = Usage::default();
        recorded
            .walk(&root, Path::new(""), 0, ENTRY_LIMIT - 1)
            .unwrap();
        recorded.confirm(&root).unwrap();
        let mut file = File::options().append(true).open(path).unwrap();
        file.write_all(b"extra").unwrap();
        file.sync_all().unwrap();
        assert_eq!(recorded.confirm(&root).unwrap_err().0.code, "Conflict");
    }
}
