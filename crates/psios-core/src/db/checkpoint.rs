use super::{Store, open_regular, resolve, verify};
use crate::{
    Config, Result, artifact,
    config::{new_file, private_dir},
    decode, err, now, require,
    storage::copy_hash,
};
use psios_api::{VERSION, canonical, id, parse, sha256};
use rusqlite::Connection;
use rustix::fs::{RenameFlags, renameat_with};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, Permissions},
    io::{ErrorKind, Read, sink},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

const FORMAT: &str = "psios.authority.checkpoint.v1";
const MANIFEST: &str = "manifest.json";
const MAX_MANIFEST: u64 = 64 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    format: String,
    schema_version: String,
    checkpoint_id: String,
    captured_at: String,
    identity: verify::DatabaseIdentity,
    witness_at_capture: String,
    owner_rotation_at_capture: String,
    members: BTreeMap<String, Member>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Member {
    bytes: String,
    digest: String,
}

fn report(checkpoint: &Checkpoint, digest: &str) -> Value {
    json!({"manifest_digest":digest,"manifest":checkpoint})
}

fn stage(parent: &Path) -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(".psios-checkpoint-")
        .permissions(Permissions::from_mode(0o700))
        .tempdir_in(parent)?)
}

fn capture_members(directory: &Path, max_bytes: u64) -> Result<BTreeMap<String, Member>> {
    let mut members = BTreeMap::new();
    let mut remaining = max_bytes;
    for &name in verify::DATABASE_FILES {
        let mut file = match open_regular(&directory.join(name)) {
            Err(e) if e.kind() == ErrorKind::NotFound && name != "state.db" => continue,
            entry => entry?,
        };
        let (bytes, digest) = copy_hash(&mut file, &mut sink(), remaining)?;
        remaining -= bytes;
        file.sync_all()?;
        members.insert(
            name.into(),
            Member {
                bytes: bytes.to_string(),
                digest,
            },
        );
    }
    Ok(members)
}

fn copy_members(
    checkpoint: &Checkpoint,
    source: &Path,
    destination: &Path,
    limit: u64,
) -> Result<()> {
    require(
        checkpoint.members.contains_key("state.db")
            && checkpoint
                .members
                .keys()
                .all(|name| verify::DATABASE_FILES.contains(&name.as_str())),
        "PreconditionFailed",
        "checkpoint manifest contains unsupported database entries",
    )?;
    let mut expected = checkpoint.members.len() + 1;
    for item in fs::read_dir(source)? {
        let name = item?.file_name();
        require(
            name.to_str()
                .is_some_and(|name| name == MANIFEST || checkpoint.members.contains_key(name)),
            "PreconditionFailed",
            "checkpoint contains an unexpected entry",
        )?;
        expected = expected
            .checked_sub(1)
            .ok_or_else(|| err("PreconditionFailed", "checkpoint directory changed"))?;
    }
    require(
        expected == 0,
        "PreconditionFailed",
        "checkpoint member is missing",
    )?;

    let mut remaining = limit;
    for (name, member) in &checkpoint.members {
        let size: u64 = member
            .bytes
            .parse()
            .ok()
            .filter(|size: &u64| size.to_string() == member.bytes)
            .ok_or_else(|| err("PreconditionFailed", "checkpoint member size is invalid"))?;
        require(
            size <= remaining,
            "QuotaExceeded",
            "checkpoint exceeds verification copy limit",
        )?;
        let mut input = open_regular(&source.join(name))?;
        require(
            input.metadata()?.len() == size,
            "PreconditionFailed",
            "checkpoint member length differs from the manifest",
        )?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(destination.join(name))?;
        let (bytes, digest) = copy_hash(&mut input, &mut output, size)?;
        require(
            bytes == size && digest == member.digest,
            "PreconditionFailed",
            "checkpoint member digest mismatch",
        )?;
        remaining -= size;
    }
    Ok(())
}

