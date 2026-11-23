use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub witness_dir: PathBuf,
    pub socket: PathBuf,
    #[serde(default = "default_queue")]
    pub queue_limit: u32,
    #[serde(default = "default_tick")]
    pub tick_ms: u64,
    #[serde(default = "default_docker")]
    pub docker_socket: PathBuf,
}
fn default_queue() -> u32 {
    256
}
fn default_tick() -> u64 {
    60
}
fn default_docker() -> PathBuf {
    "/var/run/docker.sock".into()
}
impl Config {
    pub fn local(path: &Path) -> Self {
        let mut witness = path.as_os_str().to_owned();
        witness.push(".witness");
        Self {
            state_dir: path.into(),
            witness_dir: witness.into(),
            socket: path.join("api.sock"),
            queue_limit: default_queue(),
            tick_ms: default_tick(),
            docker_socket: default_docker(),
        }
    }
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = decode(&psios_api::parse(&fs::read(path)?)?)?;
        c.validate()?;
        Ok(c)
    }
    pub fn validate(&self) -> Result<()> {
        for p in [
            &self.state_dir,
            &self.witness_dir,
            &self.socket,
            &self.docker_socket,
        ] {
            require(
                p.is_absolute()
                    && p.components().all(|c| {
                        matches!(
                            c,
                            std::path::Component::Normal(_) | std::path::Component::RootDir
                        )
                    })
                    && p.file_name().is_some(),
                "InvalidArgument",
                "use absolute normalized configuration paths",
            )?;
        }
        require(
            !self.state_dir.starts_with(&self.witness_dir)
                && !self.witness_dir.starts_with(&self.state_dir),
            "InvalidArgument",
            "witness must be outside state directory",
        )?;
        require(
            (1..=4096).contains(&self.queue_limit) && (5..=5000).contains(&self.tick_ms),
            "InvalidArgument",
            "invalid scheduler limits",
        )
    }
}
pub(crate) fn private_dir(p: &Path, create: bool) -> Result<()> {
    if create {
        match fs::DirBuilder::new().mode(0o700).create(p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let m = fs::symlink_metadata(p)?;
    require(
        m.is_dir()
            && m.uid() == rustix::process::geteuid().as_raw()
            && m.mode() & 0o077 == 0
            && fs::canonicalize(p)? == p,
        "PermissionDenied",
        "state path must be a private, owned, real directory",
    )
}
pub(crate) fn new_file(p: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(p)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    File::open(p.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub(crate) fn replace(p: &Path, bytes: &[u8]) -> Result<()> {
    let dir = p.parent().unwrap();
    let mut f = tempfile::NamedTempFile::new_in(dir)?;
    f.write_all(bytes)?;
    f.as_file().sync_all()?;
    f.persist(p).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}
