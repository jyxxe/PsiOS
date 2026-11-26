//! Witnessed ownership of named backup stages and their deferred reclamation.
use super::placement::present;
use super::{
    CHECKPOINT, Doc, INTENT, MAX_ENTRIES, MemberSet, NAMED_STAGE, NOT_APPLIED, Placement, Result,
    Value, backup_id, belongs, check_receipt, checkpoint_id, err, filename, intent_id, json,
    member, member_digest, member_name, members, params, require,
};
use crate::{
    Config,
    db::{Db, find, resolve},
};
use rusqlite::{Connection, OptionalExtension};
use rustix::fs::{AtFlags, RenameFlags, renameat_with, unlinkat};
use std::collections::HashSet;

pub(crate) const DISPOSAL_SCHEMA: &str = include_str!("custody.sql");
const MAX_STAGES: usize = 128;
const AUDIT: &str = "Storage.BackupStageReclaim";

struct Indexed {
    name: String,
    checkpoint_ref: Value,
    evidence_ref: Option<Value>,
}

fn indexed(conn: &Connection, operation: &str) -> Result<Option<Indexed>> {
    Ok(conn.query_row(
        "SELECT stage_name,checkpoint_ref,evidence_ref FROM backup_stage_custody WHERE operation=?1",
        [operation],
        |row| Ok(Indexed { name: row.get(0)?, checkpoint_ref: row.get(1)?, evidence_ref: row.get(2)? }),
    ).optional()?)
}

fn epoch(conn: &Connection) -> Result<i64> {
    let epoch: String = conn.query_row(
        "SELECT value FROM meta WHERE key='backup_custody_epoch'",
        [],
        |row| row.get(0),
    )?;
    epoch
        .parse()
        .ok()
        .filter(|epoch| *epoch >= 0)
        .ok_or_else(|| err("RecoveryRequired", "backup stage custody epoch is invalid"))
}

fn needed(conn: &Connection, id: &str) -> Result<Doc> {
    find(conn, id)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "backup stage custody resource is missing",
        )
    })
}

pub(super) fn reserve(
    db: &Db<'_>,
    op: &Doc,
    checkpoint: &Doc,
    placement: &Placement,
) -> Result<()> {
    let count: usize =
        db.c.query_row("SELECT count(*) FROM backup_stage_custody", [], |row| {
            row.get(0)
        })?;
    require(
        count < MAX_STAGES,
        "QuotaExceeded",
        "backup stage custody limit reached",
    )?;
    db.c.execute(
        "INSERT INTO backup_stage_custody VALUES(?1,?2,?3,NULL)",
        params![op.id, placement.stage_name, checkpoint.reference()],
    )?;
    Ok(())
}

pub(crate) fn checkpointed(db: &Db<'_>, op: &Doc) -> Result<bool> {
    Ok(indexed(db.c, &op.id)?.is_some())
}

fn named(conn: &Connection, op: &Doc, cutover: i64) -> Result<bool> {
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    let sequence: i64 = conn.query_row(
        "SELECT seq FROM versions WHERE id=?1 AND rev=1",
        [&effect.id],
        |row| row.get(0),
    )?;
    Ok(sequence > cutover)
}

fn source(conn: &Connection, op: &Doc) -> Result<Doc> {
    let plan = resolve(conn, &op.body["plan_ref"])?;
    let original = resolve(conn, &plan.body["allowed_effects"][0]["resource_ref"])?;
    let effect = resolve(conn, &op.body["effect_refs"][0])?;
    require(
        op.kind == "Operation"
            && plan.kind == "OperationPlan"
            && plan.owner == op.owner
            && original.kind == "Volume"
            && effect.kind == "Effect"
            && effect.owner == op.owner
            && effect.body["operation_id"] == op.id
            && effect.body["action"] == "backup.create"
            && effect.body["resource_ref"] == original.reference(),
        "RecoveryRequired",
        "backup stage custody disagrees with its originating effect and volume",
    )?;
    Ok(original)
}

