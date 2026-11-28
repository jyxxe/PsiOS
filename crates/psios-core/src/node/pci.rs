//! Bounded, read-only observation of PCI enumeration and driver binding.
use super::sysfs::{BELOW, DIRECTORY, attribute, inventory_page, is_sysfs};
use crate::{Result, err, host::Work, linux_clock, now, require};
use psios_api::{DeviceInventoryQuery, MAX_PCI_INVENTORY_ROWS, sha256};
use rustix::{
    fd::OwnedFd,
    fs::{Dir, FileType, Mode, OFlags, fstat, open, openat2, readlinkat},
    io::Errno,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
};

pub(crate) const TOOL: &str = "node.pci";
pub(crate) const FORMAT: u32 = super::network::resolver::FORMAT + 1;
const MAX_DEVICES: usize = MAX_PCI_INVENTORY_ROWS as usize;

fn hex(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn pci_address(value: &str) -> bool {
    let Some((domain, rest)) = value.split_once(':') else {
        return false;
    };
    let Some((bus, rest)) = rest.split_once(':') else {
        return false;
    };
    let Some((slot, function)) = rest.split_once('.') else {
        return false;
    };
    (4..=8).contains(&domain.len())
        && hex(domain)
        && bus.len() == 2
        && hex(bus)
        && slot.len() == 2
        && hex(slot)
        && u8::from_str_radix(slot, 16).is_ok_and(|number| number <= 31)
        && matches!(function.as_bytes(), [b'0'..=b'7'])
}

fn absence_during_scan(error: io::Error) -> crate::Error {
    if error.kind() == io::ErrorKind::NotFound {
        err("Conflict", "a PCI device changed during observation")
    } else {
        error.into()
    }
}

fn identifier(dir: &OwnedFd, name: &str, digits: usize) -> Result<String> {
    let value = attribute(dir, name).map_err(absence_during_scan)?;
    require(
        value
            .strip_prefix("0x")
            .is_some_and(|value| value.len() == digits && hex(value)),
        "HostFailure",
        "invalid PCI sysfs identifier",
    )?;
    Ok(value)
}

fn bound_driver(dir: &OwnedFd) -> Result<Option<String>> {
    let link = match readlinkat(dir, "driver", Vec::new()) {
        Ok(link) => link,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(io::Error::from(error).into()),
    };
    let path = Path::new(OsStr::from_bytes(link.as_bytes()));
    let name = path.file_name().and_then(OsStr::to_str).filter(|name| {
        (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
            && path
                .parent()
                .is_some_and(|base| base.ends_with("bus/pci/drivers"))
    });
    Ok(Some(
        name.ok_or_else(|| err("HostFailure", "invalid PCI driver link"))?
            .to_owned(),
    ))
}

#[derive(PartialEq, Eq, Serialize)]
struct Device {
    address: String,
    parent_address: Option<String>,
    vendor_id: String,
    device_id: String,
    subsystem_vendor_id: String,
    subsystem_device_id: String,
    class_code: String,
    revision: String,
    driver: Option<String>,
    #[serde(skip)]
    sysfs_identity: (u64, u64),
}

struct Tree {
    root: OwnedFd,
    entries: OwnedFd,
}

impl Tree {
    fn open() -> Result<Self> {
        let unavailable = |_| err("DependencyUnavailable", "kernel PCI sysfs is unavailable");
        let root =
            open("/sys", DIRECTORY | OFlags::NOFOLLOW, Mode::empty()).map_err(unavailable)?;
        require(
            is_sysfs(&root)?,
            "UnsupportedCapability",
            "PCI observation requires Linux sysfs",
        )?;
        let entries = openat2(&root, "bus/pci/devices", DIRECTORY, Mode::empty(), BELOW)
            .map_err(unavailable)?;
        Ok(Self { root, entries })
    }

    fn device(&self, address: &str) -> Result<Device> {
        let link = readlinkat(&self.entries, address, Vec::new())
            .map_err(io::Error::from)
            .map_err(absence_during_scan)?;
        let link = Path::new(OsStr::from_bytes(link.as_bytes()));
        let path = link
            .strip_prefix("../../..")
            .ok()
            .filter(|path| {
                path.starts_with("devices")
                    && path
                        .components()
                        .all(|part| matches!(part, Component::Normal(_)))
                    && path.file_name() == Some(OsStr::new(address))
            })
            .ok_or_else(|| err("HostFailure", "PCI link is outside the kernel device tree"))?;
        let dir = openat2(&self.root, path, DIRECTORY, Mode::empty(), BELOW)
            .map_err(io::Error::from)
            .map_err(absence_during_scan)?;
        let identity = fstat(&dir).map_err(io::Error::from)?;
        let parent_address = path
            .parent()
            .and_then(Path::file_name)
            .and_then(OsStr::to_str)
            .filter(|name| pci_address(name))
            .map(str::to_owned);
        Ok(Device {
            address: address.to_owned(),
            parent_address,
            vendor_id: identifier(&dir, "vendor", 4)?,
            device_id: identifier(&dir, "device", 4)?,
            subsystem_vendor_id: identifier(&dir, "subsystem_vendor", 4)?,
            subsystem_device_id: identifier(&dir, "subsystem_device", 4)?,
            class_code: identifier(&dir, "class", 6)?,
            revision: identifier(&dir, "revision", 2)?,
            driver: bound_driver(&dir)?,
            sysfs_identity: (identity.st_dev, identity.st_ino),
        })
    }

    fn scan(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        for entry in Dir::read_from(&self.entries).map_err(io::Error::from)? {
            let entry = entry.map_err(io::Error::from)?;
            if matches!(entry.file_name().to_bytes(), b"." | b"..") {
                continue;
            }
            require(
                devices.len() < MAX_DEVICES,
                "QuotaExceeded",
                "PCI observation exceeds 256 devices",
            )?;
            let address = entry
                .file_name()
                .to_str()
                .ok()
                .filter(|name| pci_address(name));
            let address = address
                .filter(|_| entry.file_type() == FileType::Symlink)
                .ok_or_else(|| err("HostFailure", "invalid entry in the kernel PCI directory"))?;
            devices.push(self.device(address)?);
        }
        devices.sort_unstable_by(|a, b| a.address.cmp(&b.address));
        require(
            devices
                .windows(2)
                .all(|rows| rows[0].address != rows[1].address),
            "Conflict",
            "the kernel PCI directory changed during observation",
        )?;
        Ok(devices)
    }
}

pub(crate) fn validate_inventory(query: &DeviceInventoryQuery) -> Result<()> {
    super::sysfs::validate_inventory(query, "PCI inventory", MAX_DEVICES)
}

pub(super) fn observe(work: &Work, query: &DeviceInventoryQuery) -> Result<Value> {
    let boot_id = linux_clock::boot_id()?;
    let mount_namespace = fs::metadata("/proc/self/ns/mnt")?.ino();
    let tree = Tree::open()?;
    let before = fstat(&tree.entries).map_err(io::Error::from)?;
    let devices = tree.scan()?;
    let current = Tree::open()?;
    let after = fstat(&current.entries).map_err(io::Error::from)?;
    require(
        (before.st_dev, before.st_ino) == (after.st_dev, after.st_ino)
            && devices == current.scan()?
            && mount_namespace == fs::metadata("/proc/self/ns/mnt")?.ino(),
        "Conflict",
        "the PCI inventory or driver binding changed during observation",
    )?;
    let namespace = mount_namespace.to_string();
    if query.limit.is_none() && query.cursor.is_none() {
        return Ok(json!({"boot_id":boot_id,"mount_namespace_inode":namespace,
                        "observed_at":now(),"devices":devices}));
    }
    let identities: Vec<_> = devices.iter().map(|device| device.sysfs_identity).collect();
    let digest = sha256(&serde_json::to_vec(&(
        "psios/node.pci/1",
        work.target.reference(),
        &work.task.owner,
        &work.task.spec["delegation_ref"],
        &boot_id,
        mount_namespace,
        (before.st_dev, before.st_ino),
        identities,
        &devices,
    ))?);
    let (devices, next) = inventory_page(&devices, query, &digest, "PCI inventory")?;
    Ok(json!({"boot_id":boot_id,"mount_namespace_inode":namespace,
              "observed_at":now(),"devices":devices,"snapshot_digest":digest,"next_cursor":next}))
}
