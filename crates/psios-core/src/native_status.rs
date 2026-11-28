//! Read evidence visible to a local operator even when Core is unavailable.
use crate::{Result, err, linux_clock, linux_proc, node::mounts, require};
use rustix::fs::OFlags;
use serde_json::{Value, json};
use std::{
    fs,
    io::{self, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::Path,
};
use uuid::Uuid;

const MANIFEST: &str = "/psios-deployment.json";
const MANIFEST_LIMIT: u64 = 64 * 1024;

fn manifest(path: &Path) -> Result<(Value, String)> {
    let unavailable = || {
        err(
            "UnsupportedCapability",
            "expected a native PsiOS deployment manifest",
        )
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|_| unavailable())?;
    let info = file.metadata()?;
    require(
        info.is_file()
            && info.uid() == 0
            && info.gid() == 0
            && info.nlink() == 1
            && info.mode() & 0o7777 == 0o644
            && (1..=MANIFEST_LIMIT).contains(&info.len()),
        "UnsupportedCapability",
        "native deployment manifest lacks expected custody or size",
    )?;
    let mut bytes = Vec::with_capacity(info.len() as usize);
    (&file).take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes)?;
    let revision = |metadata: &fs::Metadata| {
        (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    };
    require(
        bytes.len() as u64 == info.len()
            && revision(&info) == revision(&fs::symlink_metadata(path)?),
        "Conflict",
        "native deployment manifest changed during observation",
    )?;
    let data = psios_api::parse(&bytes)?;
    let source = data["core_source_commit"].as_str().unwrap_or_default();
    let volumes = &data["required_filesystems"];
    let uuid = |key: &str| {
        volumes[key].as_str().is_some_and(|value| {
            Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
        })
    };
    if data["format"].as_u64() != Some(1)
        || data["architecture"] != "x86_64"
        || data["firmware"] != "UEFI"
        || source.len() != 40
        || !source
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || !uuid("state")
        || !uuid("home")
        || volumes["state"] == volumes["home"]
    {
        return Err(unavailable());
    }
    Ok((data, psios_api::sha256(&bytes)))
}

fn socket_status(path: &Path) -> &'static str {
    match fs::symlink_metadata(path) {
        Ok(info) if info.file_type().is_socket() => "Present",
        Ok(_) => "UnexpectedType",
        Err(error) if error.kind() == io::ErrorKind::NotFound => "Absent",
        Err(_) => "Unavailable",
    }
}

/// Observe the installed native boot in the caller's Linux namespace.
pub fn native_status() -> Result<Value> {
    let (manifest, digest) = manifest(Path::new(MANIFEST))?;
    let proc = linux_proc::root()?;
    let boot_id = linux_clock::boot_id_in(&proc)?;
    let kernel = linux_proc::kernel_release(&proc)?;
    let storage = mounts::native_storage(&proc)?;
    Ok(json!({
        "boot_id": boot_id,
        "kernel_release": kernel,
        "boottime_ms": linux_clock::elapsed_millis()?,
        "deployment_manifest_sha256": digest,
        "core_source_commit": manifest["core_source_commit"],
        "firmware": if Path::new("/sys/firmware/efi").is_dir() { "UEFI" } else { "Unavailable" },
        "secure_boot_required": manifest["secure_boot"].is_object()
            || manifest["secure_boot_enforcement_required"] == true,
        "expected_filesystems": {
            "state": manifest["required_filesystems"]["state"],
            "home": manifest["required_filesystems"]["home"],
        },
        "storage": storage,
        "core_socket": socket_status(Path::new("/run/psios/api.sock")),
    }))
}
