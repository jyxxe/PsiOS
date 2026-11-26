//! A witnessed rename separates retiring a volume entry from reclaiming its inode.
use crate::{
    Config, Error, Result, after,
    db::{self, Db, Doc},
    decode, err,
    host::Work,
    now, policy, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

mod fs;
mod verify;
pub(crate) use fs::Location;
use fs::{Entry, Paths, Placement};
pub(crate) use verify::{verify, verify_active};

pub(crate) const FORMAT: u32 = super::ROOT_MODE_FORMAT + 1;
pub(crate) const KIND: &str = "VolumeEntryRetirement";
pub(crate) const TOOL: &str = "volume.remove_entry";
const MAX_ENTRIES: i64 = 128;
const MAX_BYTES: u64 = crate::MAX_BACKUP_BYTES;
pub(crate) const SCHEMA: &str = "
CREATE TABLE volume_entry_retirements(
    operation TEXT PRIMARY KEY,
    volume TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK(bytes>=0),
    phase TEXT NOT NULL CHECK(phase IN ('Active','PendingCleanup')),
    eligible_at TEXT NOT NULL
) WITHOUT ROWID;
CREATE INDEX volume_entry_retirement_by_volume ON volume_entry_retirements(volume);
CREATE INDEX volume_entry_retirement_eligible ON volume_entry_retirements(phase,eligible_at,operation);
CREATE TABLE volume_entry_retirement_budget(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    entries INTEGER NOT NULL CHECK(entries>=0),
    bytes INTEGER NOT NULL CHECK(bytes>=0)
);
INSERT INTO volume_entry_retirement_budget VALUES(1,0,0);
";

fn id(operation: &str) -> String {
    format!("volume-entry-retirement-{operation}")
}

fn operation_name(operation: &str) -> Result<String> {
    super::filename(operation)?;
    Ok(format!("entry-{operation}"))
}

fn record(conn: &Connection, op: &Doc) -> Result<Option<Doc>> {
    let Some(retirement) = db::find(conn, &id(&op.id))? else {
        return Ok(None);
    };
    let effect = op.body["effect_refs"]
        .as_array()
        .and_then(|refs| refs.last());
    require(
        retirement.kind == KIND
            && retirement.owner == op.owner
            && retirement.spec["operation_id"] == op.id
            && retirement.spec["plan_ref"] == op.body["plan_ref"]
            && effect
                .is_some_and(|reference| reference["resource_id"] == retirement.spec["effect_id"]),
        "RecoveryRequired",
        "entry retirement record disagrees with the original approved effect",
    )?;
    Ok(Some(retirement))
}

fn held(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |row| row.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "entry retirement requires its original volume hold",
    )
}

pub(crate) fn begin(db: &Db<'_>, work: &Work, path: &str) -> Result<()> {
    held(db, &work.op, &work.target)?;
    require(
        db.c.execute(
            "UPDATE volume_entry_retirement_budget SET entries=entries+1
            WHERE singleton=1 AND entries<?1",
            [MAX_ENTRIES],
        )? == 1,
        "QuotaExceeded",
        "entry retirement capacity exhausted",
    )?;
    db.create(
        KIND, &id(&work.op.id), &work.op.owner,
        json!({"operation_id":work.op.id,"plan_ref":work.op.body["plan_ref"],"effect_id":work.effect.id,
            "volume_ref":work.target.reference(),"data_incarnation":work.target.body["data_incarnation"],"path":path}),
        json!({"lifecycle":"Preparing"}),
    )?;
    db.c.execute(
        "INSERT INTO volume_entry_retirements VALUES(?1,?2,0,'Active',?3)",
        params![work.op.id, work.target.id, now()],
    )?;
    Ok(())
}

pub(crate) fn not_prepared(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(record(db.c, op)?.is_some_and(|entry| entry.body["lifecycle"] == "Preparing"))
}

