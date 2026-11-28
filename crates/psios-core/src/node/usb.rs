//! Bounded observation of physical USB functions and root hubs from Linux sysfs.
use super::sysfs::{BELOW, DIRECTORY, attribute, inventory_page, is_sysfs};
use crate::{Result, err, host::Work, linux_clock, now, require};
use psios_api::{DeviceInventoryQuery, sha256};
use rustix::{
    fd::OwnedFd,
    fs::{Dir, FileType, Mode, OFlags, fstat, open, openat2, readlinkat},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    ffi::OsStr,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Component, Path},
};

pub(crate) const TOOL: &str = "node.usb";
pub(crate) const FORMAT: u32 = super::pci::FORMAT + 1;
const MAX_DEVICES: usize = 256;
const MAX_ENTRIES: usize = 4096;

fn number(value: &str) -> Option<u16> {
    let parsed: u16 = value.parse().ok()?;
    (parsed.to_string() == value && parsed > 0).then_some(parsed)
}

fn location(name: &str) -> Option<(u16, &str)> {
    if let Some(bus) = name.strip_prefix("usb") {
        return Some((number(bus)?, "0"));
    }
    let (bus, route) = name.split_once('-')?;
    (route.len() <= 32
        && route
            .split('.')
            .all(|port| number(port).is_some_and(|port| port <= u8::MAX.into())))
    .then_some((number(bus)?, route))
}

fn changed(error: io::Error) -> crate::Error {
    if error.kind() == io::ErrorKind::NotFound {
        err("Conflict", "a USB device changed during observation")
    } else {
        error.into()
    }
}

fn descriptor(dir: &OwnedFd, name: &str, digits: usize) -> Result<String> {
    let value = attribute(dir, name).map_err(changed)?;
    require(
        value.len() == digits
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "HostFailure",
        "invalid USB descriptor in sysfs",
    )?;
    Ok(value)
}

fn negotiated_speed(dir: &OwnedFd) -> Result<Option<String>> {
    let value = attribute(dir, "speed").map_err(changed)?;
    if value == "unknown" {
        return Ok(None);
    }
    require(
        (1..=9).contains(&value.len())
            && value.split('.').count() <= 2
            && value
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())),
        "HostFailure",
        "invalid negotiated USB speed in sysfs",
    )?;
    Ok(Some(value))
}

#[derive(PartialEq, Eq, Serialize)]
struct Device {
    location: String,
    parent_location: Option<String>,
    bus_number: u16,
    device_address: u16,
    vendor_id: String,
    product_id: String,
    device_release: String,
    class_code: String,
    subclass_code: String,
    protocol_code: String,
    speed_mbps: Option<String>,
    authorized: bool,
    #[serde(skip)]
    sysfs_identity: (u64, u64),
}

struct Tree {
    root: OwnedFd,
    entries: OwnedFd,
}

impl Tree {
    fn open() -> Result<Self> {
        let unavailable = |_| err("DependencyUnavailable", "kernel USB sysfs is unavailable");
        let root =
            open("/sys", DIRECTORY | OFlags::NOFOLLOW, Mode::empty()).map_err(unavailable)?;
        require(
            is_sysfs(&root)?,
            "UnsupportedCapability",
            "USB observation requires Linux sysfs",
        )?;
        let entries = openat2(&root, "bus/usb/devices", DIRECTORY, Mode::empty(), BELOW)
            .map_err(unavailable)?;
        Ok(Self { root, entries })
    }

