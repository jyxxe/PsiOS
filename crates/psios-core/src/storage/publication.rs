//! Recover a completed rename by following the original permit and staged directory.
use super::*;

pub(crate) const INSPECT: &str = "backup.inspect_publication";

fn checkpoint_id(operation: &str) -> String {
    format!("backup-publication-{operation}")
}

fn intent_id(operation: &str) -> String {
    format!("backup-intent-{operation}")
}

pub(crate) fn begin(db: &Db<'_>, work: &Work) -> Result<()> {
    held(db, &work.op, &work.target)?;
    db.create(
        "BackupPublicationIntent",
        &intent_id(&work.op.id),
        &work.op.owner,
        json!({"operation_id":work.op.id}),
        json!({"plan_ref":work.op.body["plan_ref"], "effect_id":work.effect.id}),
    )?;
    Ok(())
}

pub(crate) fn not_published(db: &Db<'_>, op: &Doc) -> Result<bool> {
    // Only a dispatch by this protocol proves that publication could not
    // have started before a checkpoint was committed to the witnessed DB.
    let Some(intent) = db.find(&intent_id(&op.id))? else {
        return Ok(false);
    };
    Ok(intent.kind == "BackupPublicationIntent"
        && intent.owner == op.owner
        && intent.spec["operation_id"] == op.id
        && intent.body["plan_ref"] == op.body["plan_ref"]
        && intent.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
        && db.find(&checkpoint_id(&op.id))?.is_none())
}

fn held(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<()> {
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |r| r.get(0),
    )?;
    require(
        held,
        "RecoveryRequired",
        "original backup volume hold missing",
    )
}

pub(crate) fn record(
    db: &Db<'_>,
    work: &Work,
    receipt: &Value,
    device: u64,
    inode: u64,
) -> Result<()> {
    held(db, &work.op, &work.target)?;
    require(
        not_published(db, &work.op)?,
        "Fenced",
        "backup publication intent is unavailable or already used",
    )?;
    db.create(
        "BackupPublicationCheckpoint",
        &checkpoint_id(&work.op.id),
        &work.task.owner,
        json!({"operation_id":work.op.id}),
        json!({
            "plan_ref":work.op.body["plan_ref"],
            "effect_id":work.effect.id,
            "source_ref":work.target.reference(),
            "data_incarnation":work.target.body["data_incarnation"],
            "directory": [device,inode],
            "receipt":receipt
        }),
    )?;
    Ok(())
}

fn recorded(db: &Db<'_>, op: &Doc, volume: &Doc) -> Result<Doc> {
    held(db, op, volume)?;
    let c = db.find(&checkpoint_id(&op.id))?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "no committed backup publication checkpoint",
        )
    })?;
    require(
        c.kind == "BackupPublicationCheckpoint"
            && c.owner == op.owner
            && c.spec["operation_id"] == op.id
            && c.body["plan_ref"] == op.body["plan_ref"]
            && c.body["effect_id"] == op.body["effect_refs"][0]["resource_id"]
            && c.body["source_ref"] == volume.reference()
            && c.body["data_incarnation"] == volume.body["data_incarnation"],
        "RecoveryRequired",
        "backup publication checkpoint does not bind this operation and volume",
    )?;
    Ok(c)
}

pub(crate) fn pending(db: &Db<'_>, op: &Doc) -> Result<Doc> {
    let task = db.of(op.spec["task_id"].as_str().unwrap(), "Task")?;
    require(
        op.body["outcome_knowledge"] == "Unknown" && task.spec["task_type"] == "backup.create",
        "PreconditionFailed",
        "operation is not an unresolved backup creation",
    )?;
    let plan = db.resolve(&op.body["plan_ref"])?;
    let original = db.resolve(&plan.body["allowed_effects"][0]["resource_ref"])?;
    let volume = db.of(&original.id, "Volume")?;
    require(
        volume.reference() == original.reference(),
        "RecoveryRequired",
        "source volume differs from the backup execution plan",
    )?;
    recorded(db, op, &volume)
}

