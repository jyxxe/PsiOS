//! A private empty directory is published by moving its exact inode into the data tree.
use super::super::{
    child,
    entry_retirement::fs::{Depot, Entry, Identity, contains_entries, identity, sample},
    filename, files,
};
use crate::{Config, Result, db::Doc, err, require};
use rustix::{
    fs::{
        Access, AtFlags, Mode, OFlags, RenameFlags, accessat, fchmod, fgetxattr_query, mkdirat,
        renameat_with, unlinkat,
    },
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{fs::File, io, os::unix::fs::MetadataExt};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Parents {
    wrapper: Identity,
    root: Identity,
    parent: Identity,
    depot: Identity,
    group: u32,
    setgid: bool,
}

impl Parents {
    pub(super) fn check(&self, volume: &Doc) -> Result<()> {
        require(
            [self.wrapper, self.root, self.parent, self.depot]
                .iter()
                .all(|id| id[0] == self.root[0] && id[1] != 0)
                && ["device", "inode"]
                    .map(|name| volume.spec[name].as_str().and_then(|id| id.parse().ok()))
                    == self.root.map(Some),
            "RecoveryRequired",
            "directory creation parents disagree with the registered Volume root",
        )
    }

    fn verify_depot(&self, depot: &Depot) -> Result<()> {
        require(
            identity(&depot.wrapper)? == self.wrapper
                && identity(&depot.root)? == self.root
                && identity(&depot.directory)? == self.depot,
            "PreconditionFailed",
            "directory creation private storage identity changed",
        )
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Location {
    pub(super) parents: Parents,
    pub(super) entry: Entry,
}

impl Location {
    pub(super) fn check(&self, volume: &Doc) -> Result<()> {
        self.parents.check(volume)?;
        require(
            self.entry.is_dir()
                && self.entry.identity[0] == self.parents.root[0]
                && self.entry.identity[1] != 0
                && self.entry.owner[1] == self.parents.group
                && self.entry.mode & 0o5077 == 0
                && (self.entry.mode & 0o2000 != 0) == self.parents.setgid,
            "RecoveryRequired",
            "directory creation stage is not a same-filesystem directory",
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Placement {
    Staged,
    Published,
}

fn stage_name(operation: &str) -> Result<String> {
    filename(operation)?;
    Ok(format!("mkdir-{operation}"))
}

fn plain_inheritance(directory: &File) -> Result<()> {
    for attribute in [
        "system.posix_acl_default",
        "security.selinux",
        "security.SMACK64",
        "security.SMACK64TRANSMUTE",
    ] {
        require(
            matches!(
                fgetxattr_query(directory, attribute),
                Err(Errno::NODATA | Errno::OPNOTSUPP)
            ),
            "UnsupportedCapability",
            "directory creation cannot preserve inherited ACL or filesystem label metadata",
        )?;
    }
    Ok(())
}

fn verify_directory(directory: &File, entry: &Entry) -> Result<()> {
    require(
        Entry::new(&directory.metadata()?) == *entry,
        "PreconditionFailed",
        "new directory identity or metadata changed",
    )?;
    plain_inheritance(directory)?;
    require(
        matches!(
            fgetxattr_query(directory, "system.posix_acl_access"),
            Err(Errno::NODATA | Errno::OPNOTSUPP)
        ),
        "UnsupportedCapability",
        "new directory has access ACL metadata outside the approved profile",
    )?;
    require(
        !contains_entries(directory)? && Entry::new(&directory.metadata()?) == *entry,
        "PreconditionFailed",
        "new directory is no longer stably empty",
    )
}

pub(super) struct Paths {
    depot: Depot,
    parent: File,
    leaf: String,
    stage: String,
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
        let (parent, leaf) = files::parent(&depot.root, path)?;
        Ok(Self {
            depot,
            parent,
            leaf,
            stage: stage_name(operation)?,
        })
    }

    fn group_profile(&self) -> Result<(u32, bool)> {
        let parent = self.parent.metadata()?;
        require(
            parent.gid() == self.depot.directory.metadata()?.gid(),
            "UnsupportedCapability",
            "directory creation cannot preserve a different inherited group",
        )?;
        plain_inheritance(&self.parent)?;
        plain_inheritance(&self.depot.directory)?;
        Ok((parent.gid(), parent.mode() & 0o2000 != 0))
    }

    pub(super) fn reserve(&self, cfg: &Config) -> Result<Parents> {
        let (group, setgid) = self.group_profile()?;
        accessat(
            &self.parent,
            ".",
            Access::WRITE_OK | Access::EXEC_OK,
            AtFlags::EACCESS,
        )
        .map_err(|error| match error {
            Errno::ACCESS | Errno::PERM => err(
                "PermissionDenied",
                "directory creation parent is not writable and searchable",
            ),
            _ => io::Error::from(error).into(),
        })?;
        require(
            sample(&self.parent, &self.leaf)?.is_none()
                && sample(&self.depot.directory, &self.stage)?.is_none(),
            "Conflict",
            "directory creation destination or reserved stage already exists",
        )?;
        cfg.check_managed_space(&self.depot.directory, std::iter::empty(), 1)?;
        Ok(Parents {
            wrapper: identity(&self.depot.wrapper)?,
            root: identity(&self.depot.root)?,
            parent: identity(&self.parent)?,
            depot: identity(&self.depot.directory)?,
            group,
            setgid,
        })
    }

    fn check_parent(&self, parents: &Parents) -> Result<()> {
        parents.verify_depot(&self.depot)?;
        require(
            identity(&self.parent)? == parents.parent,
            "PreconditionFailed",
            "approved directory creation parent changed",
        )
    }

    pub(super) fn allocate(&self, parents: Parents) -> Result<Location> {
        self.check_parent(&parents)?;
        require(
            self.group_profile()? == (parents.group, parents.setgid),
            "PreconditionFailed",
            "directory creation inheritance changed before staging",
        )?;
        mkdirat(&self.depot.directory, self.stage.as_str(), Mode::RWXU).map_err(
            |error| match error {
                Errno::EXIST => err(
                    "Conflict",
                    "reserved directory creation stage already exists",
                ),
                _ => io::Error::from(error).into(),
            },
        )?;
        let staged = child(
            &self.depot.directory,
            &self.stage,
            OFlags::RDONLY | OFlags::DIRECTORY,
        )?;
        let info = staged.metadata()?;
        require(
            info.uid() == rustix::process::geteuid().as_raw()
                && info.gid() == parents.group
                && info.mode() & 0o7077 == 0,
            "UnsupportedCapability",
            "staged directory cannot preserve the ordinary parent group and private mode",
        )?;
        if parents.setgid {
            fchmod(
                &staged,
                Mode::from_raw_mode(info.mode() & 0o700) | Mode::SGID,
            )
            .map_err(io::Error::from)?;
        }
        let updated = staged.metadata()?;
        require(
            (updated.mode() & 0o2000 != 0) == parents.setgid,
            "UnsupportedCapability",
            "filesystem declined the parent setgid inheritance",
        )?;
        let entry = Entry::new(&updated);
        verify_directory(&staged, &entry)?;
        staged.sync_all()?;
        self.depot.directory.sync_all()?;
        Ok(Location { parents, entry })
    }

    pub(super) fn observe(&self, location: &Location) -> Result<Placement> {
        self.check_parent(&location.parents)?;
        let staged = sample(&self.depot.directory, &self.stage)?;
        let final_entry = sample(&self.parent, &self.leaf)?;
        match (staged, final_entry) {
            (Some(entry), None) if entry == location.entry => {
                verify_directory(
                    &child(
                        &self.depot.directory,
                        &self.stage,
                        OFlags::RDONLY | OFlags::DIRECTORY,
                    )?,
                    &entry,
                )?;
                Ok(Placement::Staged)
            }
            (None, Some(entry)) if location.entry.same_after_move(&entry) => {
                verify_directory(
                    &child(&self.parent, &self.leaf, OFlags::RDONLY | OFlags::DIRECTORY)?,
                    &entry,
                )?;
                Ok(Placement::Published)
            }
            _ => Err(err(
                "PreconditionFailed",
                "directory creation placement does not identify the original empty directory",
            )),
        }
    }

    pub(super) fn publish(&self, location: &Location) -> Result<()> {
        require(
            self.group_profile()? == (location.parents.group, location.parents.setgid)
                && self.observe(location)? == Placement::Staged,
            "PreconditionFailed",
            "directory creation changed after approval",
        )?;
        renameat_with(
            &self.depot.directory,
            self.stage.as_str(),
            &self.parent,
            self.leaf.as_str(),
            RenameFlags::NOREPLACE,
        )
        .map_err(super::super::unknown)
    }

    pub(super) fn sync(&self, location: &Location, placement: Placement) -> Result<()> {
        let (parent, name) = match placement {
            Placement::Staged => (&self.depot.directory, self.stage.as_str()),
            Placement::Published => (&self.parent, self.leaf.as_str()),
        };
        let directory = child(parent, name, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let sampled = Entry::new(&directory.metadata()?);
        require(
            match placement {
                Placement::Staged => sampled == location.entry,
                Placement::Published => location.entry.same_after_move(&sampled),
            },
            "PreconditionFailed",
            "directory changed before its durable synchronization",
        )?;
        verify_directory(&directory, &sampled)?;
        directory.sync_all()?;
        self.parent.sync_all()?;
        self.depot.directory.sync_all()?;
        Ok(())
    }
}

// A committed NotApplied outcome is sufficient to retire its private staging
// name. The record may predate allocation of the inode, but no user content
// belongs in this namespace and rmdir cannot remove even one contained entry.
pub(super) fn dispose(
    cfg: &Config,
    volume: &Doc,
    operation: &str,
    parents: &Parents,
    expected: Option<&Entry>,
) -> Result<bool> {
    let depot = Depot::open(cfg, volume, false)?;
    parents.verify_depot(&depot)?;
    let name = stage_name(operation)?;
    let removed = if let Some(entry) = sample(&depot.directory, &name)? {
        let opened = child(&depot.directory, &name, OFlags::RDONLY | OFlags::DIRECTORY)?;
        let info = opened.metadata()?;
        require(
            Entry::new(&info) == entry
                && expected.is_none_or(|expected| *expected == entry)
                && info.uid() == rustix::process::geteuid().as_raw()
                && info.gid() == parents.group
                && info.mode() & 0o5077 == 0,
            "PreconditionFailed",
            "directory creation stage changed before disposal",
        )?;
        verify_directory(&opened, &entry)?;
        unlinkat(&depot.directory, name.as_str(), AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        true
    } else {
        false
    };
    depot.directory.sync_all()?;
    Ok(removed)
}
