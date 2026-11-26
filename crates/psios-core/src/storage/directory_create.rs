//! Recover an approved mkdir by authenticating its original empty directory inode.
use crate::{
    Config, Error, Result, after,
    db::{self, Db, Doc},
    decode, err,
    host::Work,
    now, policy, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

mod filesystem;
mod verification;
pub(crate) use filesystem::{Location, Parents};
use filesystem::{Paths, Placement};
pub(crate) use verification::{verify, verify_active};

pub(crate) const FORMAT: u32 = crate::schedule::coalescence::FORMAT + 1;
pub(crate) const KIND: &str = "VolumeDirectoryCreation";
pub(crate) const TOOL: &str = "volume.mkdir";
const LIMIT: i64 = 128;
const EPOCH: &str = "volume_mkdir_custody_epoch";
pub(crate) const SCHEMA: &str = "
CREATE TABLE volume_mkdir_stages(
    operation TEXT PRIMARY KEY, volume TEXT NOT NULL,
    phase TEXT NOT NULL CHECK(phase IN ('Active','Cleanup')),
    eligible_at TEXT NOT NULL
) STRICT;
CREATE INDEX volume_mkdir_stages_volume ON volume_mkdir_stages(volume);
CREATE INDEX volume_mkdir_stages_cleanup ON volume_mkdir_stages(operation,eligible_at) WHERE phase='Cleanup';
CREATE INDEX volume_mkdir_unsettled ON versions(id,rev) WHERE kind='VolumeDirectoryCreation'
    AND body->>'lifecycle' IN ('Preparing','Allocating','Prepared','PendingCleanup');
INSERT INTO meta(key,value)
    SELECT 'volume_mkdir_custody_epoch', CAST(COALESCE(MAX(seq),0) AS TEXT) FROM versions;
";

fn id(operation: &str) -> String {
    format!("volume-directory-creation-{operation}")
}

fn record(conn: &Connection, op: &Doc) -> Result<Option<Doc>> {
    let Some(record) = db::find(conn, &id(&op.id))? else {
        return Ok(None);
    };
    require(
        record.kind == KIND
            && record.owner == op.owner
            && record.spec["operation_id"] == op.id
            && record.spec["plan_ref"] == op.body["plan_ref"]
            && record.spec["effect_id"] == op.body["effect_refs"][0]["resource_id"],
        "RecoveryRequired",
        "directory creation is not bound to its original Operation effect",
    )?;
    Ok(Some(record))
}

fn required(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    record(db.c, op)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "original directory creation has no witnessed intent",
        )
    })
}

fn held(conn: &Connection, op: &Doc, volume: &Doc) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |row| row.get(0),
    )?)
}

fn require_hold(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<()> {
    require(
        held(db.c, op, volume)? && db.of(&volume.id, "Volume")?.reference() == volume.reference(),
        "RecoveryRequired",
        "directory creation lost its exact original Volume hold",
    )
}

pub(crate) fn ensure_no_pending(db: &Db<'_>, volume: &Doc) -> Result<()> {
    let exists = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume_mkdir_stages WHERE volume=?1)",
        [&volume.id],
        |row| row.get::<_, bool>(0),
    )?;
    require(
        !exists,
        "Conflict",
        "volume retains an unresolved or unreclaimed directory stage",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work, path: &str) -> Result<()> {
    require_hold(db, &work.op, &work.target)?;
    let outstanding: i64 =
        db.c.query_row("SELECT count(*) FROM volume_mkdir_stages", [], |row| {
            row.get(0)
        })?;
    require(
        outstanding < LIMIT,
        "QuotaExceeded",
        "directory creation stage limit reached",
    )?;
    db.create(
        KIND,
        &id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id,"volume_ref":work.target.reference(),
            "path":path,"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id}),
        json!({"lifecycle":"Preparing"}),
    )?;
    db.c.execute(
        "INSERT INTO volume_mkdir_stages VALUES(?1,?2,'Active',?3)",
        params![work.op.id, work.target.id, now()],
    )?;
    Ok(())
}