fn open_candidate(cfg: &Config, c: &Doc) -> Result<(File, File)> {
    let id = c.body["receipt"]["id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "checkpointed backup has no identity"))?;
    let (parent, root) = open_backup(cfg, id)?;
    let m = root.metadata()?;
    require(
        c.body["directory"] == json!([m.dev(), m.ino()]),
        "PreconditionFailed",
        "published backup directory differs from the checkpoint",
    )?;
    Ok((parent, root))
}

fn file_set_digest(root: &File) -> Result<String> {
    let device = root.metadata()?.dev();
    let mut members = std::collections::BTreeMap::new();
    for entry in Dir::read_from(root).map_err(std::io::Error::from)? {
        let entry = entry.map_err(std::io::Error::from)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| err("PreconditionFailed", "invalid name in published backup"))?;
        if matches!(name, "." | "..") {
            continue;
        }
        let blob = name
            .strip_prefix("blob-")
            .and_then(|n| n.parse::<usize>().ok())
            .is_some_and(|n| n < MAX_ENTRIES);
        require(
            members.len() <= MAX_ENTRIES && (name == "manifest.json" || blob),
            "PreconditionFailed",
            "unexpected member in published backup",
        )?;
        let m = statat(root, name, AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
        require(
            FileType::from_raw_mode(m.st_mode) == FileType::RegularFile
                && m.st_nlink == 1
                && m.st_dev == device,
            "PreconditionFailed",
            "published backup member changed type or filesystem",
        )?;
        members.insert(
            name.to_owned(),
            json!([
                m.st_ino,
                m.st_size,
                m.st_mtime,
                m.st_mtime_nsec,
                m.st_ctime,
                m.st_ctime_nsec
            ]),
        );
    }
    Ok(sha256(&canonical(&json!(members))))
}

pub(super) fn observe(work: &Work) -> Result<Value> {
    let c = work
        .publication_checkpoint
        .as_ref()
        .ok_or_else(|| err("RecoveryRequired", "inspection lacks its backup checkpoint"))?;
    let (parent, root) = open_candidate(&work.cfg, c)?;
    let manifest = open_manifest(&root)?;
    let before = file_set_digest(&root)?;
    let receipt = &c.body["receipt"];
    let mut report = inspect_members(
        &work.cfg,
        &root,
        &manifest,
        receipt["id"].as_str().unwrap(),
        &c.body["source_ref"],
        receipt,
        None,
    )?;
    require(
        before == file_set_digest(&root)?,
        "PreconditionFailed",
        "backup file set changed during inspection",
    )?;
    // The data files were synced during staging. Seal the observed name if
    // the original process lost power between the rename and its directory sync.
    root.sync_all()?;
    parent.sync_all()?;
    report["file_set_digest"] = json!(before);
    report["checkpoint_ref"] = c.reference();
    report["operation_ref"] = work.target.reference();
    Ok(report)
}

pub(crate) fn reconcile(
    db: &Db<'_>,
    op: &Doc,
    volume: &Doc,
    observation: &Doc,
) -> Result<(Value, Value)> {
    let c = recorded(db, op, volume)?;
    let receipt = &c.body["receipt"];
    let proof = &observation.body;
    require(
        proof["operation_ref"] == op.reference()
            && proof["checkpoint_ref"] == c.reference()
            && proof["manifest_digest"] == receipt["manifest_digest"]
            && proof["entries"] == receipt["entries"]
            && proof["bytes"] == receipt["bytes"],
        "OutcomeUnknown",
        "observation does not verify the original backup publication",
    )?;
    let (_, root) = open_candidate(db.cfg, &c)?;
    require(
        proof["file_set_digest"] == file_set_digest(&root)?,
        "OutcomeUnknown",
        "backup file set changed after inspection",
    )?;
    Ok((
        catalog(db, op, volume, receipt)?,
        json!({"observation_ref":observation.reference(),"checkpoint_ref":c.reference(),
               "conclusion":"BackupPublicationConfirmed"}),
    ))
}

pub(super) fn settle(db: &Db<'_>, op: &Doc, volume: &Doc, receipt: &Value) -> Result<Value> {
    let c = recorded(db, op, volume)?;
    require(
        c.body["receipt"] == *receipt,
        "RecoveryRequired",
        "backup receipt disagrees with the committed checkpoint",
    )?;
    open_candidate(db.cfg, &c)?;
    catalog(db, op, volume, receipt)
}

fn catalog(db: &Db<'_>, op: &Doc, volume: &Doc, receipt: &Value) -> Result<Value> {
    let b = json!({
        "lifecycle": "Stored",
        "manifest_digest":receipt["manifest_digest"],
        "entries":receipt["entries"],
        "bytes":receipt["bytes"],
        "consistency": "crash_consistent",
        "failure_domain": "local-filesystem",
        "coordination": "exclusive offline volume hold"
    });
    Ok(db
        .create(
            "Backup",
            receipt["id"].as_str().unwrap(),
            &op.owner,
            json!({"source_ref":volume.reference(),"source_volume":volume.id}),
            b,
        )?
        .reference())
}
