//! Bounded inode accounting for local volumes held exclusively by the Core.
use super::{child, entry_parent, files, registered_root, same_source, volume_dir};
use crate::{
    Result, err,
    host::{Target, Work},
    now, require,
};
use rustix::fs::{Dir, Mode, OFlags, open};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{File, Metadata},
    os::unix::fs::MetadataExt,
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
    entries: Vec<(String, Metadata)>,
    files: usize,
    directories: usize,
    file_bytes: u128,
    file_blocks: u128,
    directory_blocks: u128,
}

impl Usage {
    fn walk(&mut self, dir: &File, prefix: &str, depth: u32) -> Result<()> {
        require(
            depth <= DEPTH_LIMIT,
            "QuotaExceeded",
            "volume usage directory depth limit",
        )?;
        for entry in Dir::read_from(dir).map_err(std::io::Error::from)? {
            let entry = entry.map_err(std::io::Error::from)?;
            let name = entry.file_name().to_str().map_err(|_| {
                err(
                    "UnsupportedCapability",
                    "volume usage requires UTF-8 entry names",
                )
            })?;
            if matches!(name, "." | "..") {
                continue;
            }
            // Include the measured root in the limit.
            require(
                self.entries.len() < ENTRY_LIMIT - 1,
                "QuotaExceeded",
                "volume usage entry limit",
            )?;
            let relative = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let entry = child(dir, name, OFlags::PATH)?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                self.directories += 1;
                self.directory_blocks += u128::from(meta.blocks());
                let directory = child(&entry, ".", OFlags::RDONLY | OFlags::DIRECTORY)?;
                self.entries.push((relative.clone(), meta));
                self.walk(&directory, &relative, depth + 1)?;
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
            let (parent, leaf) = entry_parent(dir, path)?;
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
    usage.walk(&dir, "", 0)?;
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
    fn a_file_changed_after_enumeration_invalidates_the_observation() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let path = nested.join("file");
        fs::write(&path, b"first").unwrap();
        let root = File::open(dir.path()).unwrap();
        let mut recorded = Usage::default();
        recorded.walk(&root, "", 0).unwrap();
        recorded.confirm(&root).unwrap();
        let mut file = File::options().append(true).open(path).unwrap();
        file.write_all(b"extra").unwrap();
        file.sync_all().unwrap();
        assert_eq!(recorded.confirm(&root).unwrap_err().0.code, "Conflict");
    }
}