    fn device(&self, name: &str, bus: u16, route: &str) -> Result<Device> {
        let link = readlinkat(&self.entries, name, Vec::new())
            .map_err(io::Error::from)
            .map_err(changed)?;
        let link = Path::new(OsStr::from_bytes(link.as_bytes()));
        let path = link
            .strip_prefix("../../..")
            .ok()
            .filter(|path| {
                path.starts_with("devices")
                    && path
                        .components()
                        .all(|component| matches!(component, Component::Normal(_)))
                    && path.file_name() == Some(OsStr::new(name))
            })
            .ok_or_else(|| err("HostFailure", "USB link is outside the kernel device tree"))?;
        let dir = openat2(&self.root, path, DIRECTORY, Mode::empty(), BELOW)
            .map_err(io::Error::from)
            .map_err(changed)?;
        let identity = fstat(&dir).map_err(io::Error::from)?;
        let address = number(&attribute(&dir, "devnum").map_err(changed)?)
            .filter(|address| *address <= 127)
            .ok_or_else(|| err("HostFailure", "invalid USB device address in sysfs"))?;
        require(
            number(&attribute(&dir, "busnum").map_err(changed)?) == Some(bus)
                && attribute(&dir, "devpath").map_err(changed)? == route,
            "Conflict",
            "the USB bus location changed during observation",
        )?;
        let authorized = match attribute(&dir, "authorized").map_err(changed)?.as_str() {
            "0" => false,
            "1" => true,
            _ => {
                return Err(err(
                    "HostFailure",
                    "invalid USB authorization state in sysfs",
                ));
            }
        };
        let parent_location = if route == "0" {
            None
        } else if let Some((parent, _)) = route.rsplit_once('.') {
            Some(format!("{bus}-{parent}"))
        } else {
            Some(format!("usb{bus}"))
        };
        Ok(Device {
            location: name.to_owned(),
            parent_location,
            bus_number: bus,
            device_address: address,
            vendor_id: descriptor(&dir, "idVendor", 4)?,
            product_id: descriptor(&dir, "idProduct", 4)?,
            device_release: descriptor(&dir, "bcdDevice", 4)?,
            class_code: descriptor(&dir, "bDeviceClass", 2)?,
            subclass_code: descriptor(&dir, "bDeviceSubClass", 2)?,
            protocol_code: descriptor(&dir, "bDeviceProtocol", 2)?,
            speed_mbps: negotiated_speed(&dir)?,
            authorized,
            sysfs_identity: (identity.st_dev, identity.st_ino),
        })
    }

    fn scan(&self) -> Result<Vec<Device>> {
        let mut devices = Vec::new();
        for (index, entry) in Dir::read_from(&self.entries)
            .map_err(io::Error::from)?
            .enumerate()
        {
            require(
                index < MAX_ENTRIES,
                "QuotaExceeded",
                "USB sysfs contains too many entries",
            )?;
            let entry = entry.map_err(io::Error::from)?;
            let name = entry
                .file_name()
                .to_str()
                .map_err(|_| err("HostFailure", "invalid name in the kernel USB directory"))?;
            // Interface names contain a configuration and interface suffix.
            if matches!(name, "." | "..") || name.contains(':') {
                continue;
            }
            require(
                devices.len() < MAX_DEVICES,
                "QuotaExceeded",
                "USB observation exceeds 256 devices",
            )?;
            let (bus, route) = location(name)
                .filter(|_| entry.file_type() == FileType::Symlink)
                .ok_or_else(|| {
                    err(
                        "HostFailure",
                        "invalid device entry in the kernel USB directory",
                    )
                })?;
            devices.push(self.device(name, bus, route)?);
        }
        devices.sort_unstable_by(|left, right| left.location.cmp(&right.location));
        require(
            devices
                .windows(2)
                .all(|rows| rows[0].location != rows[1].location),
            "Conflict",
            "the kernel USB directory changed during observation",
        )?;
        Ok(devices)
    }
}

pub(crate) fn validate_inventory(query: &DeviceInventoryQuery) -> Result<()> {
    super::sysfs::validate_inventory(query, "USB inventory", MAX_DEVICES)
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
        "the USB inventory changed during observation",
    )?;
    let identities: Vec<_> = devices.iter().map(|device| device.sysfs_identity).collect();
    let digest = sha256(&serde_json::to_vec(&(
        "psios/node.usb/1",
        work.target.reference(),
        &work.task.owner,
        &work.task.spec["delegation_ref"],
        &boot_id,
        mount_namespace,
        (before.st_dev, before.st_ino),
        identities,
        &devices,
    ))?);
    let (devices, next_cursor) = inventory_page(&devices, query, &digest, "USB inventory")?;
    Ok(
        json!({"boot_id":boot_id,"mount_namespace_inode":mount_namespace.to_string(),
              "observed_at":now(),"devices":devices,"snapshot_digest":digest,"next_cursor":next_cursor}),
    )
}