fn bind(conn: &Connection, op: &Doc, checkpoint: &Doc, cutover: i64) -> Result<Option<Placement>> {
    let original = source(conn, op)?;
    let placement = Placement::from_checkpoint(checkpoint)?;
    require(
        checkpoint.id == checkpoint_id(&op.id)
            && checkpoint.rev == 1
            && belongs(checkpoint, op, CHECKPOINT)
            && checkpoint.body["source_ref"] == original.reference()
            && checkpoint.body["data_incarnation"] == original.body["data_incarnation"]
            && placement.is_some() == named(conn, op, cutover)?,
        "RecoveryRequired",
        "backup publication checkpoint disagrees with its original custody",
    )?;
    check_receipt(op, &checkpoint.body["receipt"])?;
    if placement.is_some() {
        let intent = needed(conn, &intent_id(&op.id))?;
        let issued = resolve(conn, &checkpoint.body["intent_ref"])?;
        let mut issued_body = issued.body.clone();
        issued_body["checkpoint_ref"] = checkpoint.reference();
        require(
            intent.rev == 2
                && belongs(&intent, op, INTENT)
                && belongs(&issued, op, INTENT)
                && issued.rev == 1
                && issued.id == intent.id
                && issued.body["protocol"] == NAMED_STAGE
                && issued.body.get("checkpoint_ref").is_none()
                && issued_body == intent.body,
            "RecoveryRequired",
            "backup stage checkpoint has no reciprocal witnessed intent",
        )?;
    } else {
        require(
            checkpoint.body.get("intent_ref").is_none(),
            "RecoveryRequired",
            "unnamed backup checkpoint has a named intent reference",
        )?;
    }
    Ok(placement)
}

pub(super) fn check_active_checkpoint(conn: &Connection, op: &Doc, checkpoint: &Doc) -> Result<()> {
    let placement = bind(conn, op, checkpoint, epoch(conn)?)?;
    let indexed = indexed(conn, &op.id)?;
    match placement {
        Some(place) => require(
            indexed.is_some_and(|row| {
                row.name == place.stage_name
                    && row.checkpoint_ref == checkpoint.reference()
                    && row.evidence_ref.is_none()
            }),
            "RecoveryRequired",
            "active backup checkpoint is missing its exact stage custody",
        ),
        None => require(
            indexed.is_none(),
            "RecoveryRequired",
            "legacy backup has unexpected stage custody",
        ),
    }
}

pub(super) fn release_published(db: &Db<'_>, op: &Doc, checkpoint: &Doc) -> Result<()> {
    require(db.c.execute(
        "DELETE FROM backup_stage_custody WHERE operation=?1 AND checkpoint_ref=?2 AND evidence_ref IS NULL",
        params![op.id,checkpoint.reference()],
    )? == 1, "RecoveryRequired", "published backup lost its original stage custody")
}

pub(crate) fn queue_disposal(db: &Db<'_>, op: &Doc, evidence: &Doc) -> Result<()> {
    if evidence.body["evidence"]["conclusion"] != NOT_APPLIED {
        return Ok(());
    }
    require(db.c.execute(
        "UPDATE backup_stage_custody SET evidence_ref=?2 WHERE operation=?1 AND checkpoint_ref=?3 AND evidence_ref IS NULL",
        params![op.id,evidence.reference(),evidence.body["evidence"]["checkpoint_ref"]],
    )? == 1, "RecoveryRequired", "negative backup proof lost its original stage custody")
}

