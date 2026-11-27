use super::{
    BackupManifest, Capture, CaptureMode, Entry, ModifiedTime, capture_root, checked_root_metadata,
    root_attribute_profile, xattrs,
};
use crate::{MAX_BACKUP_BYTES, Result, decode, host::Work, now, require};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::File, os::unix::fs::MetadataExt, path::Path};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreProof {
    tree_digest: String,
    includes_mtime: bool,
    #[serde(
        default,
        deserialize_with = "super::decode_root_mode",
        skip_serializing_if = "Option::is_none"
    )]
    root_mode_unix_v1: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_xattrs_digest: Option<String>,
}

fn tree_digest<'a>(
    entries: impl Iterator<Item = (&'a Entry, Option<ModifiedTime>)>,
    root_time: Option<ModifiedTime>,
    root_mode: Option<u32>,
) -> Result<String> {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_unstable_by(|(a, _), (b, _)| a.path.cmp(&b.path));
    let mut digest = Sha256::new();
    if let Some(mode) = root_mode {
        digest.update(b"psios-restored-tree-v2\n");
        digest.update(serde_json::to_vec(&mode)?);
        digest.update(b"\n");
    } else {
        digest.update(b"psios-restored-tree-v1\n");
    }
    digest.update(serde_json::to_vec(&root_time)?);
    for entry in entries {
        digest.update(b"\n");
        digest.update(serde_json::to_vec(&entry)?);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

pub(super) fn from_manifest(manifest: &BackupManifest) -> Result<RestoreProof> {
    let times = manifest.mtime_unix_v1.as_ref();
    let entries = manifest
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry, times.map(|t| t.entries[i])));
    Ok(RestoreProof {
        tree_digest: tree_digest(entries, times.map(|t| t.root), manifest.root_mode_unix_v1)?,
        includes_mtime: times.is_some(),
        root_mode_unix_v1: manifest.root_mode_unix_v1,
        user_xattrs_digest: manifest
            .user_xattrs_unix_v1
            .as_ref()
            .map(xattrs::Tree::digest)
            .transpose()?,
    })
}

pub(super) fn from_capture(
    entries: &[(Entry, std::fs::Metadata)],
    root_time: ModifiedTime,
    root_mode: u32,
    user_xattrs: Option<&xattrs::Tree>,
) -> Result<RestoreProof> {
    let entries = entries
        .iter()
        .map(|(entry, meta)| (entry, Some(ModifiedTime::from_metadata(meta))));
    Ok(RestoreProof {
        tree_digest: tree_digest(entries, Some(root_time), Some(root_mode))?,
        includes_mtime: true,
        root_mode_unix_v1: Some(root_mode),
        user_xattrs_digest: user_xattrs.map(xattrs::Tree::digest).transpose()?,
    })
}

pub(super) fn has_user_attributes(proof: &Value) -> bool {
    proof["user_xattrs_digest"].is_string()
}

pub(super) fn check_clone_proof(proof: &Value) -> Result<()> {
    let proof: RestoreProof = decode(proof)?;
    require(
        proof.includes_mtime
            && proof
                .root_mode_unix_v1
                .is_some_and(|mode| mode <= 0o777 && mode & 0o500 == 0o500)
            && super::file_integrity::sha256_format(&proof.tree_digest)
            && proof
                .user_xattrs_digest
                .as_deref()
                .is_none_or(super::file_integrity::sha256_format),
        "RecoveryRequired",
        "volume clone has no complete captured tree proof",
    )
}

pub(super) fn verify(w: &Work) -> Result<Value> {
    let clone = w.target.spec["cloned_from"].is_object();
    let open = || capture_root(w);
    let Some(expected) = w.target.spec.get("restore_proof") else {
        require(
            !clone,
            "RecoveryRequired",
            "volume clone lacks its captured content proof",
        )?;
        checked_root_metadata(&open()?, "PreconditionFailed")?;
        // Pending clones from earlier authorities have no committed content fingerprint.
        return Ok(json!({}));
    };
    if clone {
        check_clone_proof(expected)?;
    }
    let root = open()?;
    verify_tree(&root, expected, open)?;
    Ok(json!({"verified_at":now()}))
}

pub(super) fn verify_tree(
    root: &File,
    expected: &Value,
    reopen: impl FnOnce() -> Result<File>,
) -> Result<()> {
    verify_tree_checked(root, expected, reopen, &mut || Ok(()))
}

pub(super) fn verify_tree_checked(
    root: &File,
    expected: &Value,
    reopen: impl FnOnce() -> Result<File>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let expected: RestoreProof = decode(expected)?;
    let preserve = expected.user_xattrs_digest.is_some();
    let (before, root_attributes) = root_attribute_profile(root, "PreconditionFailed", preserve)?;
    let root_mode = expected.root_mode_unix_v1.map(|_| before.mode() & 0o7777);
    require(
        root_mode == expected.root_mode_unix_v1 && root_mode.is_none_or(|mode| mode <= 0o777),
        "PreconditionFailed",
        "restored data root mode differs from its verified backup",
    )?;
    let mut capture = Capture::new(
        CaptureMode::DigestOnly,
        MAX_BACKUP_BYTES,
        super::MAX_ENTRIES,
        check,
    )
    .with_root_attributes(root_attributes);
    capture.walk(root, Path::new(""), 0)?;
    let entries = capture.entries.iter().map(|(entry, meta)| {
        (
            entry,
            expected
                .includes_mtime
                .then(|| ModifiedTime::from_metadata(meta)),
        )
    });
    let root_time = expected
        .includes_mtime
        .then(|| ModifiedTime::from_metadata(&before));
    require(
        tree_digest(entries, root_time, root_mode)? == expected.tree_digest,
        "PreconditionFailed",
        "restored volume no longer matches its verified backup tree",
    )?;
    require(
        capture
            .user_xattrs
            .as_ref()
            .map(xattrs::Tree::digest)
            .transpose()?
            == expected.user_xattrs_digest,
        "PreconditionFailed",
        "restored volume user attributes do not match the verified backup tree",
    )?;
    let current = reopen()?;
    root_attribute_profile(&current, "PreconditionFailed", preserve)?;
    capture.confirm_source(&current, &before)
}
