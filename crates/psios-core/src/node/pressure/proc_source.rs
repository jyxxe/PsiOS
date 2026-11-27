//! Bind pressure counters and their namespace labels to the same procfs view.
use crate::linux_proc::{self, unsupported_mount};
use rustix::{fd::OwnedFd, fs::readlinkat};
use std::{fs::File, io};

pub(super) struct Source {
    root: OwnedFd,
    process: OwnedFd,
    namespaces: OwnedFd,
}

#[derive(PartialEq, Eq)]
pub(super) struct Identity {
    pub(super) cgroup_namespace: String,
    pub(super) mount_namespace: String,
}

impl Source {
    pub(super) fn open() -> io::Result<Self> {
        let root = linux_proc::root()?;
        // The procfs PID can differ from getpid() in an inherited PID view.
        // Read the kernel's own link, then open only its numeric directory.
        let visible = readlinkat(&root, "self", Vec::new())?;
        let pid = visible.to_str().map_err(|_| io::ErrorKind::InvalidData)?;
        if !canonical_number(pid) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let process = linux_proc::directory(&root, pid).map_err(unsupported_mount)?;
        let namespaces = linux_proc::directory(&process, "ns").map_err(unsupported_mount)?;
        Ok(Self {
            root,
            process,
            namespaces,
        })
    }

    pub(super) fn file(&self, path: &str) -> io::Result<File> {
        linux_proc::file(&self.root, path).map_err(unsupported_mount)
    }

    pub(super) fn membership(&self) -> io::Result<File> {
        linux_proc::file(&self.process, "cgroup").map_err(unsupported_mount)
    }

    pub(super) fn identity(&self) -> io::Result<Identity> {
        Ok(Identity {
            cgroup_namespace: self.namespace("cgroup")?,
            mount_namespace: self.namespace("mnt")?,
        })
    }

    fn namespace(&self, kind: &str) -> io::Result<String> {
        let target = readlinkat(&self.namespaces, kind, Vec::new())?;
        let target = target.to_str().map_err(|_| io::ErrorKind::InvalidData)?;
        let (actual, inode) = target.split_once(":[").ok_or(io::ErrorKind::InvalidData)?;
        let inode = inode.strip_suffix(']').ok_or(io::ErrorKind::InvalidData)?;
        if actual != kind || !canonical_number(inode) {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(inode.into())
    }
}

fn canonical_number(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|number| number > 0 && number.to_string() == value)
}
