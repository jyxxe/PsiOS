use super::{CLEANUP, DISPOSAL, INSPECT, Inspection, Request, cid_ok, inspection, original};
use crate::{
    Result, broker,
    db::{self, Db, Doc},
    err,
    host::Work,
    now, require,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(super) const SCHEMA: &str = "
CREATE TABLE oci_create_disposals(
    original TEXT PRIMARY KEY, task TEXT NOT NULL UNIQUE,
    evidence TEXT NOT NULL, issued INTEGER NOT NULL CHECK(issued IN (0,1))
) STRICT, WITHOUT ROWID;
CREATE INDEX oci_create_disposal_evidence ON oci_create_disposals(evidence);
CREATE UNIQUE INDEX oci_create_once ON versions(json_extract(spec,'$.original_operation_id'))
WHERE kind='OCIStartCreateDisposal' AND rev=2;";

fn id(task: &str) -> String {
    format!("oci-create-disposal-{task}")
}

struct Active {
    task: String,
    evidence: String,
    issued: bool,
}

fn active(conn: &Connection, op: &str) -> Result<Option<Active>> {
    Ok(conn
        .query_row(
            "SELECT task,evidence,issued FROM oci_create_disposals WHERE original=?1",
            [op],
            |row| {
                Ok(Active {
                    task: row.get(0)?,
                    evidence: row.get(1)?,
                    issued: row.get(2)?,
                })
            },
        )
        .optional()?)
}

pub(super) fn removal(conn: &Connection, op: &str) -> Result<Option<Doc>> {
    let issued = conn
        .query_row(
            "SELECT * FROM versions WHERE kind='OCIStartCreateDisposal' AND rev=2
         AND json_extract(spec,'$.original_operation_id')=?1",
            [op],
            db::row,
        )
        .optional()?;
    let latch = active(conn, op)?;
    require(
        issued.is_some() == latch.as_ref().is_some_and(|latch| latch.issued),
        "RecoveryRequired",
        "OCI create lost its single issued removal custody",
    )?;
    if let Some(latch) = latch {
        let retained = record(conn, op, &latch)?;
        require(
            issued
                .as_ref()
                .is_none_or(|row| row.reference() == retained.reference()),
            "RecoveryRequired",
            "OCI create removal differs from its issued witness",
        )?;
    }
    Ok(issued)
}

fn record(conn: &Connection, op: &str, latch: &Active) -> Result<Doc> {
    let record = db::find(conn, &id(&latch.task))?
        .ok_or_else(|| err("RecoveryRequired", "OCI create disposal custody is absent"))?;
    require(
        record.kind == DISPOSAL
            && record.owner == crate::policy::OWNER
            && record.spec == json!({"original_operation_id":op,"task_id":latch.task})
            && record.rev == if latch.issued { 2 } else { 1 }
            && record.body["evidence_ref"]["resource_id"] == latch.evidence
            && record.body["candidate"]["container_id"]
                .as_str()
                .is_some_and(cid_ok)
            && record.body["phase"]
                == if latch.issued {
                    "RemovalIssued"
                } else {
                    "Accepted"
                },
        "RecoveryRequired",
        "OCI create disposal latch and record disagree",
    )?;
    Ok(record)
}

pub(crate) struct Prepared {
    pub(super) inspection: Inspection,
    pub(super) candidate: Value,
    evidence: Doc,
}

pub(crate) fn prepare(db: &Db<'_>, app: &Doc, request: &Request) -> Result<Prepared> {
    let original = original(db, request, app)?;
    let inspection = inspection(db, &original)?;
    let (evidence, candidate) = broker::inspect_recovery(
        db,
        Some(&request.evidence_ref),
        &original,
        &original,
        INSPECT,
    )?;
    require(
        inspection.removal.is_none()
            && candidate["status"] == "CreatedNeverStarted"
            && candidate["operation_ref"] == original.reference()
            && candidate["issue_ref"] == inspection.issue.reference()
            && candidate["application_ref"] == app.reference()
            && candidate["volume_ref"] == json!(inspection.volume.as_ref().map(Doc::reference))
            && candidate["engine_id"] == inspection.engine()
            && candidate["boot_id"] == inspection.boot()
            && candidate["container_id"].as_str().is_some_and(cid_ok),
        "OutcomeUnknown",
        "inspection does not prove a recoverable never-started container",
    )?;
    Ok(Prepared {
        inspection,
        candidate,
        evidence,
    })
}

pub(crate) fn admissible(db: &Db<'_>, app: &Doc, request: &Request) -> Result<Prepared> {
    let prepared = prepare(db, app, request)?;
    require(
        active(db.c, &prepared.inspection.original.id)?.is_none(),
        "Conflict",
        "this OCI create already has disposal custody",
    )?;
    Ok(prepared)
}

pub(crate) fn admit(db: &Db<'_>, task: &Doc, prepared: &Prepared) -> Result<()> {
    let op = &prepared.inspection.original;
    require(
        db.c.execute(
            "INSERT OR IGNORE INTO oci_create_disposals VALUES(?1,?2,?3,0)",
            params![op.id, task.id, prepared.evidence.id],
        )? == 1,
        "Conflict",
        "this OCI create already has disposal custody",
    )?;
    db.create(
        DISPOSAL,&id(&task.id),&task.owner,json!({"original_operation_id":op.id,"task_id":task.id}),
        json!({"phase":"Accepted","candidate":prepared.candidate,"evidence_ref":prepared.evidence.reference()}),
    )?;
    Ok(())
}

fn accepted(db: &Db<'_>, w: &Work, prepared: &Prepared) -> Result<Doc> {
    let op = &prepared.inspection.original;
    let latch = active(db.c, &op.id)?.ok_or_else(|| {
        err(
            "Fenced",
            "OCI create disposal was not independently admitted",
        )
    })?;
    let record = record(db.c, &op.id, &latch)?;
    require(
        latch.task == w.task.id
            && !latch.issued
            && record.body["candidate"] == prepared.candidate
            && record.body["evidence_ref"] == prepared.evidence.reference(),
        "Fenced",
        "OCI create disposal proof or ownership changed",
    )?;
    Ok(record)
}

pub(crate) fn begin_cleanup(db: &Db<'_>, w: &Work, prepared: &Prepared) -> Result<()> {
    accepted(db, w, prepared)?;
    Ok(())
}

pub(crate) fn record_cleanup(
    db: &Db<'_>,
    w: &Work,
    request: &Request,
    sample: &Value,
) -> Result<()> {
    let prepared = prepare(db, &w.target, request)?;
    let accepted = accepted(db, w, &prepared)?;
    require(
        *sample == prepared.candidate
            && w.effect.body["phase"] == "PermitBound"
            && db.of(&w.op.id, "Operation")?.reference() == w.op.reference()
            && db.of(&w.effect.id, "Effect")?.reference() == w.effect.reference()
            && db.of(&w.run.id, "Run")?.reference() == w.run.reference()
            && db.of(&w.task.id, "Task")?.reference() == w.task.reference()
            && db.c.execute(
                "UPDATE oci_create_disposals SET issued=1 WHERE task=?1 AND issued=0",
                [&w.task.id],
            )? == 1,
        "Fenced",
        "OCI create removal has already been issued or its permit changed",
    )?;
    let mut e = w.effect.body.clone();
    e["phase"] = json!("EffectStarted");
    let effect = db.status(&w.effect, e)?;
    let mut o = w.op.body.clone();
    o["effect_refs"] = json!([effect.reference()]);
    o["updated_at"] = json!(now());
    let op = db.status(&w.op, o)?;
    for subject in [&w.run, &w.task] {
        let mut value = subject.body.clone();
        value["operation_refs"] = json!([op.reference()]);
        db.status(subject, value)?;
    }
    let mut body = accepted.body.clone();
    body["phase"] = json!("RemovalIssued");
    body["cleanup_operation_ref"] = op.reference();
    body["cleanup_effect_ref"] = effect.reference();
    body["cleanup_run_ref"] = w.run.reference();
    db.status(&accepted, body)?;
    Ok(())
}

pub(crate) fn not_dispatched(db: &Db<'_>, task: &Doc) -> Result<bool> {
    let disposition = db.of(&id(&task.id), DISPOSAL)?;
    let original = disposition.spec["original_operation_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "OCI create disposal lacks its origin"))?;
    let active = active(db.c, original)?
        .ok_or_else(|| err("RecoveryRequired", "OCI create disposal latch is absent"))?;
    require(
        active.task == task.id
            && record(db.c, original, &active)?.reference() == disposition.reference(),
        "RecoveryRequired",
        "OCI create disposal does not own its latch",
    )?;
    Ok(!active.issued)
}

pub(crate) fn close(db: &Db<'_>, task: &str) -> Result<()> {
    db.c.execute(
        "DELETE FROM oci_create_disposals WHERE task=?1 AND issued=0",
        [task],
    )?;
    Ok(())
}

pub(crate) fn release_original(db: &Db<'_>, op: &str) -> Result<()> {
    db.c.execute("DELETE FROM oci_create_disposals WHERE original=?1", [op])?;
    Ok(())
}

pub(crate) fn retains_result(db: &Db<'_>, task: &str) -> Result<bool> {
    Ok(db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM oci_create_disposals d JOIN current a ON a.id=d.evidence
                       WHERE a.spec->>'task_id'=?1)",
        [task],
        |row| row.get(0),
    )?)
}

