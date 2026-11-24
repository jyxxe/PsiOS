use super::{BackupManifest, Capture, Entry, ModifiedTime, capture_root};
use crate::{Result, decode, host::Work, now, require};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestoreProof {
    tree_digest: String,
    includes_mtime: bool,
}

fn tree_digest<'a>(
    entries: impl Iterator<Item = (&'a Entry, Option<ModifiedTime>)>,
    root_time: Option<ModifiedTime>,
) -> Result<String> {
    let mut entries: Vec<_> = entries.collect();
    entries.sort_unstable_by(|(a, _), (b, _)| a.path.cmp(&b.path));
    let mut digest = Sha256::new();
    digest.update(b"psios-restored-tree-v1\n");
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
        tree_digest: tree_digest(entries, times.map(|t| t.root))?,
        includes_mtime: times.is_some(),
    })
}

pub(super) fn verify(w: &Work) -> Result<Value> {
    let Some(expected) = w.target.spec.get("restore_proof") else {
        // Pending clones from earlier authorities have no committed content fingerprint.
        return Ok(json!({}));
    };
    let expected: RestoreProof = decode(expected)?;
    let root = capture_root(w)?;
    let before = root.metadata()?;
    let mut capture = Capture {
        out: None,
        entries: vec![],
        total: 0,
    };
    capture.walk(&root, Path::new(""), 0)?;
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
        tree_digest(entries, root_time)? == expected.tree_digest,
        "PreconditionFailed",
        "restored volume no longer matches its verified backup tree",
    )?;
    capture.confirm_source(&capture_root(w)?, &before)?;
    Ok(json!({"verified_at": now()}))
}
