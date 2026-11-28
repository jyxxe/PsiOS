//! Read evidence visible to a local operator even when Core is unavailable.
use crate::{Result, err, linux_clock, linux_proc, node::mounts, require};
use rustix::fs::OFlags;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::Path,
};
use uuid::Uuid;

const MANIFEST: &str = "/psios-deployment.json";
const BOOT_EVIDENCE: &str = "/run/psios-boot.json";
const MANIFEST_LIMIT: u64 = 64 * 1024;
const EVIDENCE_LIMIT: u64 = 1024;

fn read_record(path: &Path, mode: u32, limit: u64) -> Result<Option<Vec<u8>>> {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let info = file.metadata()?;
    require(
        info.is_file()
            && info.uid() == 0
            && info.gid() == 0
            && info.nlink() == 1
            && info.mode() & 0o7777 == mode
            && (1..=limit).contains(&info.len()),
        "UnsupportedCapability",
        "native metadata lacks expected custody or size",
    )?;
    let mut bytes = Vec::with_capacity(info.len() as usize);
    (&file).take(limit + 1).read_to_end(&mut bytes)?;
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
        "native metadata changed during observation",
    )?;
    Ok(Some(bytes))
}

fn manifest(path: &Path) -> Result<(Value, String)> {
    let unavailable = || {
        err(
            "UnsupportedCapability",
            "expected a native PsiOS deployment manifest",
        )
    };
    let bytes = read_record(path, 0o644, MANIFEST_LIMIT)?.ok_or_else(unavailable)?;
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
        || data
            .get("uefi_boot_entries")
            .is_some_and(|enabled| enabled.as_bool() != Some(true))
    {
        return Err(unavailable());
    }
    Ok((data, psios_api::sha256(&bytes)))
}

fn boot_evidence(path: &Path, boot_id: &str, digest: &str) -> Result<Option<Value>> {
    let Some(bytes) = read_record(path, 0o444, EVIDENCE_LIMIT)? else {
        return Ok(None);
    };
    let record = psios_api::parse(&bytes)?;
    require(
        record["format"].as_u64() == Some(1)
            && record["boot_id"] == boot_id
            && record["deployment_manifest_sha256"] == digest,
        "Conflict",
        "native evidence does not match the current deployment and boot",
    )?;
    Ok(Some(record))
}

fn secure_boot_observation(evidence: &Result<Option<Value>>, required: bool) -> &'static str {
    match evidence {
        Ok(Some(record)) => match record["secure_boot"].as_str() {
            Some("Enforced") if required => "Enforced",
            Some("Unmeasured") if !required => "Unmeasured",
            _ => "Invalid",
        },
        Ok(None) => "Unavailable",
        Err(_) => "Invalid",
    }
}

fn uefi_boot_entries(evidence: &Result<Option<Value>>, enabled: bool) -> Value {
    if !enabled {
        return json!({"status": "NotRequested"});
    }
    let entries = match evidence {
        Ok(Some(record)) => &record["uefi_boot_entries"],
        Ok(None) => return json!({"status": "Unavailable"}),
        Err(_) => return json!({"status": "Invalid"}),
    };
    if entries["status"] == "Unavailable" {
        return json!({"status": "Unavailable"});
    }
    if entries["status"] == "Observed" {
        let entry = |value: &Value| u16::try_from(value.as_u64()?).ok();
        if let (Some(current), Some(order)) =
            (entry(&entries["current"]), entries["order"].as_array())
            && (1..=64).contains(&order.len())
            && order
                .iter()
                .map(entry)
                .collect::<Option<BTreeSet<_>>>()
                .is_some_and(|seen| seen.len() == order.len())
        {
            return json!({"status": "Observed", "current": current, "order": order});
        }
    }
    json!({"status": "Invalid"})
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
    let secure_boot_required = manifest["secure_boot_enforcement_required"] == true;
    let evidence = boot_evidence(Path::new(BOOT_EVIDENCE), &boot_id, &digest);
    Ok(json!({
        "boot_id": boot_id,
        "kernel_release": kernel,
        "boottime_ms": linux_clock::elapsed_millis()?,
        "deployment_manifest_sha256": digest,
        "core_source_commit": manifest["core_source_commit"],
        "firmware": if Path::new("/sys/firmware/efi").is_dir() { "UEFI" } else { "Unavailable" },
        "secure_boot_required": secure_boot_required,
        "secure_boot_observation": secure_boot_observation(&evidence, secure_boot_required),
        "uefi_boot_entries": uefi_boot_entries(&evidence, manifest["uefi_boot_entries"] == true),
        "expected_filesystems": {
            "state": manifest["required_filesystems"]["state"],
            "home": manifest["required_filesystems"]["home"],
        },
        "storage": storage,
        "core_socket": socket_status(Path::new("/run/psios/api.sock")),
    }))
}