pub(crate) fn cleanup_held(db: &Db<'_>, request: &Request, cleanup: &Doc) -> Result<bool> {
    let original = db.resolve(&request.operation_ref)?;
    Ok(removal(db.c, &original.id)?.is_some_and(|record| {
        record.spec["task_id"] == cleanup.spec["task_id"]
            && record.body["cleanup_operation_ref"]["resource_id"] == cleanup.id
    }))
}

pub(super) fn cleanup_receipt(candidate: &Value) -> Value {
    json!({"operation_ref":candidate["operation_ref"],"issue_ref":candidate["issue_ref"],
        "container_id":candidate["container_id"],"engine_id":candidate["engine_id"],
        "boot_id":candidate["boot_id"],"status":"Removed"})
}

pub(crate) fn settle_cleanup(db: &Db<'_>, work: &Work, observed: &Value) -> Result<Value> {
    let prepared = work
        .create_cleanup
        .as_ref()
        .ok_or_else(|| err("Fenced", "OCI create cleanup input unavailable"))?;
    let original = &prepared.inspection.original;
    let record = removal(db.c, &original.id)?
        .ok_or_else(|| err("OutcomeUnknown", "missing OCI removal witness"))?;
    require(
        record.spec["task_id"] == work.task.id
            && record.body["cleanup_operation_ref"]["resource_id"] == work.op.id
            && record.body["candidate"] == prepared.candidate
            && *observed == cleanup_receipt(&prepared.candidate)
            && db.get(&original.id)?.reference() == original.reference(),
        "OutcomeUnknown",
        "OCI removal receipt differs from durable custody",
    )?;
    Ok(observed.clone())
}