pub(crate) fn prepare(db: &Db<'_>, work: &Work, path: &str, location: &Location) -> Result<()> {
    held(db, &work.op, &work.target)?;
    location.check()?;
    let retirement = record(db.c, &work.op)?
        .ok_or_else(|| err("RecoveryRequired", "missing entry retirement intent"))?;
    require(
        retirement.body["lifecycle"] == "Preparing"
            && retirement.spec["volume_ref"] == work.target.reference()
            && retirement.spec["path"] == path,
        "Fenced",
        "entry retirement intent changed before the command",
    )?;
    let bytes = location.bytes() as i64;
    require(
        db.c.execute(
            "UPDATE volume_entry_retirement_budget SET bytes=bytes+?1
            WHERE singleton=1 AND bytes<=?2",
            params![bytes, MAX_BYTES as i64 - bytes],
        )? == 1,
        "QuotaExceeded",
        "entry retirement byte capacity exhausted",
    )?;
    require(
        db.c.execute(
            "UPDATE volume_entry_retirements SET bytes=?1
            WHERE operation=?2 AND volume=?3 AND phase='Active' AND bytes=0",
            params![bytes, work.op.id, work.target.id],
        )? == 1,
        "RecoveryRequired",
        "entry retirement reservation disappeared",
    )?;
    let op = db.of(&work.op.id, "Operation")?;
    let effect = db.of(&work.effect.id, "Effect")?;
    require(
        effect.body["phase"] == "PermitBound" && effect.reference() == work.effect.reference(),
        "Fenced",
        "entry retirement execution permit already used",
    )?;
    let mut started = effect.body.clone();
    started["phase"] = json!("EffectStarted");
    let started = db.status(&effect, started)?;
    let mut state = op.body.clone();
    let last = state["effect_refs"]
        .as_array_mut()
        .and_then(|refs| refs.last_mut())
        .ok_or_else(|| err("RecoveryRequired", "entry retirement has no active effect"))?;
    require(
        *last == effect.reference(),
        "Fenced",
        "entry retirement Operation effect changed",
    )?;
    *last = started.reference();
    db.status(&op, state)?;
    db.status(
        &retirement,
        json!({"lifecycle":"Prepared","location":location,"prepared_at":now(),
        "started_effect_ref":started.reference()}),
    )?;
    Ok(())
}

pub(crate) fn retire(
    work: &Work,
    path: &str,
    checkpoint: impl FnOnce(&Location) -> Result<()>,
) -> Result<Value> {
    let paths = Paths::open(&work.cfg, &work.target, path, &work.op.id, true)?;
    let location = paths.prepare()?;
    checkpoint(&location)?;
    let paths = Paths::open(&work.cfg, &work.target, path, &work.op.id, false)?;
    paths.retire(&location)?;
    paths.sync().map_err(super::unknown)?;
    let current =
        Paths::open(&work.cfg, &work.target, path, &work.op.id, false).map_err(super::unknown)?;
    let Placement::Retired(retired) = current.observe(&location).map_err(super::unknown)? else {
        return Err(err(
            "OutcomeUnknown",
            "entry retirement placement is unverified",
        ));
    };
    Ok(json!({"path":path,"retired_entry":retired}))
}

fn prepared(db: &Db<'_>, op: &Doc, volume: &Doc, path: &str) -> Result<(Doc, Location)> {
    held(db, op, volume)?;
    let entry = record(db.c, op)?.ok_or_else(|| {
        err(
            "UnsupportedCapability",
            "legacy entry removal has no witnessed retirement",
        )
    })?;
    require(
        entry.body["lifecycle"] == "Prepared"
            && entry.spec["path"] == path
            && entry.spec["volume_ref"] == volume.reference()
            && entry.spec["data_incarnation"] == volume.body["data_incarnation"],
        "RecoveryRequired",
        "entry retirement does not bind the original volume and pathname",
    )?;
    let location: Location = decode(&entry.body["location"])?;
    location.check()?;
    Ok((entry, location))
}

fn queue_cleanup(
    db: &Db<'_>,
    entry: &Doc,
    volume: &Doc,
    path: &str,
    retired: &Entry,
) -> Result<Value> {
    let updated = db.status(volume, volume.body.clone())?;
    let mut state = entry.body.clone();
    state["lifecycle"] = json!("PendingCleanup");
    state["result_volume_ref"] = updated.reference();
    state["retired_entry"] = json!(retired);
    state["retired_at"] = json!(now());
    db.status(entry, state)?;
    require(
        db.c.execute(
            "UPDATE volume_entry_retirements SET phase='PendingCleanup',eligible_at=?1
            WHERE operation=?2 AND phase='Active'",
            params![now(), entry.spec["operation_id"].as_str()],
        )? == 1,
        "RecoveryRequired",
        "missing entry retirement disposal reservation",
    )?;
    Ok(json!({"volume_ref":updated.reference(),"receipt":{"path":path}}))
}