pub(crate) fn not_prepared(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(record(db.c, op)?.is_some_and(|record| {
        matches!(
            record.body["lifecycle"].as_str(),
            Some("Preparing" | "Allocating")
        )
    }) && db.resolve(&op.body["effect_refs"][0])?.body["phase"] == "PermitBound")
}

pub(crate) fn reserve_stage(db: &Db<'_>, work: &Work, parents: &Parents) -> Result<()> {
    require_hold(db, &work.op, &work.target)?;
    parents.check(&work.target)?;
    let record = required(db, &work.op)?;
    require(
        record.spec["volume_ref"] == work.target.reference()
            && record.body["lifecycle"] == "Preparing"
            && db.of(&work.effect.id, "Effect")?.reference() == work.effect.reference(),
        "Fenced",
        "directory creation staging reservation was already used or replaced",
    )?;
    db.status(&record, json!({"lifecycle":"Allocating","parents":parents}))?;
    Ok(())
}

pub(crate) fn prepare(db: &Db<'_>, work: &Work, location: &Location) -> Result<()> {
    require_hold(db, &work.op, &work.target)?;
    location.check(&work.target)?;
    let record = required(db, &work.op)?;
    require(
        record.spec["volume_ref"] == work.target.reference()
            && record.body["lifecycle"] == "Allocating"
            && record.body["parents"] == serde_json::to_value(&location.parents)?,
        "Fenced",
        "directory creation stage did not use its approved parents",
    )?;
    let effect = work.start_host_effect(db)?;
    db.status(
        &record,
        json!({"lifecycle":"Prepared","location":location,"started_effect_ref":effect.reference()}),
    )?;
    Ok(())
}