fn with_checkpoint<T>(
    directory: &Path,
    expected_digest: &str,
    scratch: &Path,
    max_bytes: u64,
    action: impl FnOnce(&Connection, &Checkpoint) -> Result<T>,
) -> Result<T> {
    require(
        fs::symlink_metadata(directory)?.is_dir(),
        "InvalidArgument",
        "checkpoint must be a real directory",
    )?;
    let source = directory.canonicalize()?;
    private_dir(&source, false)?;
    let manifest = open_regular(&source.join(MANIFEST))?;
    let mut bytes = Vec::new();
    manifest.take(MAX_MANIFEST + 1).read_to_end(&mut bytes)?;
    require(
        bytes.len() as u64 <= MAX_MANIFEST && sha256(&bytes) == expected_digest,
        "PreconditionFailed",
        "trusted checkpoint manifest digest mismatch",
    )?;
    let checkpoint: Checkpoint = decode(&parse(&bytes)?)?;
    require(
        checkpoint.format == FORMAT && checkpoint.schema_version == VERSION,
        "SchemaVersionUnsupported",
        "unsupported authority checkpoint format",
    )?;
    let parent = verify::scratch_parent(scratch)?;
    require(
        !parent.starts_with(&source),
        "InvalidArgument",
        "verification scratch must be outside the checkpoint",
    )?;
    let tmp = stage(&parent)?;
    copy_members(&checkpoint, &source, tmp.path(), max_bytes)?;
    let (copy, identity) = verify::checked_copy(tmp.path())?;
    require(
        identity == checkpoint.identity,
        "RecoveryRequired",
        "checkpoint identity differs from its database",
    )?;
    action(&copy, &checkpoint)
}

impl Store {
    pub fn checkpoint(cfg: &Config, output: &Path, max_bytes: u64) -> Result<Value> {
        let _lock = verify::lock_offline(cfg)?;
        let name = output.file_name().ok_or_else(|| {
            err(
                "InvalidArgument",
                "checkpoint output requires a new directory name",
            )
        })?;
        let parent = output
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()?;
        private_dir(&parent, false)?;
        verify::outside_authority(cfg, &parent)?;
        let root = File::open(&parent)?;
        let tmp = stage(&parent)?;
        verify::copy_database(&cfg.state_dir, tmp.path(), max_bytes)?;
        let (copy, identity) = verify::checked_copy(tmp.path())?;
        let (witness, rotation) = verify::verified_witness(&copy, cfg)?;
        verify::check_artifacts(&copy, &identity)?;
        copy.close().map_err(|(_, error)| error)?;
        // The shared-memory index is recreated from the archived database and WAL.
        match fs::remove_file(tmp.path().join("state.db-shm")) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let checkpoint = Checkpoint {
            format: FORMAT.into(),
            schema_version: VERSION.into(),
            checkpoint_id: id("authority-checkpoint"),
            captured_at: now(),
            identity,
            witness_at_capture: witness.into(),
            owner_rotation_at_capture: rotation.into(),
            members: capture_members(tmp.path(), max_bytes)?,
        };
        let raw = canonical(&json!(checkpoint));
        new_file(&tmp.path().join(MANIFEST), &raw)?;
        renameat_with(
            &root,
            tmp.path().file_name().unwrap(),
            &root,
            name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|error| {
            if error == rustix::io::Errno::EXIST {
                err("Conflict", "checkpoint output already exists")
            } else {
                std::io::Error::from(error).into()
            }
        })?;
        let _ = tmp.keep();
        root.sync_all().map_err(|_| {
            err(
                "OutcomeUnknown",
                "checkpoint published but directory sync failed",
            )
        })?;
        Ok(report(&checkpoint, &sha256(&raw)))
    }

    pub fn verify_checkpoint(
        directory: &Path,
        digest: &str,
        scratch: &Path,
        max_bytes: u64,
    ) -> Result<Value> {
        with_checkpoint(directory, digest, scratch, max_bytes, |copy, checkpoint| {
            verify::check_artifacts(copy, &checkpoint.identity)?;
            Ok(report(checkpoint, digest))
        })
    }

    pub fn checkpoint_artifact(
        directory: &Path,
        digest: &str,
        scratch: &Path,
        max_copy_bytes: u64,
        reference: &Value,
        max_artifact_bytes: usize,
    ) -> Result<Vec<u8>> {
        with_checkpoint(directory, digest, scratch, max_copy_bytes, |copy, _| {
            let artifact = resolve(copy, reference)?;
            require(
                artifact.kind == "Artifact",
                "InvalidArgument",
                "expected a result artifact reference",
            )?;
            artifact::checkpoint_bytes(copy, &artifact, max_artifact_bytes)
        })
    }
}