pub(crate) fn settle(db: &Db<'_>, work: &Work, path: &str, receipt: &Value) -> Result<Value> {
    require(
        receipt["path"] == path,
        "RecoveryRequired",
        "unexpected entry retirement receipt",
    )?;
    let (entry, location) = prepared(db, &work.op, &work.target, path)?;
    let retired: Entry = decode(&receipt["retired_entry"])?;
    let paths = Paths::open(db.cfg, &work.target, path, &work.op.id, false).map_err(uncertain)?;
    let Placement::Retired(observed) = paths.observe(&location).map_err(uncertain)? else {
        return Err(err(
            "OutcomeUnknown",
            "entry retirement changed before receipt publication",
        ));
    };
    require(
        observed == retired,
        "OutcomeUnknown",
        "entry retirement changed before receipt publication",
    )?;
    queue_cleanup(db, &entry, &work.target, path, &retired)
}

pub(crate) struct Probe {
    cfg: Config,
    volume: Doc,
    operation: String,
    path: String,
    record: Doc,
    location: Location,
}

pub(crate) struct Sample(Probe, Placement);

pub(crate) fn prepare_reconcile(db: &Db<'_>, op: &Doc, volume: Doc, path: &str) -> Result<Probe> {
    let (record, location) = prepared(db, op, &volume, path)?;
    Ok(Probe {
        cfg: db.cfg.clone(),
        volume,
        operation: op.id.clone(),
        path: path.to_owned(),
        record,
        location,
    })
}

fn uncertain(_: Error) -> Error {
    err("OutcomeUnknown", "entry retirement placement is unverified")
}

impl Probe {
    pub(crate) fn read(self) -> Result<Sample> {
        let paths = Paths::open(&self.cfg, &self.volume, &self.path, &self.operation, false)
            .map_err(uncertain)?;
        let placement = paths.observe(&self.location).map_err(uncertain)?;
        paths.sync().map_err(super::unknown)?;
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
    let (entry, location) = prepared(db, op, volume, path)?;
    require(
        probe.record.reference() == entry.reference()
            && probe.operation == op.id
            && probe.volume.reference() == volume.reference()
            && probe.path == path,
        "OutcomeUnknown",
        "entry retirement observation is stale",
    )?;
    let current = Paths::open(db.cfg, volume, path, &op.id, false).map_err(uncertain)?;
    require(
        current.observe(&location).map_err(uncertain)? == placement,
        "OutcomeUnknown",
        "entry retirement placement changed after inspection",
    )?;
    let (completed, receipt, placement, conclusion) = match placement {
        Placement::Retired(retired) => (
            true,
            queue_cleanup(db, &entry, volume, path, &retired)?,
            "Retired",
            "RetirementConfirmed",
        ),
        Placement::Original => (
            false,
            json!({"path":path,"removed":false}),
            "Original",
            "RetirementNotApplied",
        ),
    };
    Ok((
        completed,
        receipt,
        json!({"checkpoint_ref":entry.reference(),"placement":placement,"conclusion":conclusion}),
    ))
}

pub(crate) fn abandon(db: &Db<'_>, op: &Doc) -> Result<()> {
    let Some(entry) = record(db.c, op)? else {
        return Ok(());
    };
    if matches!(
        entry.body["lifecycle"].as_str(),
        Some("Preparing" | "Prepared")
    ) {
        let mut state = entry.body.clone();
        state["lifecycle"] = json!("NotApplied");
        state["settled_at"] = json!(now());
        db.status(&entry, state)?;
        release(db, &op.id, "Active")?;
    }
    Ok(())
}

fn release(db: &Db<'_>, operation: &str, phase: &str) -> Result<()> {
    let bytes: i64 =
        db.c.query_row(
            "DELETE FROM volume_entry_retirements WHERE operation=?1 AND phase=?2 RETURNING bytes",
            params![operation, phase],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            err(
                "RecoveryRequired",
                "entry retirement reservation is missing",
            )
        })?;
    require(
        db.c.execute(
            "UPDATE volume_entry_retirement_budget SET entries=entries-1,bytes=bytes-?1
        WHERE singleton=1 AND entries>0 AND bytes>=?1",
            [bytes],
        )? == 1,
        "RecoveryRequired",
        "entry retirement reservation contradicts its budget",
    )
}

