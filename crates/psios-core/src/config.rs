use crate::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const DEFAULT_BACKUP_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_BACKUP_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub(crate) const MAX_QUEUE_LIMIT: u32 = 4096;
const DEFAULT_BACKUP_CAPTURE_TIMEOUT_SECONDS: u32 = 30;

fn default_backup_max_bytes() -> u64 {
    DEFAULT_BACKUP_MAX_BYTES
}

fn default_backup_capture_timeout_seconds() -> u32 {
    DEFAULT_BACKUP_CAPTURE_TIMEOUT_SECONDS
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub state_dir: PathBuf,
    pub witness_dir: PathBuf,
    pub socket: PathBuf,
    #[serde(default = "default_queue")]
    pub queue_limit: u32,
    #[serde(default)]
    pub principal_queue_limit: Option<u32>,
    #[serde(default)]
    pub read_result_bytes_per_principal: Option<i64>,
    #[serde(default = "default_recovery_queue")]
    pub recovery_queue_limit: u32,
    #[serde(default = "default_tick")]
    pub tick_ms: u64,
    #[serde(default = "default_workers")]
    pub workers: u32,
    #[serde(default)]
    pub services: Vec<ServiceConfig>,
    #[serde(default)]
    pub network_probes: Vec<NetworkProbeConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_link: Option<NetworkLinkConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_device_reconcile_ms: Option<u64>,
    #[serde(default = "default_docker")]
    pub docker_socket: PathBuf,
    #[serde(default)]
    pub oci_effective_volume_observation: bool,
    #[serde(default = "default_oci_capture_bytes")]
    pub oci_capture_max_bytes: u32,
    #[serde(default)]
    pub authority_reserve: AuthorityReserve,
    #[serde(default = "default_backup_max_bytes")]
    pub backup_max_bytes: u64,
    #[serde(default = "default_backup_capture_timeout_seconds")]
    pub backup_capture_timeout_seconds: u32,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthorityReserve {
    pub state_bytes: u64,
    pub witness_bytes: u64,
    pub state_inodes: u64,
    pub witness_inodes: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Manager {
    User,
    System,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub unit: String,
    pub manager: Manager,
    #[serde(default)]
    pub allow_control: bool,
    pub definition_digest: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProbeConfig {
    pub name: String,
    pub endpoint: SocketAddr,
    #[serde(default = "default_probe_timeout")]
    pub timeout_ms: u64,
    pub dns: Option<DnsProbeConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkLinkConfig {
    pub interface: String,
    pub mac: String,
    pub original_uuid: String,
    pub candidate_uuid: String,
    pub confirmation_seconds: u32,
}

impl NetworkLinkConfig {
    fn validate(&self) -> Result<()> {
        let iface = self.interface.as_bytes();
        require(
            (1..=15).contains(&iface.len())
                && iface[0].is_ascii_alphanumeric()
                && iface
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
                && self.interface != "lo",
            "InvalidArgument",
            "network link requires a valid non-loopback interface name",
        )?;
        let mac: Vec<&str> = self.mac.split(':').collect();
        require(
            mac.len() == 6
                && mac.iter().all(|octet| {
                    octet.len() == 2
                        && octet
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
                && u8::from_str_radix(mac[0], 16).is_ok_and(|first| first & 1 == 0)
                && self.mac != "00:00:00:00:00:00",
            "InvalidArgument",
            "network link requires a canonical nonzero unicast MAC",
        )?;
        let profile_id = |value: &str| {
            uuid::Uuid::parse_str(value)
                .is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == value)
        };
        require(
            profile_id(&self.original_uuid)
                && profile_id(&self.candidate_uuid)
                && self.original_uuid != self.candidate_uuid,
            "InvalidArgument",
            "network link requires two distinct canonical profile UUIDs",
        )?;
        require(
            (20..=120).contains(&self.confirmation_seconds),
            "InvalidArgument",
            "network confirmation window must be between 20 and 120 seconds",
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DnsProbeConfig {
    pub resolver: SocketAddr,
    pub name: String,
}

fn default_probe_timeout() -> u64 {
    2_000
}
fn default_workers() -> u32 {
    4
}
fn default_queue() -> u32 {
    256
}
fn default_recovery_queue() -> u32 {
    2
}
fn default_tick() -> u64 {
    60
}
fn default_docker() -> PathBuf {
    "/var/run/docker.sock".into()
}
fn default_oci_capture_bytes() -> u32 {
    storage::oci_archive::DEFAULT_CAPTURE_BYTES
}
impl Default for Config {
    fn default() -> Self {
        Self::local(Path::new("/var/lib/psios"))
    }
}
impl Config {
    /// Explicit paths win. Only an absent system configuration selects the historical local layout.
    pub fn resolve(config: Option<&Path>, state: Option<&Path>) -> Result<Self> {
        match (config, state) {
            (Some(_), Some(_)) => Err(err("InvalidArgument", "choose one of --config and --state")),
            (Some(file), None) => Self::load(file),
            (None, Some(path)) => Ok(Self::local(&std::path::absolute(path)?)),
            (None, None) => {
                let file = Path::new("/etc/psios/core.json");
                match fs::symlink_metadata(file) {
                    Ok(_) => Self::load(file),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    pub fn local(path: &Path) -> Self {
        let mut witness = path.as_os_str().to_owned();
        witness.push(".witness");
        Self {
            state_dir: path.into(),
            witness_dir: witness.into(),
            socket: path.join("api.sock"),
            queue_limit: default_queue(),
            principal_queue_limit: None,
            read_result_bytes_per_principal: None,
            recovery_queue_limit: default_recovery_queue(),
            workers: default_workers(),
            services: Vec::new(),
            network_probes: Vec::new(),
            network_link: None,
            block_device_reconcile_ms: None,
            tick_ms: default_tick(),
            docker_socket: default_docker(),
            oci_effective_volume_observation: false,
            oci_capture_max_bytes: default_oci_capture_bytes(),
            authority_reserve: AuthorityReserve::default(),
            backup_max_bytes: DEFAULT_BACKUP_MAX_BYTES,
            backup_capture_timeout_seconds: DEFAULT_BACKUP_CAPTURE_TIMEOUT_SECONDS,
        }
    }
    pub fn load(path: &Path) -> Result<Self> {
        let mut source = File::open(path)?;
        let mut bytes = Vec::new();
        source.read_to_end(&mut bytes)?;
        let c: Self = decode(&psios_api::parse(&bytes)?)?;
        if c.network_link.is_some() {
            let opened = source.metadata()?;
            let current = fs::symlink_metadata(path)?;
            require(
                current.is_file()
                    && opened.dev() == current.dev()
                    && opened.ino() == current.ino()
                    && opened.uid() == 0
                    && opened.mode() & 0o022 == 0,
                "PermissionDenied",
                "network link enrollment requires a root-owned configuration file protected from writes",
            )?;
        }
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
        let mut units = std::collections::HashSet::new();
        require(
            self.services.len() <= 64
                && self.services.iter().all(|s| {
                    s.unit.len() <= 120
                        && s.unit.ends_with(".service")
                        && s.unit
                            .as_bytes()
                            .first()
                            .is_some_and(u8::is_ascii_alphanumeric)
                        && s.unit
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.@-".contains(&b))
                        && (!s.allow_control || s.definition_digest.is_some())
                        && s.definition_digest.as_deref().is_none_or(|d| {
                            d.strip_prefix("sha256:").is_some_and(|h| {
                                h.len() == 64
                                    && h.bytes()
                                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                            })
                        })
                        && units.insert(&s.unit)
                }),
            "InvalidArgument",
            "invalid service allowlist or missing control definition digest",
        )?;
        node::network::probe::validate(&self.network_probes)?;
        if let Some(link) = &self.network_link {
            link.validate()?;
        }
        require(
            self.block_device_reconcile_ms
                .is_none_or(|interval| (250..=60_000).contains(&interval)),
            "InvalidArgument",
            "block device reconciliation interval must be between 250 and 60000 milliseconds",
        )?;
        require(
            (1..=storage::oci_archive::MAX_BYTES).contains(&self.oci_capture_max_bytes),
            "InvalidArgument",
            "OCI capture max must be between 1 and 33554432 bytes",
        )?;
        require(
            (1..=MAX_BACKUP_BYTES).contains(&self.backup_max_bytes),
            "InvalidArgument",
            "backup maximum must be between 1 byte and 1 TiB",
        )?;
        require(
            (1..=86_400).contains(&self.backup_capture_timeout_seconds),
            "InvalidArgument",
            "backup capture timeout must be between 1 and 86400 seconds",
        )?;
        require(
            self.read_result_bytes_per_principal.is_none_or(|limit| {
                (artifact::capacity::RESERVATION..=1024 * 1024 * 1024 * 1024).contains(&limit)
            }),
            "InvalidArgument",
            "read result quota must be between 4 MiB and 1 TiB",
        )?;
        require(
            (1..=16).contains(&self.workers)
                && (1..=MAX_QUEUE_LIMIT).contains(&self.queue_limit)
                && self
                    .principal_queue_limit
                    .is_none_or(|limit| (1..=self.queue_limit).contains(&limit))
                && self.recovery_queue_limit <= 16
                && (5..=5000).contains(&self.tick_ms),
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
