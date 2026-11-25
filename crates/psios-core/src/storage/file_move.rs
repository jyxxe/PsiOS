//! Witnessed, no-replacement rename within one exclusive directory volume.
use super::{file_integrity, files, unknown};
use crate::{
    Config, Result,
    db::{Db, Doc},
    decode, err,
    host::{Target, Work},
    require,
};
use file_integrity::Revision;
use rusqlite::params;
use rustix::fs::{AtFlags, RenameFlags, renameat_with, statat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs::File, os::unix::fs::MetadataExt};

pub(crate) const TOOL: &str = "volume.rename_file";
pub(crate) const INSPECT: &str = "volume.inspect_rename";
pub(crate) const FORMAT: u32 = super::read_holds::FORMAT + 1;
const INTENT: &str = "VolumeFileRenameIntent";
const CHECKPOINT: &str = "VolumeFileRenameCheckpoint";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub target: Target,
    source: String,
    destination: String,
    expected_observation_digest: String,
}

impl Request {
    pub(crate) fn validate(&self) -> Result<()> {
        files::path(&self.source, false)?;
        files::path(&self.destination, false)?;
        require(
            self.source != self.destination
                && file_integrity::sha256_format(&self.expected_observation_digest),
            "InvalidArgument",
            "rename requires distinct paths and an observed file digest",
        )
    }
}

type DirectoryIdentity = [String; 2];

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Witness {
    source_parent: DirectoryIdentity,
    destination_parent: DirectoryIdentity,
    revision: Revision,
    content_digest: String,
    observation_digest: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
enum Placement {
    Original,
    Renamed,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    checkpoint_ref: Value,
    operation_ref: Value,
    placement: Placement,
    revision: Revision,
    content_digest: String,
}

struct Paths {
    source: (File, String),
    destination: (File, String),
}

fn identity(file: &File) -> Result<DirectoryIdentity> {
    let metadata = file.metadata()?;
    Ok([metadata.dev().to_string(), metadata.ino().to_string()])
}

fn present((dir, name): &(File, String)) -> Result<bool> {
    match statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Ok(true),
        Err(Errno::NOENT) => Ok(false),
        Err(error) => Err(std::io::Error::from(error).into()),
    }
}

impl Paths {
    fn open(config: &Config, volume: &Doc, source: &str, destination: &str) -> Result<Self> {
        let root = file_integrity::volume_root(config, volume)?;
        Ok(Self {
            source: files::parent(&root, source)?,
            destination: files::parent(&root, destination)?,
        })
    }

    fn check_parents(&self, witness: &Witness) -> Result<()> {
        require(
            identity(&self.source.0)? == witness.source_parent
                && identity(&self.destination.0)? == witness.destination_parent,
            "PreconditionFailed",
            "rename directory identity changed",
        )
    }

    fn prepare(&self, revision: &Revision) -> Result<()> {
        require(
            file_integrity::revision(&self.source.0, &self.source.1)? == *revision,
            "PreconditionFailed",
            "source differs from the approved file observation",
        )?;
        require(
            !present(&self.destination)?,
            "Conflict",
            "rename destination already exists",
        )
    }

    fn state(&self, witness: &Witness) -> Result<(Placement, Revision)> {
        self.check_parents(witness)?;
        match (present(&self.source)?, present(&self.destination)?) {
            (true, false) => {
                let revision = file_integrity::revision(&self.source.0, &self.source.1)?;
                require(
                    witness.revision == revision,
                    "PreconditionFailed",
                    "source identity contradicts the rename witness",
                )?;
                Ok((Placement::Original, revision))
            }
            (false, true) => {
                let revision = file_integrity::revision(&self.destination.0, &self.destination.1)?;
                require(
                    witness.revision.same_after_rename(&revision),
                    "PreconditionFailed",
                    "destination identity contradicts the rename witness",
                )?;
                Ok((Placement::Renamed, revision))
            }
            _ => Err(err(
                "PreconditionFailed",
                "rename placement cannot be verified",
            )),
        }
    }

    fn sync(&self) -> Result<()> {
        self.destination.0.sync_all()?;
        self.source.0.sync_all()?;
        Ok(())
    }
}

fn receipt(source: &str, destination: &str, witness: &Witness, revision: &Revision) -> Value {
    json!({"source":source,"destination":destination,"content_digest":witness.content_digest,
        "source_observation_digest":witness.observation_digest,"revision":revision})
}

