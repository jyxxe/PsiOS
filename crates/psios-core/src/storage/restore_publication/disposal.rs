//! Reclaim verified-unpublished restore stages only after logical settlement.
use super::*;
use crate::db::{find, resolve};
use rusqlite::Connection;
use rustix::fs::{RenameFlags, renameat_with};

const AUDIT: &str = "Storage.RestoreStageReclaim";

pub(crate) fn queue_disposal(db: &Db<'_>, op: &Doc, evidence: &Doc) -> Result<()> {
    if evidence.body["evidence"]["conclusion"] != NOT_APPLIED {
        return Ok(());
    }
    db.c.execute(
        "INSERT INTO restore_stage_disposals(operation,checkpoint_ref,evidence_ref) VALUES(?1,?2,?3)",
        params![op.id, evidence.body["evidence"]["checkpoint_ref"], evidence.reference()],
    )?;
    Ok(())
}

pub(crate) struct Disposal {
    operation: String,
    checkpoint: Doc,
    evidence: Doc,
}

pub(crate) fn next_disposal(db: &Db<'_>, cursor: &str) -> Result<Option<String>> {
    Ok(db.c.query_row(
        "SELECT operation FROM restore_stage_disposals WHERE operation>?1 ORDER BY operation LIMIT 1",
        [cursor], |r| r.get(0),
    ).optional()?)
}

impl Disposal {
    pub(crate) fn open(db: &Db<'_>, operation: &str) -> Result<Self> {
        Self::indexed(db.c, operation)
    }

    pub(super) fn indexed(conn: &Connection, operation: &str) -> Result<Self> {
        let (checkpoint_ref, evidence_ref): (Value, Value) = conn.query_row(
            "SELECT checkpoint_ref,evidence_ref FROM restore_stage_disposals WHERE operation=?1",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let evidence = resolve(conn, &evidence_ref)?;
        require(
            evidence.body["evidence"]["checkpoint_ref"] == checkpoint_ref
                && !finished(conn, operation)?,
            "RecoveryRequired",
            "pending restore disposal disagrees with its evidence or completion",
        )?;
        Self::authenticate(conn, operation, evidence)
    }

    pub(super) fn authenticate(conn: &Connection, operation: &str, evidence: Doc) -> Result<Self> {
        let op = verification::needed(conn, operation)?;
        let checkpoint = resolve(conn, &evidence.body["evidence"]["checkpoint_ref"])?;
        let prior = resolve(conn, &evidence.body["operation_ref"])?;
        let effect = resolve(conn, &op.body["effect_refs"][0])?;
        let intent = find(conn, &intent_id(operation))?;
        let (binding, source) = Source::load(conn, &op)?;
        let placement = verify_custody(conn, &op, &checkpoint, intent.as_ref())?;
        binding.check_receipt(&op, &checkpoint.body["receipt"])?;
        require(
            placement.stage_name()?.is_some()
                && op.kind == "Operation"
                && checkpoint.id == checkpoint_id(operation)
                && op.body["outcome_knowledge"] == "Known"
                && matches!(op.body["lifecycle"].as_str(), Some("Failed" | "Cancelled"))
                && evidence.kind == "RecoveryEvidence"
                && evidence.rev == 1
                && evidence.owner == op.owner
                && evidence.body["evidence"]["conclusion"] == NOT_APPLIED
                && evidence.body["target_ref"] == source.reference()
                && binding.matches(&checkpoint, &source, true)
                && prior.kind == "Operation"
                && prior.id == operation
                && prior.owner == op.owner
                && prior.body["outcome_knowledge"] == "Unknown"
                && prior.spec == op.spec
                && prior.body["plan_ref"] == op.body["plan_ref"]
                && effect.kind == "Effect"
                && effect.owner == op.owner
                && effect.body["operation_id"] == operation
                && effect.body["action"] == binding.tool()
                && effect.body["resource_ref"] == source.reference()
                && effect.body["phase"] == "EffectReconciled"
                && effect.body["evidence_refs"]
                    .as_array()
                    .is_some_and(|refs| refs.contains(&evidence.reference())),
            "RecoveryRequired",
            "restore stage disposal has no settled not-applied evidence",
        )?;
        Ok(Self {
            operation: operation.into(),
            checkpoint,
            evidence,
        })
    }

    pub(super) fn stage_name(&self) -> Result<&str> {
        self.checkpoint.body["placement"]["stage_name"]
            .as_str()
            .ok_or_else(|| err("RecoveryRequired", "restore discard has no original stage"))
    }

    pub(crate) fn run(&self, cfg: &Config) -> Result<bool> {
        let placement: Placement = decode(&self.checkpoint.body["placement"])?;
        let source = self.stage_name()?;
        let receipt = &self.checkpoint.body["receipt"];
        let final_name = volume_id(receipt)?;
        let parent = open_parent(cfg, &placement)?;
        let retired = format!(".restore-discard-{}", self.operation);
        filename(&retired)?;
        require(
            !present(&parent, final_name)?,
            "Conflict",
            "restore destination changed after reconciliation",
        )?;
        if present(&parent, source)? {
            // No filesystem deletion before this no-replace rename. A retry
            // can continue on the quarantined original inode after partial removal.
            let (_dir, data) = open_exact(&parent, source, &placement, receipt)?;
            if self.checkpoint.body.get("create_source").is_some() {
                creation::check_empty(&data)?;
            }
            renameat_with(&parent, source, &parent, &retired, RenameFlags::NOREPLACE)
                .map_err(std::io::Error::from)?;
            parent.sync_all()?;
        }
        let removed = present(&parent, &retired)?;
        if removed {
            let dir = child(&parent, &retired, OFlags::RDONLY | OFlags::DIRECTORY)?;
            require(
                private_identity(&dir, &placement.directory)?,
                "PreconditionFailed",
                "restore quarantine directory differs from the verified stage",
            )?;
            if self.checkpoint.body.get("create_source").is_some() {
                creation::dispose(&parent, &retired, &dir, receipt)?;
            } else {
                staging::discard(&parent, retired.as_str(), &dir)?;
            }
        }
        parent.sync_all()?;
        Ok(removed)
    }

    pub(crate) fn finish(self, db: &Db<'_>, removed: bool) -> Result<()> {
        require(db.c.execute(
            "DELETE FROM restore_stage_disposals WHERE operation=?1 AND checkpoint_ref=?2 AND evidence_ref=?3",
            params![self.operation,self.checkpoint.reference(),self.evidence.reference()],
        )? == 1, "RecoveryRequired", "restore disposal authorization changed")?;
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

pub(super) fn finished(conn: &Connection, operation: &str) -> Result<bool> {
    let audited = conn.query_row(
        "SELECT actor,decision FROM audit WHERE method='Storage.RestoreStageReclaim' AND resource=?1",
        [operation],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ).optional()?;
    if let Some((actor, outcome)) = audited {
        require(
            actor == crate::policy::SYSTEM
                && matches!(outcome.as_str(), "Removed" | "ConfirmedAbsent"),
            "RecoveryRequired",
            "restore disposal completion record is invalid",
        )?;
        return Ok(true);
    }
    Ok(false)
}