pub(super) fn guard_volume_retirement(wrapper: &std::fs::File) -> Result<()> {
    fs::guard_volume_retirement(wrapper)
}

pub(crate) fn ensure_no_pending(db: &Db<'_>, volume: &Doc) -> Result<()> {
    let active: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM volume_entry_retirements WHERE volume=?1)",
        [&volume.id],
        |row| row.get(0),
    )?;
    require(
        !active,
        "Conflict",
        "volume retains an unresolved or unreclaimed entry retirement",
    )
}

pub(crate) fn settlement_ref(db: &Db<'_>, op: &Doc) -> Result<Value> {
    let record = record(db.c, op)?
        .ok_or_else(|| err("RecoveryRequired", "entry settlement evidence is missing"))?;
    require(
        matches!(
            record.body["lifecycle"].as_str(),
            Some("PendingCleanup" | "NotApplied")
        ),
        "RecoveryRequired",
        "entry effects require exact durable settlement evidence",
    )?;
    Ok(record.reference())
}

pub(crate) struct Disposal {
    record: Doc,
    volume: Doc,
    operation: String,
    location: Location,
    retired: Entry,
}

pub(crate) fn next_disposal(db: &Db<'_>) -> Result<Option<Disposal>> {
    let operation: Option<String> = db.c.query_row(
        "SELECT operation FROM volume_entry_retirements WHERE phase='PendingCleanup' AND eligible_at<=?1
        ORDER BY eligible_at,operation LIMIT 1", [now()], |row| row.get(0),
    ).optional()?;
    let Some(operation) = operation else {
        return Ok(None);
    };
    let op = db.of(&operation, "Operation")?;
    let record = record(db.c, &op)?
        .ok_or_else(|| err("RecoveryRequired", "entry disposal evidence is missing"))?;
    let (_, volume) = verify::binding(db.c, &record)?;
    require(
        record.body["lifecycle"] == "PendingCleanup" && volume.body["lifecycle"] == "Available",
        "RecoveryRequired",
        "entry disposal has no committed logical retirement",
    )?;
    let location: Location = decode(&record.body["location"])?;
    location.check()?;
    let retired: Entry = decode(&record.body["retired_entry"])?;
    location.check_retired(&retired)?;
    Ok(Some(Disposal {
        record,
        volume,
        operation,
        location,
        retired,
    }))
}

impl Disposal {
    pub(crate) fn run(&self, cfg: &Config) -> Result<bool> {
        fs::purge(
            cfg,
            &self.volume,
            &self.operation,
            &self.location,
            &self.retired,
        )
    }

    pub(crate) fn finish(self, db: &Db<'_>, result: Result<bool>) -> Result<()> {
        require(
            db.of(&self.record.id, KIND)?.reference() == self.record.reference(),
            "Conflict",
            "entry disposal custody changed",
        )?;
        match result {
            Ok(removed) => {
                let mut state = self.record.body.clone();
                state["lifecycle"] = json!("Reclaimed");
                state["reclaimed_at"] = json!(now());
                db.status(&self.record, state)?;
                release(db, &self.operation, "PendingCleanup")?;
                db.audit(
                    policy::SYSTEM,
                    "Storage.VolumeEntryReclaim",
                    Some(&self.operation),
                    if removed {
                        "Removed"
                    } else {
                        "ConfirmedAbsent"
                    },
                )?;
            }
            Err(error) => {
                db.c.execute("UPDATE volume_entry_retirements SET eligible_at=?1 WHERE operation=?2 AND phase='PendingCleanup'",
                    params![after(5), self.operation])?;
                eprintln!("psiosd volume entry reclamation: {}", error.0.code);
            }
        }
        Ok(())
    }
}