pub(crate) fn rename(
    work: &Work,
    request: &Request,
    checkpoint: impl FnOnce(&Witness) -> Result<()>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Value> {
    let volume = &work.target;
    let paths = Paths::open(&work.cfg, volume, &request.source, &request.destination)?;
    let (revision, content_digest) = file_integrity::capture(
        &paths.source.0,
        &paths.source.1,
        file_integrity::MAX_BYTES,
        check,
    )?;
    let observation =
        file_integrity::observation(volume, &request.source, &revision, &content_digest)?;
    require(
        observation["observation_digest"] == request.expected_observation_digest,
        "PreconditionFailed",
        "file observation changed since the rename was prepared",
    )?;
    paths.prepare(&revision)?;
    let witness = Witness {
        source_parent: identity(&paths.source.0)?,
        destination_parent: identity(&paths.destination.0)?,
        revision,
        content_digest,
        observation_digest: request.expected_observation_digest.clone(),
    };
    checkpoint(&witness)?;

    let paths = Paths::open(&work.cfg, volume, &request.source, &request.destination)?;
    paths.check_parents(&witness)?;
    paths.prepare(&witness.revision)?;
    match renameat_with(
        &paths.source.0,
        paths.source.1.as_str(),
        &paths.destination.0,
        paths.destination.1.as_str(),
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(Errno::EXIST) => return Err(err("Conflict", "rename destination already exists")),
        Err(Errno::NOENT) => return Err(err("PreconditionFailed", "rename path changed")),
        Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP | Errno::XDEV) => {
            return Err(err(
                "UnsupportedCapability",
                "filesystem cannot provide the approved atomic rename",
            ));
        }
        Err(error) => return Err(unknown(error)),
    }
    paths.sync().map_err(unknown)?;
    let current =
        Paths::open(&work.cfg, volume, &request.source, &request.destination).map_err(unknown)?;
    let (placement, observed) = current.state(&witness).map_err(unknown)?;
    require(
        placement == Placement::Renamed,
        "OutcomeUnknown",
        "rename success was not observed",
    )?;
    // The effect was issued. Evidence collection continues under the retained volume hold.
    let (verified, bytes_digest) = file_integrity::capture(
        &current.destination.0,
        &current.destination.1,
        file_integrity::MAX_BYTES,
        &mut || Ok(()),
    )
    .map_err(unknown)?;
    let (placement, latest) = Paths::open(&work.cfg, volume, &request.source, &request.destination)
        .and_then(|opened| opened.state(&witness))
        .map_err(unknown)?;
    require(
        placement == Placement::Renamed
            && observed == verified
            && verified == latest
            && bytes_digest == witness.content_digest,
        "OutcomeUnknown",
        "moved bytes or placement changed before confirmation",
    )?;
    Ok(receipt(
        &request.source,
        &request.destination,
        &witness,
        &verified,
    ))
}

fn intent_id(operation: &str) -> String {
    format!("file-rename-intent-{operation}")
}

fn checkpoint_id(operation: &str) -> String {
    format!("file-rename-checkpoint-{operation}")
}

fn matches_operation(doc: &Doc, operation: &Doc, kind: &str) -> bool {
    doc.kind == kind
        && doc.owner == operation.owner
        && doc.spec["operation_id"] == operation.id
        && doc.body["plan_ref"] == operation.body["plan_ref"]
        && doc.body["effect_id"] == operation.body["effect_refs"][0]["resource_id"]
}

fn held(db: &Db<'_>, operation: &Doc, volume: &Doc) -> Result<()> {
    let held = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, operation.id],
        |row| row.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "original exclusive volume hold is unavailable",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.op, &work.target)?;
    db.create(
        INTENT, &intent_id(&work.op.id), &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,"volume_ref":work.target.reference()}),
    )?;
    Ok(())
}

pub(crate) fn not_prepared(db: &Db<'_>, operation: &Doc) -> Result<bool> {
    Ok(db
        .find(&intent_id(&operation.id))?
        .is_some_and(|entry| matches_operation(&entry, operation, INTENT))
        && db.find(&checkpoint_id(&operation.id))?.is_none())
}

pub(crate) fn record(db: &Db<'_>, work: &Work, request: &Request, witness: &Witness) -> Result<()> {
    held(db, &work.op, &work.target)?;
    require(
        not_prepared(db, &work.op)?
            && witness.observation_digest == request.expected_observation_digest,
        "Fenced",
        "rename checkpoint does not match its approved file observation",
    )?;
    db.create(
        CHECKPOINT, &checkpoint_id(&work.op.id), &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
               "volume_ref":work.target.reference(),"data_incarnation":work.target.body["data_incarnation"],
               "source":request.source,"destination":request.destination,"witness":witness}),
    )?;
    Ok(())
}