fn verify_active(conn: &Connection, operation: &str, cutover: i64) -> Result<()> {
    let op = needed(conn, operation)?;
    let id = op.spec["task_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "backup operation has no original Task"))?;
    let task = needed(conn, id)?;
    let original = source(conn, &op)?;
    let queue: Option<(String, String)> = conn
        .query_row(
            "SELECT operation,phase FROM queue WHERE task=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let held: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![original.id, operation],
        |row| row.get(0),
    )?;
    require(
        held && task.kind == "Task"
            && task.owner == op.owner
            && task.spec["task_type"] == "backup.create"
            && task.body["operation_refs"][0] == op.reference()
            && task.body["outcome_knowledge"] == op.body["outcome_knowledge"]
            && needed(conn, &original.id)?.reference() == original.reference()
            && queue.is_some_and(|(id, phase)| {
                id == operation
                    && matches!(
                        (phase.as_str(), op.body["outcome_knowledge"].as_str()),
                        ("Dispatched", Some("InFlight")) | ("Unknown", Some("Unknown"))
                    )
            }),
        "RecoveryRequired",
        "active backup queue and source volume custody disagree",
    )?;
    if let Some(checkpoint) = find(conn, &checkpoint_id(operation))? {
        return check_active_checkpoint(conn, &op, &checkpoint);
    }
    let intent = find(conn, &intent_id(operation))?;
    let stage = indexed(conn, operation)?;
    require(
        stage.is_none()
            && (!named(conn, &op, cutover)?
                || intent.is_some_and(|intent| {
                    belongs(&intent, &op, INTENT)
                        && intent.body["protocol"] == NAMED_STAGE
                        && intent.rev == 1
                        && intent.body.get("checkpoint_ref").is_none()
                })),
        "RecoveryRequired",
        "active backup is missing its witnessed publication checkpoint",
    )
}

fn finished(conn: &Connection, operation: &str) -> Result<bool> {
    let audit: Option<(String, String)> = conn.query_row(
        "SELECT actor,decision FROM audit WHERE method='Storage.BackupStageReclaim' AND resource=?1",
        [operation], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    if let Some((actor, decision)) = audit {
        require(
            actor == crate::policy::SYSTEM
                && matches!(decision.as_str(), "Removed" | "ConfirmedAbsent"),
            "RecoveryRequired",
            "backup stage reclamation record is invalid",
        )?;
        return Ok(true);
    }
    Ok(false)
}

// Nothing below follows a mutable catalog or holds the source volume.
pub(crate) struct Disposal {
    operation: String,
    checkpoint: Doc,
    evidence: Doc,
    placement: Placement,
    members: MemberSet,
}

impl Disposal {
    pub(crate) fn open(db: &Db<'_>, operation: &str) -> Result<Self> {
        Self::indexed(db.c, operation)
    }

    fn indexed(conn: &Connection, operation: &str) -> Result<Self> {
        let row = indexed(conn, operation)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "backup stage reclamation custody is missing",
            )
        })?;
        let evidence_ref = row.evidence_ref.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "backup stage reclamation has no settled evidence",
            )
        })?;
        let evidence = resolve(conn, &evidence_ref)?;
        let job = Self::authenticate(conn, operation, evidence)?;
        require(
            row.name == job.placement.stage_name
                && row.checkpoint_ref == job.checkpoint.reference()
                && !finished(conn, operation)?,
            "RecoveryRequired",
            "backup stage reclamation index disagrees with its exact proof",
        )?;
        Ok(job)
    }

    fn authenticate(conn: &Connection, operation: &str, evidence: Doc) -> Result<Self> {
        let op = needed(conn, operation)?;
        let details = &evidence.body["evidence"];
        let checkpoint = resolve(conn, &details["checkpoint_ref"])?;
        let placement = bind(conn, &op, &checkpoint, epoch(conn)?)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "legacy backup cannot authorize stage reclamation",
            )
        })?;
        let prior = resolve(conn, &evidence.body["operation_ref"])?;
        let effect = resolve(conn, &op.body["effect_refs"][0])?;
        let observation = resolve(conn, &details["observation_ref"])?;
        require(
            details["members"]
                .as_object()
                .is_some_and(|entries| entries.len() <= MAX_ENTRIES + 1),
            "RecoveryRequired",
            "backup stage reclamation lacks a bounded member seal",
        )?;
        let members: MemberSet = crate::decode(&details["members"])?;
        require(
            op.body["outcome_knowledge"] == "Known"
                && matches!(op.body["lifecycle"].as_str(), Some("Failed" | "Cancelled"))
                && evidence.kind == "RecoveryEvidence"
                && evidence.rev == 1
                && evidence.owner == op.owner
                && details["conclusion"] == NOT_APPLIED
                && details["file_set_digest"] == placement.file_set_digest
                && member_digest(&members) == placement.file_set_digest
                && members.contains_key("manifest.json")
                && members.keys().all(|name| member_name(name))
                && evidence.body["target_ref"] == checkpoint.body["source_ref"]
                && prior.id == operation
                && prior.kind == "Operation"
                && prior.spec == op.spec
                && prior.owner == op.owner
                && prior.rev < op.rev
                && prior.body["outcome_knowledge"] == "Unknown"
                && prior.body["plan_ref"] == op.body["plan_ref"]
                && prior.body["effect_refs"][0]["resource_id"] == effect.id
                && effect.body["phase"] == "EffectReconciled"
                && effect.body["evidence_refs"]
                    .as_array()
                    .is_some_and(|refs| refs.contains(&evidence.reference()))
                && observation.kind == "Artifact",
            "RecoveryRequired",
            "backup stage reclamation has no settled original NotApplied proof",
        )?;
        Ok(Self {
            operation: operation.into(),
            checkpoint,
            evidence,
            placement,
            members,
        })
    }

    pub(crate) fn run(&self, config: &Config) -> Result<bool> {
        let parent = self.placement.parent(config)?;
        let final_name = backup_id(&self.checkpoint.body["receipt"])?;
        let stage = &self.placement.stage_name;
        let quarantine = format!(".backup-discard-{}", self.operation);
        filename(&quarantine)?;
        require(
            !present(&parent, final_name)?,
            "Conflict",
            "backup publication name appeared after reconciliation",
        )?;
        if present(&parent, stage)? {
            let directory = self.placement.directory(&parent, stage)?;
            require(
                members(&directory)? == self.members,
                "PreconditionFailed",
                "backup stage changed after reconciliation",
            )?;
            renameat_with(&parent, stage, &parent, &quarantine, RenameFlags::NOREPLACE)
                .map_err(std::io::Error::from)?;
            parent.sync_all()?;
        }
        let removed = present(&parent, &quarantine)?;
        if removed {
            require(
                !present(&parent, stage)? && !present(&parent, final_name)?,
                "Conflict",
                "competing backup stage placement is present",
            )?;
            let directory = self.placement.directory(&parent, &quarantine)?;
            let remaining = members(&directory)?;
            require(
                remaining
                    .iter()
                    .all(|(name, identity)| self.members.get(name) == Some(identity)),
                "PreconditionFailed",
                "backup quarantine contains an unrecognized member",
            )?;
            for (name, identity) in remaining {
                require(
                    member(&directory, self.placement.directory[0], &name)? == identity,
                    "PreconditionFailed",
                    "backup quarantine member changed before removal",
                )?;
                unlinkat(&directory, name.as_str(), AtFlags::empty())
                    .map_err(std::io::Error::from)?;
            }
            directory.sync_all()?;
            self.placement.directory(&parent, &quarantine)?;
            unlinkat(&parent, quarantine.as_str(), AtFlags::REMOVEDIR)
                .map_err(std::io::Error::from)?;
        }
        parent.sync_all()?;
        Ok(removed)
    }

    pub(crate) fn finish(self, db: &Db<'_>, removed: bool) -> Result<()> {
        require(db.c.execute(
            "DELETE FROM backup_stage_custody WHERE operation=?1 AND checkpoint_ref=?2 AND evidence_ref=?3",
            params![self.operation,self.checkpoint.reference(),self.evidence.reference()],
        )? == 1, "RecoveryRequired", "backup stage reclamation lost its authorization")?;
        db.audit(
            crate::policy::SYSTEM,
            AUDIT,
            Some(&self.operation),
            if removed {
                "Removed"
            } else {
                "ConfirmedAbsent"
            },
        )?;
        Ok(())
    }
}