pub(crate) fn publish(
    work: &Work,
    path: &str,
    mut checkpoint: impl FnMut(crate::host::Checkpoint<'_>) -> Result<()>,
) -> Result<Value> {
    use crate::host::Checkpoint;
    let volume_root = super::file_integrity::volume_root(&work.cfg, &work.target)?;
    work.cfg
        .check_managed_space(&volume_root, std::iter::empty(), 1)?;
    let paths = Paths::open(&work.cfg, &work.target, path, &work.op.id, true)?;
    let parents = paths.reserve(&work.cfg)?;
    checkpoint(Checkpoint::DirectoryAllocation(&parents))?;
    let location = paths.allocate(parents)?;
    checkpoint(Checkpoint::DirectoryCreation(&location))?;
    let paths =
        Paths::open(&work.cfg, &work.target, path, &work.op.id, false).map_err(super::unknown)?;
    paths.publish(&location).map_err(uncertain)?;
    paths
        .sync(&location, Placement::Published)
        .map_err(super::unknown)?;
    let latest =
        Paths::open(&work.cfg, &work.target, path, &work.op.id, false).map_err(super::unknown)?;
    require(
        latest.observe(&location).map_err(uncertain)? == Placement::Published,
        "OutcomeUnknown",
        "directory creation postcondition is not verified",
    )?;
    Ok(json!({"path":path}))
}

fn prepared(db: &Db<'_>, op: &Doc, volume: &Doc, path: &str) -> Result<(Doc, Location)> {
    require_hold(db, op, volume)?;
    let record = required(db, op)?;
    require(
        record.body["lifecycle"] == "Prepared"
            && record.spec["path"] == path
            && record.spec["volume_ref"] == volume.reference(),
        "RecoveryRequired",
        "directory creation has no exact publication checkpoint",
    )?;
    let location: Location = decode(&record.body["location"])?;
    location.check(volume)?;
    Ok((record, location))
}

fn uncertain(_: Error) -> Error {
    err(
        "OutcomeUnknown",
        "the original directory creation placement cannot be verified",
    )
}

fn commit_publication(db: &Db<'_>, record: &Doc, volume: &Doc, path: &str) -> Result<Value> {
    let operation = verification::field(&record.spec, "operation_id")?;
    let updated = db.status(volume, volume.body.clone())?;
    let mut body = record.body.clone();
    body["lifecycle"] = json!("Published");
    body["result_volume_ref"] = updated.reference();
    db.status(record, body)?;
    release(db, operation, "Active")?;
    Ok(json!({"volume_ref":updated.reference(),"receipt":{"path":path}}))
}

pub(crate) fn settle(db: &Db<'_>, work: &Work, path: &str, receipt: &Value) -> Result<Value> {
    let op = db.of(&work.op.id, "Operation")?;
    let (record, location) = prepared(db, &op, &work.target, path)?;
    let paths = Paths::open(db.cfg, &work.target, path, &op.id, false).map_err(uncertain)?;
    require(
        *receipt == json!({"path":path})
            && paths.observe(&location).map_err(uncertain)? == Placement::Published,
        "OutcomeUnknown",
        "directory creation receipt does not match the newly published directory",
    )?;
    commit_publication(db, &record, &work.target, path)
}

pub(crate) struct Probe {
    config: Config,
    volume: Doc,
    operation: Value,
    operation_id: String,
    path: String,
    record: Doc,
    location: Location,
}

pub(crate) struct Sample(Probe, Placement);

pub(crate) fn prepare_reconcile(db: &Db<'_>, op: &Doc, volume: Doc, path: &str) -> Result<Probe> {
    let (record, location) = prepared(db, op, &volume, path)?;
    Ok(Probe {
        config: db.cfg.clone(),
        volume,
        operation: op.reference(),
        operation_id: op.id.clone(),
        path: path.into(),
        record,
        location,
    })
}

impl Probe {
    pub(crate) fn read(self) -> Result<Sample> {
        let paths = Paths::open(
            &self.config,
            &self.volume,
            &self.path,
            &self.operation_id,
            false,
        )
        .map_err(uncertain)?;
        let placement = paths.observe(&self.location).map_err(uncertain)?;
        paths
            .sync(&self.location, placement)
            .map_err(super::unknown)?;
        Ok(Sample(self, placement))
    }
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    volume: &Doc,
    path: &str,
    Sample(probe, placement): Sample,
) -> Result<(bool, Value, Value)> {
    let (record, location) = prepared(db, op, volume, path)?;
    let paths = Paths::open(db.cfg, volume, path, &op.id, false).map_err(uncertain)?;
    require(
        op.reference() == probe.operation
            && volume.reference() == probe.volume.reference()
            && record.reference() == probe.record.reference()
            && path == probe.path
            && paths.observe(&location).map_err(uncertain)? == placement,
        "OutcomeUnknown",
        "original directory placement changed after inspection",
    )?;
    let (applied, result, conclusion) = match placement {
        Placement::Published => (
            true,
            commit_publication(db, &record, volume, path)?,
            "DirectoryCreationConfirmed",
        ),
        Placement::Staged => {
            queue_cleanup(db, &record, &op.id)?;
            (
                false,
                json!({"path":path,"created":false}),
                "DirectoryCreationNotApplied",
            )
        }
    };
    Ok((
        applied,
        result,
        json!({"checkpoint_ref":record.reference(),"conclusion":conclusion}),
    ))
}

fn queue_cleanup(db: &Db<'_>, record: &Doc, operation: &str) -> Result<()> {
    let mut body = record.body.clone();
    body["lifecycle"] = json!("PendingCleanup");
    db.status(record, body)?;
    require(db.c.execute(
        "UPDATE volume_mkdir_stages SET phase='Cleanup',eligible_at=?1 WHERE operation=?2 AND phase='Active'",
        params![now(), operation],
    )? == 1, "RecoveryRequired", "directory creation lost its cleanup reservation")
}

fn release(db: &Db<'_>, operation: &str, phase: &str) -> Result<()> {
    require(
        db.c.execute(
            "DELETE FROM volume_mkdir_stages WHERE operation=?1 AND phase=?2",
            params![operation, phase],
        )? == 1,
        "RecoveryRequired",
        "directory creation lost its stage reservation",
    )
}

pub(crate) fn abandon(db: &Db<'_>, op: &Doc) -> Result<()> {
    let Some(record) = record(db.c, op)? else {
        return Ok(());
    };
    match record.body["lifecycle"].as_str() {
        Some("Preparing") => {
            db.status(&record, json!({"lifecycle":"NotStarted"}))?;
            release(db, &op.id, "Active")
        }
        Some("Allocating") => queue_cleanup(db, &record, &op.id),
        Some("PendingCleanup" | "Published") => Ok(()),
        _ => Err(err(
            "RecoveryRequired",
            "cannot abandon a checkpointed directory creation",
        )),
    }
}

pub(crate) fn settlement_ref(db: &Db<'_>, op: &Doc) -> Result<Value> {
    let record = required(db, op)?;
    require(
        matches!(
            record.body["lifecycle"].as_str(),
            Some("Published" | "PendingCleanup")
        ),
        "RecoveryRequired",
        "directory creation must settle with exact publication custody",
    )?;
    Ok(record.reference())
}

pub(crate) struct Disposal {
    record: Doc,
    volume: Doc,
    operation: String,
}

pub(crate) fn next_disposal(db: &Db<'_>, cursor: &mut String) -> Result<Option<Disposal>> {
    let id: Option<String> = db.c.query_row(
        "SELECT operation FROM volume_mkdir_stages WHERE phase='Cleanup' AND operation>?1 AND eligible_at<=?2 ORDER BY operation LIMIT 1",
        params![cursor.as_str(), now()], |row| row.get(0),
    ).optional()?;
    let Some(id) = id else {
        cursor.clear();
        return Ok(None);
    };
    cursor.clone_from(&id);
    let op = db.of(&id, "Operation")?;
    let record = required(db, &op)?;
    let volume = verification::disposal_volume(db.c, &record)?;
    Ok(Some(Disposal {
        record,
        volume,
        operation: id,
    }))
}

impl Disposal {
    pub(crate) fn run(&self, config: &Config) -> Result<bool> {
        if let Some(location) = self.record.body.get("location") {
            let location: Location = decode(location)?;
            filesystem::dispose(
                config,
                &self.volume,
                &self.operation,
                &location.parents,
                Some(&location.entry),
            )
        } else {
            let parents: Parents = decode(&self.record.body["parents"])?;
            filesystem::dispose(config, &self.volume, &self.operation, &parents, None)
        }
    }

    pub(crate) fn finish(self, db: &Db<'_>, result: Result<bool>) -> Result<()> {
        require(
            db.of(&self.record.id, KIND)?.reference() == self.record.reference(),
            "Conflict",
            "directory disposal custody changed",
        )?;
        match result {
            Ok(removed) => {
                let observation = if removed {
                    "Removed"
                } else {
                    "ConfirmedAbsent"
                };
                let mut body = self.record.body.clone();
                body["lifecycle"] = json!("Reclaimed");
                body["disposal"] = json!(observation);
                db.status(&self.record, body)?;
                release(db, &self.operation, "Cleanup")?;
                db.audit(
                    policy::SYSTEM,
                    "Storage.VolumeDirectoryStageReclaim",
                    Some(&self.operation),
                    observation,
                )?;
            }
            Err(error) => {
                db.c.execute("UPDATE volume_mkdir_stages SET eligible_at=?1 WHERE operation=?2 AND phase='Cleanup'", params![after(5),self.operation])?;
                eprintln!(
                    "psiosd directory creation stage reclamation: {}",
                    error.0.code
                );
            }
        }
        Ok(())
    }
}
