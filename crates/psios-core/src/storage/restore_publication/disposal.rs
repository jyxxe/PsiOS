//! Reclaim verified-unpublished restore stages only after logical settlement.
use super::*;
use rustix::fs::{RenameFlags, renameat_with};

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
        let (checkpoint_ref, evidence_ref): (Value, Value) = db.c.query_row(
            "SELECT checkpoint_ref,evidence_ref FROM restore_stage_disposals WHERE operation=?1",
            [operation],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let op = db.of(operation, "Operation")?;
        let checkpoint = db.resolve(&checkpoint_ref)?;
        let evidence = db.resolve(&evidence_ref)?;
        let prior = db.resolve(&evidence.body["operation_ref"])?;
        let effect = db.resolve(&op.body["effect_refs"][0])?;
        let intent = db.get(&intent_id(operation))?;
        verify_custody(&op, &checkpoint, intent.as_ref())?;
        require(
            checkpoint.id == checkpoint_id(operation)
                && op.body["outcome_knowledge"] == "Known"
                && matches!(op.body["lifecycle"].as_str(), Some("Failed" | "Cancelled"))
                && evidence.kind == "RecoveryEvidence"
                && evidence.owner == op.owner
                && evidence.body["evidence"]["checkpoint_ref"] == checkpoint_ref
                && evidence.body["evidence"]["conclusion"] == NOT_APPLIED
                && evidence.body["target_ref"] == checkpoint.body["backup_ref"]
                && prior.kind == "Operation"
                && prior.id == operation
                && prior.owner == op.owner
                && prior.body["outcome_knowledge"] == "Unknown"
                && prior.spec == op.spec
                && prior.body["plan_ref"] == op.body["plan_ref"]
                && effect.kind == "Effect"
                && effect.owner == op.owner
                && effect.body["operation_id"] == operation
                && effect.body["phase"] == "EffectReconciled"
                && effect.body["evidence_refs"]
                    .as_array()
                    .is_some_and(|refs| refs.contains(&evidence_ref)),
            "RecoveryRequired",
            "restore stage disposal has no settled not-applied evidence",
        )?;
        Ok(Self {
            operation: operation.into(),
            checkpoint,
            evidence,
        })
    }

    pub(crate) fn run(&self, cfg: &Config) -> Result<bool> {
        let placement: Placement = decode(&self.checkpoint.body["placement"])?;
        let source = placement
            .stage_name()?
            .ok_or_else(|| err("RecoveryRequired", "restore discard has no original stage"))?;
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
            let (_dir, _data) = open_exact(&parent, source, &placement, receipt)?;
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
            staging::discard(&parent, retired.as_str(), &dir)?;
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
            "Storage.RestoreStageReclaim",
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