pub(crate) fn next_disposal(db: &Db<'_>, cursor: &str) -> Result<Option<String>> {
    Ok(db.c.query_row(
        "SELECT operation FROM backup_stage_custody WHERE operation>?1 AND evidence_ref IS NOT NULL ORDER BY operation LIMIT 1",
        [cursor], |row| row.get(0),
    ).optional()?)
}

fn selected(conn: &Connection) -> Result<HashSet<String>> {
    let cutover = epoch(conn)?;
    let balanced: bool = conn.query_row(
        "SELECT (SELECT count(*) FROM backup_not_applied)
           = (SELECT count(*) FROM backup_stage_custody WHERE evidence_ref IS NOT NULL)
             + (SELECT count(*) FROM audit WHERE method='Storage.BackupStageReclaim')",
        [],
        |row| row.get(0),
    )?;
    require(
        balanced,
        "RecoveryRequired",
        "backup stage reclamation has missing or orphaned custody",
    )?;
    let mut known = HashSet::new();
    let mut active = conn.prepare(
        "SELECT q.operation FROM queue q JOIN current t ON t.id=q.task
         WHERE q.phase IN ('Dispatched','Unknown') AND t.spec->>'task_type'='backup.create'
         UNION
         SELECT h.holder FROM holds h JOIN current o ON o.id=h.holder JOIN current t ON t.id=o.spec->>'task_id'
         WHERE o.kind='Operation' AND t.kind='Task' AND t.spec->>'task_type'='backup.create'",
    )?;
    for operation in active.query_map([], |row| row.get::<_, String>(0))? {
        verify_active(conn, &operation?, cutover)?;
    }
    let mut stages = conn.prepare(
        "SELECT operation,stage_name,evidence_ref IS NOT NULL FROM backup_stage_custody",
    )?;
    for row in stages.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, bool>(2)?,
        ))
    })? {
        let (operation, name, disposing) = row?;
        require(
            known.len() < MAX_STAGES && known.insert(name),
            "RecoveryRequired",
            "backup stage custody exceeds its limit",
        )?;
        if disposing {
            Disposal::indexed(conn, &operation)?;
        } else {
            verify_active(conn, &operation, cutover)?;
            require(
                find(conn, &checkpoint_id(&operation))?.is_some(),
                "RecoveryRequired",
                "retained backup stage has no checkpoint",
            )?;
        }
    }
    Ok(known)
}

