use super::*;

pub fn recover(db: &Db<'_>) -> Result<()> {
    apps::mount_observation::verify_active(db.c)?;
    apps::stop::verify_active(db.c)?;
    apps::restart::verify_active(db.c)?;
    node::network::trial::verify_active(db.c, crate::db::CURRENT_FORMAT)?;
    artifact::retention::delayed::verify_pending(db.c)?;
    network_trial::recover_parked(db)?;
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
        let tool = field(&t.spec, "task_type");
        let read = policy::readonly(tool);
        // Activation verifies the filesystem and changes the volume only in the finish transaction.
        let activation_uncommitted = phase == "Dispatched" && tool == "volume.activate";
        // The catalog change, pin update and receipt commit together in the
        // finish transaction. Before settlement the host only inspects data.
        let catalog_uncommitted = phase == "Dispatched"
            && matches!(
                tool,
                apps::artifact_install::TOOL
                    | apps::artifact_install::STAGE_TOOL
                    | apps::release_retirement::TOOL
                    | apps::volume_rebind::TOOL
            );
        // Export is read-only at Docker; the authority publishes the archive with its receipt.
        let capture_uncommitted = phase == "Dispatched" && tool == apps::capture::TOOL;
        let backup_unpublished = phase == "Dispatched"
            && tool == "backup.create"
            && storage::publication::not_published(db, &operation(db, &t)?)?;
        let restore_unpublished = phase == "Dispatched"
            && tool == "backup.restore"
            && storage::restore_publication::not_published(db, &operation(db, &t)?)?;
        let trial_unstarted = phase == "Dispatched"
            && tool == node::network::trial::TOOL
            && node::network::trial::not_started(db, &operation(db, &t)?)?;
        let restart_undispatched = phase == "Dispatched"
            && tool == apps::restart::TOOL
            && apps::restart::not_dispatched(db, &operation(db, &t)?)?;
        let stop_undispatched = phase == "Dispatched"
            && matches!(tool, "application.stop" | apps::stop::TOOL)
            && apps::stop::not_dispatched(db, &operation(db, &t)?)?;
        let service_undispatched = phase == "Dispatched"
            && node::service_control::is_tool(tool)
            && node::service_control::not_dispatched(db, &operation(db, &t)?)?;
        let image_undispatched = phase == "Dispatched"
            && tool == apps::restore_image::TOOL
            && apps::restore_image::not_dispatched(db, &operation(db, &t)?)?;
        let rename_unprepared = phase == "Dispatched"
            && tool == storage::file_move::TOOL
            && storage::file_move::not_prepared(db, &operation(db, &t)?)?;
        let deletion_unprepared = phase == "Dispatched"
            && matches!(tool, "volume.delete" | "backup.delete")
            && storage::retirement::not_prepared(db, &operation(db, &t)?)?;
        if phase == "Dispatched"
            && !read
            && !activation_uncommitted
            && !catalog_uncommitted
            && !capture_uncommitted
            && !backup_unpublished
            && !restore_unpublished
            && !trial_unstarted
            && !restart_undispatched
            && !stop_undispatched
            && !service_undispatched
            && !image_undispatched
            && !deletion_unprepared
            && !rename_unprepared
        {
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
        if t.body["lifecycle"] == "Cancelling"
            || attempt >= 3
            || activation_uncommitted
            || catalog_uncommitted
            || capture_uncommitted
            || backup_unpublished
            || restore_unpublished
            || trial_unstarted
            || restart_undispatched
            || stop_undispatched
            || service_undispatched
            || image_undispatched
            || deletion_unprepared
            || rename_unprepared
        {
            close(
                db,
                &t,
                if t.body["lifecycle"] == "Cancelling" {
                    "Cancelled"
                } else {
                    "Failed"
                },
                if service_undispatched
                    || stop_undispatched
                    || restart_undispatched
                    || trial_unstarted
                {
                    "NotStarted"
                } else {
                    "Known"
                },
                Some(&err(
                    "LeaseLost",
                    if backup_unpublished {
                        "backup interrupted before publication was authorized"
                    } else if restore_unpublished {
                        "restore interrupted before volume publication was authorized"
                    } else if trial_unstarted {
                        "network trial stopped before its checkpoint invocation"
                    } else if restart_undispatched {
                        "OCI restart interrupted before its invocation was checkpointed"
                    } else if stop_undispatched {
                        "OCI stop or cleanup interrupted before its dispatch was checkpointed"
                    } else if service_undispatched {
                        "systemd control interrupted before its invocation was checkpointed"
                    } else if image_undispatched {
                        "OCI image restore interrupted before load was checkpointed"
                    } else if activation_uncommitted {
                        "activation had not committed at restart"
                    } else if catalog_uncommitted {
                        "application catalog change had not committed at restart"
                    } else if capture_uncommitted {
                        "OCI archive publication had not committed at restart"
                    } else if rename_unprepared {
                        "file rename interrupted before its observation was checkpointed"
                    } else if deletion_unprepared {
                        "storage deletion interrupted before its identity was prepared"
                    } else {
                        "run interrupted"
                    },
                )),
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
            storage::read_holds::release(db, &t.id)?;
        }
        let knowledge = if read && phase == "Dispatched" {
            "Unknown"
        } else {
            "NotStarted"
        };
        let conditions = if phase == "Dispatched" {
            json!([
                condition("OutcomeUnknown", "read result lost with worker"),
                condition("LeaseLost", "local worker restarted")
            ])
        } else {
            json!([condition("LeaseLost", "local worker restarted")])
        };
        requeue(db, &t, &op, knowledge, conditions)?;
    }
    Ok(())
}

pub fn release_unstarted_claim(db: &Db<'_>, task: &str) -> Result<()> {
    let claimed = db.c.query_row(
        "SELECT EXISTS(SELECT 1 FROM queue WHERE task=?1 AND phase='Claimed')",
        [task],
        |row| row.get::<_, bool>(0),
    )?;
    if !claimed {
        return Ok(());
    }
    let t = db.of(task, "Task")?;
    require(
        t.body["lifecycle"] == "Running" && t.body["outcome_knowledge"] == "NotStarted",
        "RecoveryRequired",
        "claimed task cannot be returned after an effect has started",
    )?;
    requeue(
        db,
        &t,
        &operation(db, &t)?,
        "NotStarted",
        json!([condition(
            "LeaseLost",
            "authority stopped before effect dispatch"
        )]),
    )
}

pub(super) fn requeue(
    db: &Db<'_>,
    task: &Doc,
    op: &Doc,
    knowledge: &str,
    conditions: Value,
) -> Result<()> {
    let run = db.get(field(&task.body, "current_run_id"))?;
    let mut state = run.body.clone();
    state["operation_refs"] = json!([op.reference()]);
    state["lifecycle"] = json!("Lost");
    state["outcome_knowledge"] = json!(knowledge);
    state["conditions"] = conditions;
    db.status(&run, state)?;
    let mut state = task.body.clone();
    state.as_object_mut().unwrap().remove("current_run_id");
    state["lifecycle"] = json!("Queued");
    state["operation_refs"] = json!([op.reference()]);
    state["outcome_knowledge"] = json!("NotStarted");
    db.status(task, state)?;
    db.c.execute("UPDATE queue SET phase='Queued' WHERE task=?1", [&task.id])?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reconcile {
    id: String,
    expected_resource_version: String,
    evidence_ref: Option<Value>,
}
pub(crate) fn inspected(
    db: &Db<'_>,
    reference: Option<&Value>,
    op: &Doc,
    target: &Doc,
    tool: &str,
) -> Result<(Doc, Value)> {
    let rf = reference.ok_or_else(|| {
        err(
            "PreconditionFailed",
            "a fresh inspection artifact is required",
        )
    })?;
    let proof = db.resolve(rf)?;
    require(
        proof.kind == "Artifact",
        "PreconditionFailed",
        "expected a retained inspection artifact",
    )?;
    let identity = |name: &str| {
        proof.spec[name].as_str().ok_or_else(|| {
            err(
                "PreconditionFailed",
                "inspection artifact has no producing Task, Run or Operation",
            )
        })
    };
    let observation_task = db.of(identity("task_id")?, "Task")?;
    let inspect_op = db.of(identity("operation_id")?, "Operation")?;
    let inspect_plan = db.resolve(&inspect_op.body["plan_ref"])?;
    let fresh:bool=db.c.query_row("SELECT EXISTS(SELECT 1 FROM versions sample JOIN versions lost
                ON sample.seq>lost.seq WHERE sample.kind='Run' AND sample.id=?1 AND sample.rev=1 AND lost.id=?2 AND lost.rev=?3)",
                params![identity("run_id")?,op.id,op.rev],|r|r.get(0))?;
    require(
        fresh
            && observation_task.spec["task_type"] == tool
            && observation_task.body["lifecycle"] == "Succeeded"
            && inspect_plan.body["allowed_effects"][0]["resource_ref"] == target.reference(),
        "OutcomeUnknown",
        "inspection does not witness this operation after recovery",
    )?;
    let observation = artifact::read(db, &proof, None)?;
    Ok((proof, observation))
}
fn reconcile_request(db: &Db<'_>, actor: &str, params: &Value) -> Result<(Reconcile, Doc)> {
    policy::admin(actor)?;
    let req: Reconcile = decode(params)?;
    let op = db.of(&req.id, "Operation")?;
    op.cas(&req.expected_resource_version)?;
    require(
        op.body["outcome_knowledge"] == "Unknown",
        "PreconditionFailed",
        "operation has no unknown outcome",
    )?;
    Ok((req, op))
}

pub(crate) enum ReconcileProbe {
    Service(node::service_control::ReconcileProbe),
    Network(Box<node::network::trial::Inspection>),
    VolumeFile(Box<storage::reconcile_file::Probe>),
}

pub(crate) enum ReconcileSample {
    Service(node::service_control::ReconcileSample),
    Network(node::network::trial::ReconcileSample),
    VolumeFile(Box<storage::reconcile_file::Sample>),
}

impl ReconcileProbe {
    pub(crate) fn read(self) -> Result<ReconcileSample> {
        match self {
            Self::Service(probe) => probe.read().map(ReconcileSample::Service),
            Self::Network(probe) => probe.live().map(ReconcileSample::Network),
            Self::VolumeFile(probe) => probe
                .read()
                .map(|file| ReconcileSample::VolumeFile(Box::new(file))),
        }
    }
}

pub(crate) fn prepare_reconcile(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
) -> Result<Option<ReconcileProbe>> {
    let (req, op) = reconcile_request(db, actor, params)?;
    let task = db.of(field(&op.spec, "task_id"), "Task")?;
    if node::service_control::is_tool(field(&task.spec, "task_type")) {
        Ok(
            node::service_control::prepare_reconcile_probe(db, &op, req.evidence_ref.as_ref())?
                .map(ReconcileProbe::Service),
        )
    } else if task.spec["task_type"] == node::network::trial::TOOL {
        inspected(
            db,
            req.evidence_ref.as_ref(),
            &op,
            &op,
            node::network::trial::INSPECT,
        )?;
        Ok(Some(ReconcileProbe::Network(Box::new(
            node::network::trial::inspect(db, &op)?,
        ))))
    } else if matches!(
        field(&task.spec, "task_type"),
        "volume.write" | "volume.append"
    ) {
        require(
            req.evidence_ref.is_none(),
            "InvalidArgument",
            "file reconciliation reads the original volume directly",
        )?;
        let action = reconcile_action(db, &task)?;
        let target = reconcile_target(db, &op, &action)?;
        Ok(Some(ReconcileProbe::VolumeFile(Box::new(
            storage::reconcile_file::Probe::new(db.cfg, target, &op, action)?,
        ))))
    } else {
        Ok(None)
    }
}

fn reconcile_action(db: &Db<'_>, task: &Doc) -> Result<Action> {
    let input = db.resolve(&task.spec["input_ref"])?;
    Action::parse(
        field(&task.spec, "task_type"),
        dependencies::tool_input(&input)?,
    )
}

fn reconcile_target(db: &Db<'_>, op: &Doc, action: &Action) -> Result<Doc> {
    let (target_id, expected) = match action {
        Action::Write(p) => (&p.id, &p.expected_resource_version),
        Action::Append(p) => (&p.target.id, &p.target.expected_resource_version),
        Action::RenameFile(p) => (&p.target.id, &p.target.expected_resource_version),
        Action::Start(p)
        | Action::Stop(p)
        | Action::ServiceStart(p)
        | Action::ServiceStop(p)
        | Action::Delete(p)
        | Action::DeleteBackup(p)
        | Action::Restore(p)
        | Action::Backup(p) => (&p.id, &p.expected_resource_version),
        Action::ServiceRestart(p) => (&p.id, &p.expected_resource_version),
        Action::NetworkTrial(p) => (&p.id, &p.expected_resource_version),
        Action::RestartApp(p) => (&p.id, &p.expected_resource_version),
        Action::ContinueOciStop(p) => (&p.target.id, &p.target.expected_resource_version),
        Action::RestoreReleaseImage(p) => (&p.target.id, &p.target.expected_resource_version),
        _ => {
            return Err(err(
                "UnsupportedCapability",
                "no postcondition verifier for this operation",
            ));
        }
    };
    let target = db.get(target_id)?;
    let held: bool = if let Action::ContinueOciStop(request) = action {
        apps::stop::cleanup_held(db, request, op)?
    } else {
        db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM holds WHERE volume=?1 AND holder=?2)",
            params![target.id, op.id],
            |r| r.get(0),
        )?
    };
    require(
        held && target.version() == *expected,
        "RecoveryRequired",
        "original exclusive hold or target revision unavailable",
    )?;
    Ok(target)
}