fn verified_absence(
    db: &Db<'_>,
    original: &Doc,
    freshness: &Doc,
    evidence: Option<&Value>,
) -> Result<(Doc, Doc)> {
    let inspection = inspection(db, original)?;
    let removal = inspection
        .removal
        .ok_or_else(|| err("OutcomeUnknown", "OCI removal was never checkpointed"))?;
    let (proof, observation) =
        broker::inspect_recovery(db, evidence, freshness, original, INSPECT)?;
    let fresh: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions r JOIN versions c ON r.seq>c.seq
            WHERE r.id=?1 AND r.rev=1 AND r.kind='Run' AND c.id=?2 AND c.rev=2)",
        params![proof.spec["run_id"].as_str(), removal.id],
        |row| row.get(0),
    )?;
    let candidate = &removal.body["candidate"];
    require(
        fresh
            && observation["status"] == "RemovalConfirmed"
            && observation["operation_ref"] == original.reference()
            && observation["issue_ref"] == candidate["issue_ref"]
            && observation["removal_ref"] == removal.reference()
            && observation["container_id"] == candidate["container_id"]
            && observation["engine_id"] == candidate["engine_id"]
            && observation["boot_id"] == candidate["boot_id"]
            && super::boot_id()? == candidate["boot_id"],
        "OutcomeUnknown",
        "fresh independent inspection does not confirm exact OCI removal",
    )?;
    Ok((proof, removal))
}