fn custody_error(error: crate::Error) -> crate::Error {
    if error.0.code == "RecoveryRequired" {
        error
    } else {
        err(
            "RecoveryRequired",
            format!("backup stage custody verification: {}", error.0.message),
        )
    }
}

pub(crate) fn protected_stages(db: &Db<'_>) -> Result<HashSet<String>> {
    selected(db.c).map_err(custody_error)
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let verify = || -> Result<()> {
        selected(conn)?;
        let mut history = conn.prepare("SELECT * FROM backup_not_applied")?;
        for evidence in history.query_map([], crate::db::row)? {
            let evidence = evidence?;
            let operation = evidence.body["operation_ref"]["resource_id"]
                .as_str()
                .ok_or_else(|| err("RecoveryRequired", "backup stage evidence has no Operation"))?;
            if let Some(row) = indexed(conn, operation)? {
                require(
                    row.evidence_ref == Some(evidence.reference()),
                    "RecoveryRequired",
                    "backup stage evidence differs from pending reclamation",
                )?;
            } else {
                require(
                    finished(conn, operation)?,
                    "RecoveryRequired",
                    "backup stage evidence has no reclamation record",
                )?;
                Disposal::authenticate(conn, operation, evidence.clone())?;
            }
        }
        Ok(())
    };
    verify().map_err(custody_error)
}
