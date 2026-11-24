use super::{child, remove_contents};
use crate::{Config, Result, config::private_dir, require};
use rustix::fs::{AtFlags, Dir, OFlags, unlinkat};
use std::{
    fs::{File, Permissions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

const PREFIX: &str = ".stage-";
const SUFFIX_LEN: usize = 6;

pub(super) fn create(parent: &Path) -> Result<tempfile::TempDir> {
    private_dir(parent, false)?;
    Ok(tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .prefix(PREFIX)
        .rand_bytes(SUFFIX_LEN)
        .tempdir_in(parent)?)
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
            remove_contents(&dir, 0, true)?;
            unlinkat(&root, name, AtFlags::REMOVEDIR).map_err(std::io::Error::from)?;
            Ok(())
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