pub(crate) fn reconcile_cleanup(
    db: &Db<'_>,
    op: &Doc,
    request: &Request,
    evidence: Option<&Value>,
) -> Result<(Value, Value)> {
    let original = db.resolve(&request.operation_ref)?;
    let (proof, removal) = verified_absence(db, &original, op, evidence)?;
    require(
        removal.body["cleanup_operation_ref"]["resource_id"] == op.id
            && removal.spec["task_id"] == op.spec["task_id"],
        "Fenced",
        "OCI removal witness belongs to a different cleanup",
    )?;
    Ok((
        cleanup_receipt(&removal.body["candidate"]),
        json!({"observation_ref":proof.reference(),"removal_ref":removal.reference(),"conclusion":"RemovalConfirmed"}),
    ))
}

pub(crate) fn reconcile_original(
    db: &Db<'_>,
    op: &Doc,
    evidence: Option<&Value>,
) -> Result<(Value, Value)> {
    let (proof, removal) = verified_absence(db, op, op, evidence)?;
    let child = db.of(
        removal.body["cleanup_operation_ref"]["resource_id"]
            .as_str()
            .unwrap(),
        "Operation",
    )?;
    require(
        child.body["outcome_knowledge"] == "Known"
            && child.spec["task_id"] == removal.spec["task_id"],
        "Conflict",
        "exact OCI cleanup must settle before the original create",
    )?;
    let receipt = cleanup_receipt(&removal.body["candidate"]);
    Ok((
        receipt,
        json!({"observation_ref":proof.reference(),"removal_ref":removal.reference(),
        "cleanup_operation_ref":child.reference(),"conclusion":"CreateCompensated"}),
    ))
}

pub(super) fn verify_active(conn: &Connection) -> Result<()> {
    let mut statement =
        conn.prepare("SELECT original,task,evidence,issued FROM oci_create_disposals")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let original: String = row.get(0)?;
        let active = Active {
            task: row.get(1)?,
            evidence: row.get(2)?,
            issued: row.get(3)?,
        };
        let record = record(conn, &original, &active)?;
        let op = db::find(conn, &original)?
            .ok_or_else(|| err("RecoveryRequired", "OCI create disposal origin missing"))?;
        require(
            op.body["outcome_knowledge"] == "Unknown"
                && record.body["candidate"]["operation_ref"] == op.reference()
                && db::find(conn, &active.task)?
                    .is_some_and(|task| task.spec["task_type"] == CLEANUP)
                && db::resolve(conn, &record.body["evidence_ref"])?.kind == "Artifact",
            "RecoveryRequired",
            "OCI create disposal has no exact unresolved operation or evidence",
        )?;
    }
    let detached:bool=conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue q JOIN versions c
             ON json_extract(c.spec,'$.original_operation_id')=q.operation
           WHERE q.phase='Unknown' AND c.kind='OCIStartCreateDisposal' AND c.rev=2
             AND NOT EXISTS(SELECT 1 FROM oci_create_disposals d WHERE d.original=q.operation AND d.issued=1))",
        [], |row| row.get(0),
    )?;
    require(
        !detached,
        "RecoveryRequired",
        "unresolved OCI create lost its issued cleanup latch",
    )?;
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue q JOIN current t ON t.id=q.task
                WHERE q.phase<>'Done' AND t.spec->>'task_type'=?1
                AND NOT EXISTS(SELECT 1 FROM oci_create_disposals d WHERE d.task=q.task))",
        [CLEANUP],
        |row| row.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "active OCI create cleanup lost its custody latch",
    )
}

pub(super) fn verify(conn: &Connection) -> Result<()> {
    verify_active(conn)?;
    let mut statement = conn.prepare("SELECT * FROM versions WHERE kind=?1 AND rev=2")?;
    for record in statement.query_map([DISPOSAL], db::row)? {
        let record = record?;
        let cleanup = db::resolve(conn, &record.body["cleanup_operation_ref"])?;
        let effect = db::resolve(conn, &record.body["cleanup_effect_ref"])?;
        let original = db::resolve(conn, &record.body["candidate"]["operation_ref"])?;
        require(
            cleanup.kind == "Operation"
                && original.kind == "Operation"
                && cleanup.spec["task_id"] == record.spec["task_id"]
                && original.id == record.spec["original_operation_id"]
                && cleanup.body["effect_refs"][0] == effect.reference()
                && effect.body["phase"] == "EffectStarted"
                && effect.body["action"] == CLEANUP,
            "RecoveryRequired",
            "OCI create removal witness has no matching authorized cleanup",
        )?;
    }
    Ok(())
}