/// Reconcile only an independently verifiable local postcondition. Never dispatch the write again.
pub(crate) fn reconcile(
    db: &Db<'_>,
    actor: &str,
    params: &Value,
    sample: Result<Option<ReconcileSample>>,
) -> Result<Value> {
    let (req, op) = reconcile_request(db, actor, params)?;
    let t = db.of(field(&op.spec, "task_id"), "Task")?;
    let action = reconcile_action(db, &t)?;
    let target = reconcile_target(db, &op, &action)?;
    let mut reason = None;
    let (result, proof_details) = match action {
        Action::NetworkTrial(_) => {
            let (proof, artifact) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &op,
                node::network::trial::INSPECT,
            )?;
            let live = match sample? {
                Some(ReconcileSample::Network(live)) => Some(live),
                _ => None,
            };
            let (applied, result, evidence) =
                node::network::trial::reconcile(db, &op, &proof, artifact, live)?;
            if !applied {
                reason = Some(err("LeaseLost", "the original network profile is restored"));
            }
            (result, evidence)
        }
        Action::Delete(_) | Action::DeleteBackup(_) => {
            require(
                req.evidence_ref.is_none(),
                "InvalidArgument",
                "storage deletion reconciliation reads the prepared directory directly",
            )?;
            let (completed, result, evidence) = storage::retirement::reconcile(db, &op, &target)?;
            if !completed {
                reason = Some(err(
                    "LeaseLost",
                    "approved storage deletion did not take effect",
                ));
            }
            (result, evidence)
        }
        Action::Backup(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &op,
                storage::publication::INSPECT,
            )?;
            storage::publication::reconcile(db, &op, &target, &proof, &observation)?
        }
        Action::Restore(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &op,
                storage::restore_publication::INSPECT,
            )?;
            let (applied, result, evidence) =
                storage::restore_publication::reconcile(db, &op, &target, &proof, &observation)?;
            if !applied {
                reason = Some(err("LeaseLost", "restored volume was not published"));
            }
            (result, evidence)
        }
        Action::RenameFile(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &op,
                storage::file_move::INSPECT,
            )?;
            let (applied, result, evidence) =
                storage::file_move::reconcile(db, &op, &target, &proof, &observation)?;
            if !applied {
                reason = Some(err("LeaseLost", "the approved rename did not take effect"));
            }
            (result, evidence)
        }
        Action::RestartApp(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &target,
                apps::restart::INSPECT,
            )?;
            apps::restart::reconcile(db, &op, &target, &proof, &observation)?
        }
        Action::ContinueOciStop(request) => {
            let (proof, observation) =
                apps::stop::fresh_health(db, req.evidence_ref.as_ref(), &op, &target)?;
            apps::stop::reconcile_cleanup(db, &target, &op, &request, &proof, &observation)?
        }
        Action::RestoreReleaseImage(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &op,
                apps::restore_image::INSPECT,
            )?;
            apps::restore_image::reconcile(db, &op, &target, &proof, &observation)?
        }
        Action::ServiceStart(_) | Action::ServiceStop(_) | Action::ServiceRestart(_) => {
            let (proof, observation) = inspected(
                db,
                req.evidence_ref.as_ref(),
                &op,
                &target,
                "service.inspect",
            )?;
            let checkpoint =
                node::service_control::confirmed(db, &op, &target, &action, &observation)?;
            let live_manager = if let Some(checkpoint) = &checkpoint {
                let manager = match sample? {
                    Some(ReconcileSample::Service(manager)) => Some(manager),
                    _ => None,
                };
                node::service_control::require_live_manager(
                    &op,
                    checkpoint,
                    &proof,
                    &observation,
                    manager.as_ref(),
                )?
            } else {
                None
            };
            let wanted = if matches!(action, Action::ServiceStop(_)) {
                "inactive"
            } else {
                "active"
            };
            require(
                observation["unit"] == target.spec["unit"]
                    && observation["active_state"] == wanted
                    && observation["needs_reload"] == false
                    && observation["definition_digest"] == target.spec["definition_digest"],
                "OutcomeUnknown",
                "observation does not confirm this service postcondition",
            )?;
            let mut evidence = if let Action::ServiceRestart(request) = &action {
                require(
                    node::restarted(&observation, request),
                    "OutcomeUnknown",
                    "observation does not confirm a new invocation on the approved boot",
                )?;
                json!({"observation_ref":proof.reference(),"conclusion":"ObservedNewInvocation",
                    "previous_invocation_id":request.expected_invocation_id,"active_invocation_id":observation["invocation_id"]})
            } else {
                json!({"observation_ref":proof.reference(),"conclusion":"ObservedPostcondition"})
            };
            let mut result = json!({"service_ref":node::observed_effect(db,&target,&op.id,&observation)?.reference(),"observation":observation});
            if let Some(checkpoint) = checkpoint {
                evidence["checkpoint_ref"] = checkpoint.reference();
                result["checkpoint_ref"] = checkpoint.reference();
            }
            if let Some(live_manager) = live_manager {
                evidence["live_manager"] = live_manager;
            }
            (result, evidence)
        }
        Action::Start(_) | Action::Stop(_) => {
            let (proof, observation) = if matches!(action, Action::Stop(_)) {
                apps::stop::fresh_health(db, req.evidence_ref.as_ref(), &op, &target)?
            } else {
                inspected(
                    db,
                    req.evidence_ref.as_ref(),
                    &op,
                    &target,
                    "application.health",
                )?
            };
            let vol = target.spec["volume_id"]
                .as_str()
                .map(|id| db.of(id, "Volume"))
                .transpose()?;
            if let Some(v) = &vol {
                let plan = db.resolve(&op.body["plan_ref"])?;
                require(
                    plan.body["preconditions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|c| c["resource_ref"] == v.reference()),
                    "RecoveryRequired",
                    "original bound volume revision unavailable",
                )?;
            }
            let (result, conclusion) = if matches!(action, Action::Start(_)) {
                let c = apps::pending_start(db, &target)?.ok_or_else(|| {
                    err(
                        "RecoveryRequired",
                        "original OCI start checkpoint unavailable",
                    )
                })?;
                require(
                    c.body["start_operation_id"] == op.id
                        && c.body["volume_ref"] == json!(vol.as_ref().map(Doc::reference))
                        && observation["checkpoint_ref"] == c.reference()
                        && observation["engine_id"] == c.body["engine_id"]
                        && observation["container_id"] == c.body["container_id"]
                        && observation["start_operation_id"] == op.id
                        && matches!(
                            observation["effective_limits"]["source"].as_str(),
                            Some("LinuxCgroupV2" | "DockerCgroupV1")
                        )
                        && observation["process_running"] == true,
                    "OutcomeUnknown",
                    "inspection does not confirm the checkpointed runtime and resource limits",
                )?;
                (
                    apps::started(db, &target, &op.id, vol.as_ref(), &observation, true)?,
                    "StartConfirmed",
                )
            } else {
                apps::stop::original_ready(db, &target, &op, vol.as_ref())?;
                require(
                    target.body["engine_id"].is_string()
                        && observation["removal_confirmed"] == true
                        && observation["engine_id"] == target.body["engine_id"]
                        && observation["container_id"] == target.body["container_id"]
                        && observation["start_operation_id"] == target.body["start_operation_id"],
                    "OutcomeUnknown",
                    "OCI removal has no matching engine and instance evidence",
                )?;
                (
                    apps::stopped(db, &target, &op.id, vol.as_ref(), None)?,
                    "RemovalConfirmed",
                )
            };
            (
                result,
                json!({"observation_ref":proof.reference(),"conclusion":conclusion}),
            )
        }
        _ => {
            require(
                req.evidence_ref.is_none(),
                "InvalidArgument",
                "file reconciliation reads the original volume directly",
            )?;
            let Some(ReconcileSample::VolumeFile(file)) = sample? else {
                return Err(err(
                    "OutcomeUnknown",
                    "no synced file observation is available",
                ));
            };
            let (receipt, file_revision) = file.into_receipt(db.cfg, &target, &op)?;
            (
                json!({"volume_ref":db.status(&target,target.body.clone())?.reference(),"receipt":receipt}),
                json!({"observation":receipt,"file_revision":file_revision,"conclusion":"ObservedPostcondition"}),
            )
        }
    };
    let evidence=db.record("RecoveryEvidence",&t.owner,json!({"operation_ref":op.reference(),"target_ref":target.reference(),"verified_at":now(),"evidence":proof_details}))?;
    if t.spec["task_type"] == "backup.restore" {
        storage::restore_publication::queue_disposal(db, &op, &evidence)?;
    }
    let artifact = artifact::publish(
        db,
        &t.owner,
        json!({
            "task_id": t.id,
            "operation_id": op.id,
            "run_id": t.body["current_run_id"],
            "effect_ref": op.body["effect_refs"][0],
            "reconciliation_evidence_ref":evidence.reference()
        }),
        &result,
    )?;
    let state = if t.body["lifecycle"] == "Cancelled" {
        "Cancelled"
    } else if reason.is_some() {
        "Failed"
    } else {
        "Succeeded"
    };
    close(db, &t, state, "Known", reason.as_ref(), Some(&artifact))?;
    db.audit(
        actor,
        "Operation.Reconcile",
        Some(&op.id),
        if reason.is_some() {
            "ObservedNotApplied"
        } else {
            "ObservedPostcondition"
        },
    )?;
    Ok(db.get(&op.id)?.body)
}
