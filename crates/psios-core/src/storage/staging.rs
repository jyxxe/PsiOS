use super::{child, filename, remove_contents, unknown};
use crate::{Config, Result, config::private_dir, err, require};
use rustix::fs::{AtFlags, Dir, OFlags, RenameFlags, renameat_with, statat, unlinkat};
use std::{
    fs::{File, Permissions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const PREFIX: &str = ".stage-";
const SUFFIX_LEN: usize = 6;

pub(super) struct Stage {
    root: File,
    dir: File,
    path: PathBuf,
    published: bool,
    retain_on_failure: bool,
}

pub(super) fn create(parent: &Path) -> Result<Stage> {
    private_dir(parent, false)?;
    let root = File::open(parent)?;
    let path = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .prefix(PREFIX)
        .rand_bytes(SUFFIX_LEN)
        .tempdir_in(parent)?
        .keep();
    let dir = child(
        &root,
        path.file_name().unwrap(),
        OFlags::RDONLY | OFlags::DIRECTORY,
    )?;
    Ok(Stage {
        root,
        dir,
        path,
        published: false,
        retain_on_failure: false,
    })
}

pub(super) fn resume(parent: &Path, name: &str) -> Result<Stage> {
    private_dir(parent, false)?;
    filename(name)?;
    let root = File::open(parent)?;
    let path = parent.join(name);
    private_dir(&path, true)?;
    let dir = child(&root, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
    dir.try_lock().map_err(|_| {
        err(
            "Conflict",
            "backup export is already using this staging directory",
        )
    })?;
    check_entry(&root, name, &dir)?;
    root.sync_all()?;
    Ok(Stage {
        root,
        dir,
        path,
        published: false,
        retain_on_failure: true,
    })
}

impl Stage {
    pub(super) fn directory(&self) -> &File {
        &self.dir
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn identity(&self) -> Result<(u64, u64)> {
        let metadata = self.dir.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }

    pub(super) fn publish(mut self, name: &str) -> Result<()> {
        filename(name)?;
        let staged = self.path.file_name().unwrap();
        check_entry(&self.root, staged, &self.dir)?;
        self.dir.sync_all()?;
        renameat_with(&self.root, staged, &self.root, name, RenameFlags::NOREPLACE).map_err(
            |e| {
                if e == rustix::io::Errno::EXIST {
                    err("Conflict", "storage operation identity already present")
                } else {
                    std::io::Error::from(e).into()
                }
            },
        )?;
        self.published = true;
        self.root.sync_all().map_err(unknown)
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.published && !self.retain_on_failure {
            let name = self.path.file_name().unwrap();
            let removed = discard(&self.root, name, &self.dir)
                .and_then(|()| self.root.sync_all().map_err(Into::into));
            if let Err(e) = removed {
                eprintln!("psiosd storage staging {name:?}: {}", e.0.message);
            }
        }
    }
}

fn check_entry(root: &File, name: impl rustix::path::Arg, dir: &File) -> Result<()> {
    let entry = statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
    let pinned = dir.metadata()?;
    require(
        entry.st_dev == pinned.dev() && entry.st_ino == pinned.ino(),
        "PreconditionFailed",
        "storage stage identity changed",
    )
}

fn discard(root: &File, name: impl rustix::path::Arg + Copy, dir: &File) -> Result<()> {
    check_entry(root, name, dir)?;
    remove_contents(dir, 0)?;
    unlinkat(root, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
    Ok(())
}

fn matches_prefix(name: &[u8], prefix: &[u8]) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == SUFFIX_LEN && suffix.iter().all(u8::is_ascii_alphanumeric)
    })
}

fn reclaim(parent: &Path, legacy_backup: bool) -> Result<()> {
    private_dir(parent, false)?;
    let root = File::open(parent)?;
    let mut removed = 0;
    for entry in Dir::read_from(&root).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if !matches_prefix(bytes, PREFIX.as_bytes())
            && !(legacy_backup && matches_prefix(bytes, b".tmp"))
        {
            continue;
        }
        let result = (|| -> Result<()> {
            let dir = child(&root, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
            require(
                dir.metadata()?.mode() & 0o077 == 0,
                "PermissionDenied",
                "staged directory is accessible to other accounts",
            )?;
            discard(&root, name, &dir)
        })();
        match result {
            Ok(()) => removed += 1,
            Err(e) => eprintln!("psiosd storage staging {name:?}: {}", e.0.message),
        }
    }
    if removed != 0 {
        root.sync_all()?;
        eprintln!(
            "psiosd reclaimed {removed} interrupted stages in {}",
            parent.display()
        );
    }
    Ok(())
}

// Serve holds the exclusive authority lock. Workers only publish by renaming out of staging.
// Completed paths can belong to operations whose receipts were lost and must stay intact.
pub(crate) fn reclaim_on_startup(cfg: &Config) {
    for (dir, legacy_backup) in [("volumes", false), ("backups", true)] {
        if let Err(e) = reclaim(&cfg.state_dir.join(dir), legacy_backup) {
            eprintln!("psiosd could not reclaim {dir} staging: {}", e.0.message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn publishing_does_not_replace_an_existing_empty_directory() {
        let scratch_dir = tempfile::tempdir().unwrap();
        let parent = scratch_dir.path().canonicalize().unwrap().join("pool");
        private_dir(&parent, true).unwrap();
        let existing = parent.join("volume-existing");
        fs::create_dir(&existing).unwrap();
        let inode = existing.metadata().unwrap().ino();
        let stage = create(&parent).unwrap();
        let scratch = stage.path().to_path_buf();
        fs::write(scratch.join("record"), b"proposed data").unwrap();

        let error = stage.publish("volume-existing").unwrap_err();
        assert_eq!(error.0.code, "Conflict");
        assert_eq!(existing.metadata().unwrap().ino(), inode);
        assert!(fs::read_dir(existing).unwrap().next().is_none());
        assert!(!scratch.exists());
    }
}