pub(crate) fn pending(db: &Db<'_>, operation: &Doc) -> Result<(Doc, Doc)> {
    let task = db.of(field(&operation.spec, "task_id")?, "Task")?;
    require(
        operation.body["outcome_knowledge"] == "Unknown" && task.spec["task_type"] == TOOL,
        "PreconditionFailed",
        "operation is not an unresolved file rename",
    )?;
    let checkpoint = db
        .find(&checkpoint_id(&operation.id))?
        .ok_or_else(|| err("RecoveryRequired", "file rename checkpoint is unavailable"))?;
    let volume = db.of(
        field(&checkpoint.body["volume_ref"], "resource_id")?,
        "Volume",
    )?;
    require(
        matches_operation(&checkpoint, operation, CHECKPOINT)
            && checkpoint.body["volume_ref"] == volume.reference()
            && checkpoint.body["data_incarnation"] == volume.body["data_incarnation"],
        "RecoveryRequired",
        "file rename checkpoint no longer binds the original volume",
    )?;
    held(db, operation, &volume)?;
    Ok((volume, checkpoint))
}

fn field<'v>(value: &'v Value, key: &str) -> Result<&'v str> {
    value[key]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "file rename witness incomplete"))
}

fn paths(config: &Config, volume: &Doc, checkpoint: &Doc) -> Result<Paths> {
    Paths::open(
        config,
        volume,
        field(&checkpoint.body, "source")?,
        field(&checkpoint.body, "destination")?,
    )
}

pub(crate) fn inspect(work: &Work, check: &mut impl FnMut() -> Result<()>) -> Result<Value> {
    let checkpoint = work
        .publication_checkpoint
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "missing rename checkpoint"))?;
    let volume = work
        .volume
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "missing original volume"))?;
    let witness: Witness = decode(&checkpoint.body["witness"])?;
    let opened = paths(&work.cfg, volume, checkpoint)?;
    let (placement, expected) = opened.state(&witness)?;
    let (directory, name) = if placement == Placement::Original {
        &opened.source
    } else {
        &opened.destination
    };
    let (revision, content_digest) =
        file_integrity::capture(directory, name, file_integrity::MAX_BYTES, check)?;
    opened.sync()?;
    let (current, latest) = paths(&work.cfg, volume, checkpoint)?.state(&witness)?;
    require(
        current == placement
            && expected == revision
            && revision == latest
            && content_digest == witness.content_digest,
        "PreconditionFailed",
        "file contents or placement changed during rename inspection",
    )?;
    Ok(serde_json::to_value(Observation {
        checkpoint_ref: checkpoint.reference(),
        operation_ref: work.target.reference(),
        placement,
        revision,
        content_digest,
    })?)
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    operation: &Doc,
    volume: &Doc,
    proof: &Doc,
    observation: &Value,
) -> Result<(bool, Value, Value)> {
    let (original, checkpoint) = pending(db, operation)?;
    let observed: Observation = decode(observation)?;
    let witness: Witness = decode(&checkpoint.body["witness"])?;
    require(
        original.reference() == volume.reference()
            && observed.checkpoint_ref == checkpoint.reference()
            && observed.operation_ref == operation.reference()
            && observed.content_digest == witness.content_digest,
        "OutcomeUnknown",
        "inspection does not confirm the original file rename",
    )?;
    let (placement, revision) = paths(db.cfg, volume, &checkpoint)
        .and_then(|opened| opened.state(&witness))
        .map_err(unknown)?;
    require(
        placement == observed.placement && revision == observed.revision,
        "OutcomeUnknown",
        "rename inspection no longer matches the volume",
    )?;
    let applied = placement == Placement::Renamed;
    let result = if applied {
        json!({"volume_ref":db.status(volume, volume.body.clone())?.reference(),
               "receipt":receipt(field(&checkpoint.body,"source")?,field(&checkpoint.body,"destination")?,&witness,&revision)})
    } else {
        json!({"volume_ref":volume.reference(),"placement":"Original"})
    };
    Ok((
        applied,
        result,
        json!({"observation_ref":proof.reference(),
        "conclusion":if applied{"RenameConfirmed"}else{"OriginalConfirmed"}}),
    ))
}
