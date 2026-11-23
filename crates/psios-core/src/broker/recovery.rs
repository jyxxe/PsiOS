use super::*;

pub fn recover(db: &Db<'_>) -> Result<()> {
    let active = db
        .c
        .prepare("SELECT task,phase,attempt FROM queue WHERE phase IN ('Claimed','Dispatched')")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, u32>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (task, phase, attempt) in active {
        let t = db.get(&task)?;
        let read = policy::readonly(field(&t.spec, "task_type"));
        if phase == "Dispatched" && !read {
            close(
                db,
                &t,
                if t.body["lifecycle"] == "Cancelling" {
                    "Cancelled"
                } else {
                    "Paused"
                },
                "Unknown",
                Some(&err(
                    "OutcomeUnknown",
                    "authority restarted after effect dispatch; reconcile operation before reuse",
                )),
                None,
            )?;
            continue;
        }
        if t.body["lifecycle"] == "Cancelling" || attempt >= 3 {
            close(
                db,
                &t,
                if t.body["lifecycle"] == "Cancelling" {
                    "Cancelled"
                } else {
                    "Failed"
                },
                "Known",
                Some(&err("LeaseLost", "run interrupted")),
                None,
            )?;
            continue;
        }
        let mut op = operation(db, &t)?;
        if phase == "Dispatched" {
            let reserved=db.c.execute("UPDATE budgets SET reserved=reserved+1 WHERE id=(SELECT budget FROM queue WHERE task=?1) AND spent+reserved<maximum",[&task])?;
            if reserved == 0 {
                close(
                    db,
                    &t,
                    "Failed",
                    "Known",
                    Some(&err("QuotaExceeded", "read retry budget exhausted")),
                    None,
                )?;
                continue;
            }
            let mut effects = op.body["effect_refs"].clone();
            let last = effects.as_array_mut().unwrap().last_mut().unwrap();
            let effect = db.resolve(last)?;
            let evidence = db.record(
                "RecoveryEvidence",
                &t.owner,
                json!({
                    "tool": t.spec["task_type"],
                    "run_id": t.body["current_run_id"],
                    "conclusion": "ReadOnlyResultLost",
                    "observed_at": now()
                }),
            )?;
            let mut b = effect.body.clone();
            b["phase"] = json!("EffectReconciled");
            b["outcome_knowledge"] = json!("Known");
            b["evidence_refs"] = json!([evidence.reference()]);
            *last = db.status(&effect, b)?.reference();
            op = op_update(db, &op, "Pending", "NotStarted", effects, json!([]))?;
            db.c.execute("DELETE FROM holds WHERE holder=?1", [&op.id])?;
        }
        let r = db.get(field(&t.body, "current_run_id"))?;
        let mut b = r.body.clone();
        b["operation_refs"] = json!([op.reference()]);
        b["lifecycle"] = json!("Lost");
        b["outcome_knowledge"] = json!(if read && phase == "Dispatched" {
            "Unknown"
        } else {
            "NotStarted"
        });
        b["conditions"] = if phase == "Dispatched" {
            json!([
                condition("OutcomeUnknown", "read result lost with worker"),
                condition("LeaseLost", "local worker restarted")
            ])
        } else {
            json!([condition("LeaseLost", "local worker restarted")])
        };
        db.status(&r, b)?;
        let mut b = t.body.clone();
        b.as_object_mut().unwrap().remove("current_run_id");
        b["lifecycle"] = json!("Queued");
        b["operation_refs"] = json!([op.reference()]);
        b["outcome_knowledge"] = json!("NotStarted");
        db.status(&t, b)?;
        db.c.execute("UPDATE queue SET phase='Queued' WHERE task=?1", [&task])?;
    }
    Ok(())
}

/// Reconcile only an independently verifiable local postcondition. Never dispatch the write again.
pub fn reconcile(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let req: Conditional = decode(params)?;
    let op = db.of(&req.id, "Operation")?;
    op.cas(&req.expected_resource_version)?;
    require(
        op.body["outcome_knowledge"] == "Unknown",
        "PreconditionFailed",
        "operation has no unknown outcome",
    )?;
    let t = db.of(field(&op.spec, "task_id"), "Task")?;
    let input = db.resolve(&t.spec["input_ref"])?;
    let Action::Write(write) = Action::parse(field(&t.spec, "task_type"), &input.body)? else {
        return Err(err(
            "UnsupportedCapability",
            "this operation requires offline reconciliation",
        ));
    };
    let volume = db.of(&write.id, "Volume")?;
    let held: bool = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
        params![volume.id, op.id],
        |r| r.get(0),
    )?;
    require(
        held && volume.version() == write.expected_resource_version,
        "RecoveryRequired",
        "original exclusive hold or volume revision unavailable",
    )?;
    let receipt = storage::confirm_write(db.cfg, &volume, &write.name, &write.content)?;
    let volume = db.status(&volume, volume.body.clone())?;
    let evidence = json!({"volume_ref": volume.reference(), "receipt": receipt});
    let artifact = db.create(
        "Artifact",
        &id("artifact"),
        &t.owner,
        json!({
            "task_id": t.id,
            "operation_id": op.id,
            "run_id": t.body["current_run_id"],
            "effect_ref": op.body["effect_refs"][0],
            "media_type": "application/json",
            "size_bytes": canonical(&evidence).len().to_string(),
            "digest": sha256(&canonical(&evidence))
        }),
        evidence,
    )?;
    let state = if t.body["lifecycle"] == "Cancelled" {
        "Cancelled"
    } else {
        "Succeeded"
    };
    close(db, &t, state, "Known", None, Some(&artifact))?;
    db.audit(
        actor,
        "Operation.Reconcile",
        Some(&op.id),
        "ObservedPostcondition",
    )?;
    Ok(db.get(&op.id)?.body)
}
