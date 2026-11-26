use super::*;
use crate::host::Work;
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::{Request, VERSION, id};
use serde_json::{Value, json};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn initialized(path: &Path) -> (Config, Store) {
    let c = Config::local(&path.join("core"));
    Store::init(&c).unwrap();
    let s = Store::open(c.clone()).unwrap();
    (c, s)
}
fn call(s: &mut Store, m: &str, p: Value) -> Result<Value> {
    let token = fs::read_to_string(s.config.witness_dir.join("owner.token"))?;
    call_as(s, m, p, token)
}
fn call_as(s: &mut Store, m: &str, p: Value, token: String) -> Result<Value> {
    s.request(
        rustix::process::geteuid().as_raw(),
        Request {
            schema_version: VERSION.into(),
            request_id: id("test"),
            token,
            method: m.into(),
            params: p,
        },
    )
}
fn submit(s: &mut Store, tool: &str, p: Value) -> String {
    let req = call(
        s,
        "Task.Prepare",
        json!({"tool": tool, "input": p, "delegation_id": "owner-delegation", "key": id("key")}),
    )
    .unwrap();
    call(s, "Task.Submit", req).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .into()
}
fn claim(s: &mut Store) -> String {
    s.transaction("claim", broker::claim).unwrap().unwrap()
}
fn dispatch(s: &mut Store, id: &str) -> Work {
    s.transaction("dispatch", |db| broker::dispatch(db, id))
        .unwrap()
        .unwrap()
}
fn execute(s: &mut Store, task: &str) -> Value {
    assert_eq!(claim(s), task);
    let w = dispatch(s, task);
    let r = w.perform(|event| s.transaction("checkpoint", |db| w.checkpoint(db, event)));
    s.transaction("finish", |db| broker::finish(db, &w, r))
        .unwrap();
    let t = call(s, "Task.Get", json!({"id": task})).unwrap();
    assert_eq!(t["lifecycle"], "Succeeded", "{t}");
    call(
        s,
        "Artifact.Read",
        json!({"reference": t["result"]["artifact_refs"][0]}),
    )
    .unwrap()
}
fn approve(s: &mut Store, task: &str) {
    let t = call(s, "Task.Get", json!({"id": task})).unwrap();
    let r = call(
        s,
        "Resource.Get",
        json!({"id": t["conditions"][0]["message"]}),
    )
    .unwrap();
    call(
        s,
        "Approval.Decide",
        json!({
            "id": r["reference"]["resource_id"],
            "expected_resource_version": r["reference"]["version"],
            "plan_digest": r["value"]["plan_digest"],
            "approve": true
        }),
    )
    .unwrap();
}

const HOST_WRITE_BARRIER: &str = "PSIOS_TEST_CRASH_STATE";

#[test]
fn systemd_task_releases_its_hold_after_restart_before_the_command_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let (mut config, store) = initialized(directory.path());
    drop(store);
    config.services.push(config::ServiceConfig {
        unit: "psios-recovery-fixture.service".into(),
        manager: config::Manager::User,
        allow_control: true,
        definition_digest: Some(format!("sha256:{}", "a".repeat(64))),
    });
    let mut store = Store::open(config.clone()).unwrap();
    let id = "service-psios-recovery-fixture.service";
    let service = call(&mut store, "Resource.Get", json!({"id":id})).unwrap()["reference"].clone();
    let input = json!({"id":id,"expected_resource_version":service["version"]});
    let original = submit(&mut store, "service.start", input.clone());
    approve(&mut store, &original);
    assert_eq!(claim(&mut store), original);
    let work = dispatch(&mut store, &original);
    assert_eq!(work.effect.body["phase"], "PermitBound");
    let operation = work.op.id.clone();
    drop(work);

    drop(store);
    let mut store = Store::open(config).unwrap();
    store
        .transaction("recover-fixture", broker::recover)
        .unwrap();
    let recovered = call(&mut store, "Task.Get", json!({"id":original})).unwrap();
    assert_eq!(recovered["lifecycle"], "Failed");
    assert_eq!(recovered["outcome_knowledge"], "NotStarted");
    assert_eq!(recovered["conditions"][0]["type"], "LeaseLost");
    store
        .transaction("check-hold", |db| {
            assert!(!db.c.query_row(
                "SELECT EXISTS(SELECT 1 FROM holds WHERE holder=?1)",
                [&operation],
                |r| r.get::<_, bool>(0)
            )?);
            Ok(())
        })
        .unwrap();
    let next = submit(&mut store, "service.stop", input);
    approve(&mut store, &next);
    assert_eq!(claim(&mut store), next);
    assert_ne!(dispatch(&mut store, &next).op.id, operation);
}

#[test]
fn a_root_budget_slot_is_held_through_claim_and_dispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":"local-owner","actions":["task.submit","task.read","system.status"],
            "units":2,"ttl_seconds":90,"task_concurrency":1}),
    )
    .unwrap();
    let mut submit = || {
        let admission = call(
            &mut store,
            "Task.Prepare",
            json!({"tool":"system.status","input":{},"key":id("capacity"),
                "delegation_id":grant["delegation_ref"]["resource_id"]}),
        )
        .unwrap();
        call(&mut store, "Task.Submit", admission).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = submit();
    let second = submit();
    assert_eq!(claim(&mut store), first);
    assert!(
        store
            .transaction("saturated-claim", broker::claim)
            .unwrap()
            .is_none()
    );
    store
        .transaction("release", |db| broker::release_unstarted_claim(db, &first))
        .unwrap();
    let budget = call(
        &mut store,
        "Budget.Get",
        json!({"id":grant["budget_ref"]["resource_id"]}),
    )
    .unwrap();
    assert_eq!(budget["task_concurrency_remaining"], 1);
    assert_eq!(budget["reserved"], 2);

    assert_eq!(claim(&mut store), first);
    let running = dispatch(&mut store, &first);
    assert!(
        store
            .transaction("saturated-dispatch", broker::claim)
            .unwrap()
            .is_none()
    );
    let receipt = running.perform(|_| unreachable!("system.status has no host checkpoint"));
    store
        .transaction("result", |db| broker::finish(db, &running, receipt))
        .unwrap();
    assert_eq!(claim(&mut store), second);
}

#[test]
fn oci_capture_staging_uses_approved_byte_limits_and_releases_with_the_lease() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::local(&tmp.path().join("core"));
    let budget = apps::capture::STAGING_BYTES;
    cfg.oci_capture_max_bytes = budget;
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg.clone()).unwrap();
    let node = call(&mut store, "Node.Get", json!({})).unwrap()["reference"]["resource_id"].clone();
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":"local-owner",
        "actions":["task.submit","task.read","artifact.upload",apps::capture::TOOL],
        "units":8,"ttl_seconds":90,"tool_targets":[node]}),
    )
    .unwrap();
    let producer = submit(&mut store, "system.status", json!({}));
    execute(&mut store, &producer);
    let output =
        call(&mut store, "Task.Get", json!({"id":producer})).unwrap()["result"]["artifact_refs"][0]
            ["digest"]
            .clone();

    let capture = |store: &mut Store, max_bytes: Option<u32>, requires: bool| {
        let mut input = json!({"image_id":format!("sha256:{}","a".repeat(64)),
                               "expected_engine_id":"test-local-engine"});
        if let Some(bytes) = max_bytes {
            input["max_archive_bytes"] = json!(bytes);
        }
        let mut params = json!({"tool":apps::capture::TOOL,"input":input,"key":id("capture"),
            "delegation_id":grant["delegation_ref"]["resource_id"]});
        if requires {
            params["requires"] = json!([{"task_id":producer,"digest":output}]);
        }
        let prepared = call(store, "Task.Prepare", params).unwrap();
        let accepted = call(store, "Task.Submit", prepared).unwrap();
        let task = accepted["task_id"].as_str().unwrap().to_owned();
        approve(store, &task);
        task
    };
    let reserved = |store: &mut Store| {
        let status = call(store, "Node.Get", json!({})).unwrap();
        let staging = &status["oci_capture_staging"];
        assert_eq!(staging["max_bytes"], budget);
        staging["reserved_bytes"].as_u64().unwrap()
    };
    let default_bytes = storage::oci_archive::DEFAULT_CAPTURE_BYTES;
    let first = capture(&mut store, Some(budget - default_bytes), true);
    let waiting = capture(&mut store, Some(budget), false);
    let legacy = capture(&mut store, None, false);
    assert_eq!(reserved(&mut store), 0);
    assert_eq!(claim(&mut store), first);
    assert_eq!(reserved(&mut store), u64::from(budget - default_bytes));
    assert_eq!(claim(&mut store), legacy);
    assert_eq!(reserved(&mut store), u64::from(budget));
    assert!(
        store
            .transaction("capture-budget-full", broker::claim)
            .unwrap()
            .is_none()
    );
    let check_waiting = |store: &mut Store| {
        let queued = call(store, "Task.Get", json!({"id":waiting})).unwrap();
        assert_eq!(queued["lifecycle"], "Queued");
        assert!(queued.get("current_run_id").is_none());
        store
            .transaction("check-queued-approval", |db| {
                let (attempts, bound): (u32, Option<String>) = db.c.query_row(
                "SELECT q.attempt,a.operation FROM queue q JOIN approval_bindings a ON a.task=q.task
                 WHERE q.task=?1",
                [&waiting], |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
                assert_eq!((attempts, bound), (0, None));
                Ok(())
            })
            .unwrap();
    };
    check_waiting(&mut store);
    let control = submit(&mut store, "system.status", json!({}));
    execute(&mut store, &control);
    check_waiting(&mut store);
    store
        .transaction("overbooked-capture-dispatch", |db| {
            // Inject an overbooked slot to exercise the dispatch fence independently
            // of the normal claim filter. Do not leave the injected state behind.
            db.c.execute("UPDATE queue SET phase='Claimed' WHERE task=?1", [&waiting])?;
            assert!(broker::dispatch(db, &legacy)?.is_none());
            db.c.execute("UPDATE queue SET phase='Queued' WHERE task=?1", [&waiting])?;
            let bound: Option<String> = db.c.query_row(
                "SELECT operation FROM approval_bindings WHERE task=?1",
                [&legacy],
                |row| row.get(0),
            )?;
            assert!(bound.is_none());
            Ok(())
        })
        .unwrap();
    assert_eq!(reserved(&mut store), u64::from(budget - default_bytes));
    assert_eq!(claim(&mut store), legacy);

    let cancel = |store: &mut Store, task: &str| {
        let revision =
            call(store, "Resource.Get", json!({"id":task})).unwrap()["reference"]["version"]
                .clone();
        call(
            store,
            "Task.Cancel",
            json!({"id":task,"expected_resource_version":revision}),
        )
        .unwrap()
    };
    assert_eq!(cancel(&mut store, &legacy)["lifecycle"], "Cancelled");
    assert_eq!(reserved(&mut store), u64::from(budget - default_bytes));
    let running = dispatch(&mut store, &first);
    assert_eq!(cancel(&mut store, &first)["lifecycle"], "Cancelling");
    assert_eq!(reserved(&mut store), u64::from(budget - default_bytes));
    check_waiting(&mut store);
    store
        .transaction("cancelled-capture-settlement", |db| {
            broker::finish(
                db,
                &running,
                Err(err("Fenced", "capture cancellation observed")),
            )
        })
        .unwrap();
    assert_eq!(reserved(&mut store), 0);
    assert_eq!(claim(&mut store), waiting);
    drop(dispatch(&mut store, &waiting));
    assert_eq!(reserved(&mut store), u64::from(budget));

    drop(store);
    let mut store = Store::open(cfg).unwrap();
    store
        .transaction("recover-capture-lease", broker::recover)
        .unwrap();
    assert_eq!(reserved(&mut store), 0);
    let lost = call(&mut store, "Task.Get", json!({"id":waiting})).unwrap();
    assert_eq!(lost["lifecycle"], "Failed");
    assert_eq!(lost["conditions"][0]["type"], "LeaseLost");
}

#[test]
fn storage_deletion_interrupted_before_rename_can_settle_without_replaying() {
    for prepared in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let (config, mut store) = initialized(tmp.path());
        let create = submit(
            &mut store,
            "volume.create",
            json!({"name":"delete-recovery"}),
        );
        let volume = execute(&mut store, &create);
        let deletion =
            json!({"id":volume["resource_id"],"expected_resource_version":volume["version"]});
        let directory = config
            .state_dir
            .join("volumes")
            .join(volume["resource_id"].as_str().unwrap());
        let content = directory.join("data/payload");
        fs::write(&content, b"keep until an authorized rename").unwrap();
        let task = submit(&mut store, "volume.delete", deletion.clone());
        approve(&mut store, &task);
        assert_eq!(claim(&mut store), task);
        let work = dispatch(&mut store, &task);
        if prepared {
            let interrupted = work.perform(|event| {
                store.transaction("prepare-deletion", |db| work.checkpoint(db, event))?;
                Err(err(
                    "OutcomeUnknown",
                    "test stopped before irreversible rename",
                ))
            });
            assert_eq!(interrupted.unwrap_err().0.code, "OutcomeUnknown");
        }
        drop(store);
        let mut reopened = Store::open(config.clone()).unwrap();
        reopened.transaction("restart", broker::recover).unwrap();
        let snapshot = call(&mut reopened, "Task.Get", json!({"id":task})).unwrap();
        let op = &snapshot["operation_refs"][0];
        if prepared {
            assert_eq!(snapshot["outcome_knowledge"], "Unknown");
            call(
                &mut reopened,
                "Operation.Reconcile",
                json!({"id":op["resource_id"],"expected_resource_version":op["version"]}),
            )
            .unwrap();
        }
        let settled = call(&mut reopened, "Task.Get", json!({"id":task})).unwrap();
        assert_eq!(settled["lifecycle"], "Failed");
        assert_eq!(settled["outcome_knowledge"], "Known");
        assert_eq!(settled["conditions"][0]["type"], "LeaseLost");
        assert_eq!(
            fs::read(&content).unwrap(),
            b"keep until an authorized rename"
        );
        let retirement_id = format!("storage-retirement-{}", work.op.id);
        let record = call(&mut reopened, "Resource.Get", json!({"id":retirement_id})).unwrap();
        assert_eq!(record["value"]["lifecycle"], "NotApplied");
        assert!(
            reopened
                .transaction("no-cleanup", storage::retirement::next)
                .unwrap()
                .is_none()
        );

        let retried = submit(&mut reopened, "volume.delete", deletion);
        approve(&mut reopened, &retried);
        let receipt = execute(&mut reopened, &retried);
        assert_eq!(receipt["volume_ref"]["resource_id"], volume["resource_id"]);
        assert!(!directory.exists());
        let job = reopened
            .transaction("inspect-cleanup", storage::retirement::next)
            .unwrap()
            .unwrap();
        let parent = directory.parent().unwrap();
        let retired = parent.join(
            receipt["receipt"]["retirement_ref"]["resource_id"]
                .as_str()
                .unwrap()
                .replacen("storage-retirement-", ".retired-", 1),
        );
        let protected = parent.join("fixture-original");
        fs::rename(&retired, &protected).unwrap();
        crate::config::private_dir(&retired, true).unwrap();
        fs::write(retired.join("sentinel"), b"foreign directory").unwrap();
        let refused = job.reclaim(&config).err().unwrap();
        assert_eq!(refused.0.code, "PreconditionFailed");
        assert_eq!(
            fs::read(retired.join("sentinel")).unwrap(),
            b"foreign directory"
        );
        fs::remove_file(retired.join("sentinel")).unwrap();
        fs::remove_dir(&retired).unwrap();
        fs::rename(&protected, &retired).unwrap();
        let result = job.reclaim(&config);
        reopened
            .transaction("settle-cleanup", |db| job.finish(db, result))
            .unwrap();
        assert!(!retired.exists());
    }
}

#[test]
fn elapsed_approval_is_refused_before_the_queue_sweep() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let creation = submit(&mut store, "volume.create", json!({"name":"approval-time"}));
    let source = execute(&mut store, &creation);
    let task = submit(
        &mut store,
        "volume.write",
        json!({"id":source["resource_id"], "expected_resource_version":source["version"],
            "name":"never", "content":"late"}),
    );
    let waiting = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    let request = waiting["conditions"][0]["message"].as_str().unwrap();
    store
        .transaction("elapsed-without-sweep", |db| {
            let req = db.of(request, "ApprovalRequest")?;
            assert!(!expired(req.body["expires_at"].as_str().unwrap())?);
            assert_eq!(
                db.c.execute(
                    "UPDATE authorization_deadlines SET deadline_boot_ms=0 WHERE resource=?1",
                    [&task],
                )?,
                1
            );
            Ok(())
        })
        .unwrap();
    let req = call(&mut store, "Resource.Get", json!({"id":request})).unwrap();
    let result = call(
        &mut store,
        "Approval.Decide",
        json!({"id":request,"expected_resource_version":req["reference"]["version"],
            "plan_digest":req["value"]["plan_digest"],"approve":true}),
    );
    assert_eq!(result.unwrap_err().0.code, "ApprovalInvalidated");
    assert_eq!(
        call(&mut store, "Resource.Get", json!({"id":request})).unwrap(),
        req
    );
    assert!(store.transaction("sweep", broker::claim).unwrap().is_none());
    let task = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(task["outcome_knowledge"], "NotStarted");
    assert_eq!(task["conditions"][0]["type"], "DeadlineExceeded");
}

#[test]
fn backup_export_rechecks_revocation_before_disclosing_a_prepared_read() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut store) = initialized(tmp.path());
    let creation = submit(&mut store, "volume.create", json!({"name":"export-race"}));
    let source = execute(&mut store, &creation);
    let capture = submit(
        &mut store,
        "backup.create",
        json!({"id":source["resource_id"],"expected_resource_version":source["version"]}),
    );
    let backup = execute(&mut store, &capture);
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":"local-owner","tool_targets":[backup["resource_id"]],
            "actions":[storage::export::ACTION],"units":1,"ttl_seconds":60}),
    )
    .unwrap();
    let caller = rustix::process::geteuid().as_raw();
    let request = Request {
        schema_version: VERSION.into(),
        request_id: id("export-race"),
        token: fs::read_to_string(cfg.witness_dir.join("owner.token")).unwrap(),
        method: storage::export::METHOD.into(),
        params: json!({"reference":backup,"member":{"kind":"Manifest"},"offset":0,"length":64}),
    };
    let job = store.prepare_export(caller, &request).unwrap();
    let read = job.read().unwrap();
    assert_eq!(read.next_offset, Some(64));
    call(
        &mut store,
        "Grant.Revoke",
        json!({"id":grant["grant_ref"]["resource_id"],
            "expected_resource_version":grant["grant_ref"]["version"]}),
    )
    .unwrap();
    assert_eq!(
        store
            .finish_export(caller, &request, Ok(read))
            .unwrap_err()
            .0
            .code,
        "PermissionDenied"
    );
}

fn cut_after_host_write(c: &Config) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recovery_tests::kill_after_host_write_requires_observed_postcondition",
            "--nocapture",
        ])
        .env(HOST_WRITE_BARRIER, &c.state_dir)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let limit = Instant::now() + Duration::from_secs(20);
    while !c.state_dir.join("test-at-barrier").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "crash worker exited early"
        );
        if Instant::now() > limit {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("crash worker timeout")
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn kill_after_host_write_requires_observed_postcondition() {
    if let Some(path) = std::env::var_os(HOST_WRITE_BARRIER) {
        let cfg = Config::local(Path::new(&path));
        let mut s = Store::open(cfg.clone()).unwrap();
        let task = claim(&mut s);
        let w = dispatch(&mut s, &task);
        w.perform(|event| s.transaction("checkpoint", |db| w.checkpoint(db, event)))
            .unwrap();
        fs::write(
            cfg.state_dir.join("test-at-barrier"),
            "effect issued, receipt withheld",
        )
        .unwrap();
        let mut byte = [0];
        std::io::stdin().read_exact(&mut byte).unwrap();
        panic!("parent missed crash barrier")
    }
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let create = submit(&mut s, "volume.create", json!({"name": "durable"}));
    let volume = execute(&mut s, &create);
    let task = submit(
        &mut s,
        "volume.write",
        json!({
            "id": volume["resource_id"],
            "expected_resource_version": volume["version"],
            "name": "once",
            "content": "verified"
        }),
    );
    approve(&mut s, &task);
    drop(s);
    cut_after_host_write(&c);
    let mut s = Store::open(c.clone()).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    let t = call(&mut s, "Task.Get", json!({"id": task})).unwrap();
    assert_eq!(t["lifecycle"], "Paused");
    assert_eq!(t["outcome_knowledge"], "Unknown");
    assert!(s.transaction("scheduler", broker::claim).unwrap().is_none());
    assert_eq!(
        call(
            &mut s,
            "Task.Retry",
            json!({"id":task,"key":"unsafe-retry","expected_resource_version":t["task_revision"]})
        )
        .unwrap_err()
        .0
        .code,
        "OutcomeUnknown"
    );
    let op = &t["operation_refs"][0];
    let p = json!({"id": op["resource_id"], "expected_resource_version": op["version"]});
    let file = c
        .state_dir
        .join("volumes")
        .join(volume["resource_id"].as_str().unwrap())
        .join("data/once");
    assert_eq!(fs::read_to_string(&file).unwrap(), "verified");
    fs::write(&file, "different").unwrap();
    assert_eq!(
        call(&mut s, "Operation.Reconcile", p.clone())
            .unwrap_err()
            .0
            .code,
        "OutcomeUnknown"
    );
    fs::write(&file, "verified").unwrap();
    assert_eq!(
        call(&mut s, "Operation.Reconcile", p).unwrap()["outcome_knowledge"],
        "Known"
    );
    let done = call(&mut s, "Task.Get", json!({"id": task})).unwrap();
    assert_eq!(done["lifecycle"], "Succeeded");
    let lost = call(&mut s, "Run.Get", json!({"id": done["current_run_id"]})).unwrap();
    assert_eq!(lost["lifecycle"], "Lost");
    let evidence = call(
        &mut s,
        "Artifact.Read",
        json!({"reference": done["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    assert_eq!(
        evidence["receipt"]["digest"],
        psios_api::sha256(b"verified")
    );
}

#[test]
fn interval_schedule_moves_elapsed_cutoff_by_the_original_windows() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":[schedule::ACTION,"task.submit","task.read","system.status"],
               "tool_targets":["profile-local"],"units":3,"ttl_seconds":180}),
    )
    .unwrap();
    let schedule = call(
        &mut store,
        "Schedule.Create",
        json!({"key":"elapsed-series","tool":"system.status","input":{},
               "delegation_id":grant["delegation_ref"]["resource_id"],
               "run_at":after(45),"deadline_at":after(60),
               "repeat":{"interval_seconds":30,"count":3,"missed":{"mode":"BoundedCatchUp","limit":2},"overlap":"Allow"}}),
    )
    .unwrap();
    let id = schedule["reference"]["resource_id"].as_str().unwrap();
    let original = store
        .transaction("cross-two-windows", |db| {
            let cutoff = policy::deadline::Clock::sample()?.boot.millis - 45_000;
            assert_eq!(
                db.c.execute(
                    "UPDATE authorization_deadlines SET deadline_boot_ms=?2 WHERE resource=?1",
                    rusqlite::params![id, cutoff],
                )?,
                1
            );
            Ok(cutoff)
        })
        .unwrap();
    schedule::tick(&mut store).unwrap();
    let state = call(&mut store, "Schedule.Get", json!({"id":id})).unwrap();
    assert_eq!(state["state"]["lifecycle"], "Pending");
    assert_eq!(state["state"]["next"]["ordinal"], 3);
    let references = state["state"]["trigger_refs"].as_array().unwrap();
    assert_eq!(references.len(), 2);
    for reference in references {
        let record = call(
            &mut store,
            "Schedule.TriggerGet",
            json!({"id":reference["resource_id"]}),
        )
        .unwrap();
        assert_eq!(record["state"]["lifecycle"], "Expired");
        assert!(record["state"]["task_receipt"].is_null());
    }
    let stored: i64 = store
        .transaction("next-cutoff", |db| {
            Ok(db.c.query_row(
                "SELECT deadline_boot_ms FROM authorization_deadlines WHERE resource=?1",
                [id],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(stored, original + 60_000);
}

#[test]
fn workflow_step_inherits_the_runs_elapsed_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER, "actions":[workflow::ACTION,"task.submit","task.read","system.status"],
               "tool_targets":["profile-local"],"units":1,"ttl_seconds":120}),
    )
    .unwrap();
    let definition = call(
        &mut store,
        "Workflow.Define",
        json!({"key":"elapsed-step", "steps":[{"id":"observe","tool":"system.status","input":{},"after":[]}]}),
    )
    .unwrap();
    let run = call(
        &mut store,
        "Workflow.Start",
        json!({"key":"elapsed-run","definition_ref":definition["reference"],
               "delegation_id":grant["delegation_ref"]["resource_id"],"deadline_at":after(60)}),
    )
    .unwrap();
    let id = run["reference"]["resource_id"].as_str().unwrap();
    let cap = store
        .transaction("narrow-deadline", |db| {
            let cap = policy::deadline::Clock::sample()?.boot.millis + 25_000;
            assert_eq!(
                db.c.execute(
                    "UPDATE authorization_deadlines SET deadline_boot_ms=?2 WHERE resource=?1",
                    rusqlite::params![id, cap],
                )?,
                1
            );
            Ok(cap)
        })
        .unwrap();
    workflow::tick(&mut store).unwrap();
    let observed = call(&mut store, "Workflow.Get", json!({"id":id})).unwrap();
    let task = observed["state"]["steps"][0]["task_receipt"]["task_id"]
        .as_str()
        .unwrap();
    let stored: i64 = store
        .transaction("observe-deadline", |db| {
            Ok(db.c.query_row(
                "SELECT deadline_boot_ms FROM authorization_deadlines WHERE resource=?1",
                [task],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(stored, cap);
}

#[test]
fn workflow_resumes_a_missing_artifact_and_keeps_pinned_input_after_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut store) = initialized(tmp.path());
    let creation = submit(&mut store, "volume.create", json!({"name":"mapped-result"}));
    let volume = execute(&mut store, &creation);
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":[workflow::ACTION,"task.submit","task.read","system.status","volume.write"],
                "tool_targets":["profile-local",volume["resource_id"]],"units":2,"ttl_seconds":120}),
    )
    .unwrap();
    let definition = call(
        &mut store,
        "Workflow.Define",
        json!({"key":"mapped","steps":[
            {"id":"snapshot","tool":"system.status","input":{},"after":[]},
            {"id":"record","tool":"volume.write","after":["snapshot"],
             "input":{"id":volume["resource_id"],"expected_resource_version":volume["version"],"name":"authority","content":"seed"},
             "inputs_from":{"content":{"step":"snapshot","pointer":"/authority_id"}}}]}),
    )
    .unwrap();
    assert_eq!(definition["definition"]["dsl_version"], 3);
    let run = call(
        &mut store,
        "Workflow.Start",
        json!({"key":"mapped-run","definition_ref":definition["reference"],
               "delegation_id":grant["delegation_ref"]["resource_id"],"deadline_at":after(60)}),
    )
    .unwrap();
    let lookup = json!({"id":run["reference"]["resource_id"]});
    workflow::tick(&mut store).unwrap();
    let admitted = call(&mut store, "Workflow.Get", lookup.clone()).unwrap();
    let source = admitted["state"]["steps"][0]["task_receipt"]["task_id"]
        .as_str()
        .unwrap();
    let snapshot = execute(&mut store, source);
    let source_task = call(&mut store, "Task.Get", json!({"id":source})).unwrap();
    let reference = &source_task["result"]["artifact_refs"][0];
    let artifact = reference["resource_id"].as_str().unwrap();
    let bytes: Vec<u8> = store
        .transaction("simulate-missing-artifact-chunk", |db| {
            Ok(db.c.query_row(
                "DELETE FROM artifact_chunks WHERE artifact_id=?1 AND chunk_index=0
                 RETURNING payload",
                [artifact],
                |row| row.get(0),
            )?)
        })
        .unwrap();
    let unavailable = call(
        &mut store,
        "Task.VerifyResults",
        json!({"id":source,"expected_resource_version":source_task["task_revision"]}),
    )
    .unwrap();
    assert_eq!(unavailable["lifecycle"], "Succeeded");
    assert_eq!(unavailable["result"]["availability"], "Unavailable");
    workflow::tick(&mut store).unwrap();
    workflow::tick(&mut store).unwrap();
    let waiting = call(&mut store, "Workflow.Get", lookup.clone()).unwrap();
    assert_eq!(waiting["state"]["lifecycle"], "Blocked");
    assert_eq!(waiting["state"]["reason"], "ResultUnavailable");
    assert!(waiting["state"]["steps"][1]["task_receipt"].is_null());
    let mut resume = lookup.clone();
    resume["expected_resource_version"] = waiting["reference"]["version"].clone();
    drop(store);

    let mut store = Store::open(cfg.clone()).unwrap();
    workflow::tick(&mut store).unwrap();
    assert_eq!(
        call(&mut store, "Workflow.Get", lookup.clone()).unwrap(),
        waiting
    );
    call(&mut store, "Workflow.Resume", resume.clone()).unwrap();
    workflow::tick(&mut store).unwrap();
    let blocked = call(&mut store, "Workflow.Get", lookup.clone()).unwrap();
    assert_eq!(blocked["state"]["reason"], "ResultUnavailable");
    assert!(blocked["state"]["steps"][1]["task_receipt"].is_null());
    assert_eq!(
        call(&mut store, "Workflow.Resume", resume.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    let repair = call(
        &mut store,
        "Artifact.RepairChunk",
        json!({"reference":reference,"offset":0,"data_base64":STANDARD.encode(bytes)}),
    )
    .unwrap();
    assert_eq!(repair["repaired"], true);
    let restored = call(
        &mut store,
        "Task.VerifyResults",
        json!({"id":source,"expected_resource_version":unavailable["task_revision"]}),
    )
    .unwrap();
    assert_eq!(restored["result"], source_task["result"]);
    assert_eq!(restored["operation_refs"], source_task["operation_refs"]);
    resume["expected_resource_version"] = blocked["reference"]["version"].clone();
    call(&mut store, "Workflow.Resume", resume).unwrap();
    workflow::tick(&mut store).unwrap();
    assert_eq!(
        call(&mut store, "Task.Get", json!({"id":source})).unwrap(),
        restored
    );
    let pinned = call(&mut store, "Workflow.Get", lookup.clone()).unwrap();
    let binding = &pinned["state"]["steps"][1]["input_binding"];
    assert_eq!(
        binding["sources"][0]["source_task_ref"],
        pinned["state"]["steps"][0]["task_ref"]
    );
    assert!(pinned["state"]["steps"][1]["task_receipt"].is_null());
    let materialized = call(
        &mut store,
        "Resource.Get",
        json!({"id":binding["input_ref"]["resource_id"]}),
    )
    .unwrap();
    assert_eq!(materialized["value"]["content"], snapshot["authority_id"]);
    drop(store);

    let data = cfg
        .state_dir
        .join("volumes")
        .join(volume["resource_id"].as_str().unwrap())
        .join("data/authority");
    let mut store = Store::open(cfg).unwrap();
    workflow::tick(&mut store).unwrap();
    let awaiting = call(&mut store, "Workflow.Get", lookup.clone()).unwrap();
    assert_eq!(awaiting["state"]["lifecycle"], "WaitingApproval");
    assert_eq!(awaiting["state"]["steps"][1]["input_binding"], *binding);
    let receipt = &awaiting["state"]["steps"][1]["task_receipt"];
    assert_eq!(receipt["request_digest"], binding["request_digest"]);
    let task = receipt["task_id"].as_str().unwrap();
    let pending = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    let req = call(
        &mut store,
        "Resource.Get",
        json!({"id":pending["conditions"][0]["message"]}),
    )
    .unwrap();
    let plan = call(
        &mut store,
        "Resource.Get",
        json!({"id":req["value"]["plan_ref"]["resource_id"]}),
    )
    .unwrap();
    assert_eq!(plan["value"]["input_refs"][0], binding["input_ref"]);
    assert!(!data.exists());
    approve(&mut store, task);
    execute(&mut store, task);
    workflow::tick(&mut store).unwrap();
    let completed = call(&mut store, "Workflow.Get", lookup).unwrap();
    assert_eq!(completed["state"]["lifecycle"], "Succeeded");
    assert_eq!(
        fs::read_to_string(data).unwrap(),
        snapshot["authority_id"].as_str().unwrap()
    );
}

#[test]
fn workflow_keeps_dependents_blocked_until_real_effect_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let create = submit(
        &mut s,
        "volume.create",
        json!({"name": "workflow-recovery"}),
    );
    let volume = execute(&mut s, &create);
    let grant = call(
        &mut s, "Grant.Issue",
        json!({"principal_id": policy::OWNER,
               "actions": [workflow::ACTION, "task.submit", "task.read", "volume.write", "system.status"],
               "tool_targets": [volume["resource_id"], "profile-local"], "units": 2, "ttl_seconds": 120}),
    ).unwrap();
    let definition = call(
        &mut s,
        "Workflow.Define",
        json!({"key":"real-write-then-read","steps":[
          {"id":"write","tool":"volume.write","after":[],
           "input":{"id":volume["resource_id"],"expected_resource_version":volume["version"],
                    "name":"once","content":"verified"}},
          {"id":"read","tool":"system.status","after":["write"],"input":{},
           "when":{"step":"write","pointer":"/receipt/name","equals":"once"}}
        ]}),
    )
    .unwrap();
    let run = call(
        &mut s,
        "Workflow.Start",
        json!({"key":"run","definition_ref":definition["reference"],
               "delegation_id":grant["delegation_ref"]["resource_id"],"deadline_at":after(60)}),
    )
    .unwrap();
    let lookup = json!({"id":run["reference"]["resource_id"]});
    workflow::tick(&mut s).unwrap();
    let awaiting = call(&mut s, "Workflow.Get", lookup.clone()).unwrap();
    let first = awaiting["state"]["steps"][0]["task_receipt"]["task_id"]
        .as_str()
        .unwrap();
    approve(&mut s, first);
    drop(s);
    cut_after_host_write(&c);

    let mut s = Store::open(c.clone()).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    workflow::tick(&mut s).unwrap();
    let blocked = call(&mut s, "Workflow.Get", lookup.clone()).unwrap();
    assert_eq!(blocked["state"]["lifecycle"], "Blocked");
    assert_eq!(blocked["state"]["outcome_knowledge"], "Unknown");
    assert_eq!(
        blocked["state"]["steps"][0]["task_receipt"],
        awaiting["state"]["steps"][0]["task_receipt"]
    );
    assert_eq!(blocked["state"]["steps"][1]["lifecycle"], "Pending");
    assert!(blocked["state"]["steps"][1]["condition"].is_null());
    assert!(s.transaction("claim", broker::claim).unwrap().is_none());
    let task = call(&mut s, "Task.Get", json!({"id": first})).unwrap();
    let op = &task["operation_refs"][0];
    let path = c
        .state_dir
        .join("volumes")
        .join(volume["resource_id"].as_str().unwrap())
        .join("data/once");
    assert_eq!(fs::read_to_string(&path).unwrap(), "verified");
    call(
        &mut s,
        "Operation.Reconcile",
        json!({"id":op["resource_id"],"expected_resource_version":op["version"]}),
    )
    .unwrap();
    workflow::tick(&mut s).unwrap();
    workflow::tick(&mut s).unwrap();
    let selected = call(&mut s, "Workflow.Get", lookup.clone()).unwrap();
    let evidence = selected["state"]["steps"][1]["condition"].clone();
    assert_eq!(evidence["selected"], true);
    assert!(selected["state"]["steps"][1]["task_receipt"].is_null());
    drop(s);
    let mut s = Store::open(c).unwrap();
    workflow::tick(&mut s).unwrap();
    let active = call(&mut s, "Workflow.Get", lookup.clone()).unwrap();
    assert_eq!(active["state"]["steps"][1]["condition"], evidence);
    let next = active["state"]["steps"][1]["task_receipt"]["task_id"]
        .as_str()
        .unwrap();
    let report = execute(&mut s, next);
    assert!(report["kernel_release"].is_string());
    workflow::tick(&mut s).unwrap();
    let final_run = call(&mut s, "Workflow.Get", lookup).unwrap();
    assert_eq!(final_run["state"]["lifecycle"], "Succeeded");
    assert_eq!(final_run["state"]["outcome_knowledge"], "Known");
    assert_eq!(fs::read_to_string(path).unwrap(), "verified");
    assert!(s.transaction("claim", broker::claim).unwrap().is_none());
}

#[test]
fn lost_read_gets_new_claim_and_late_receipt_is_fenced() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "system.status", json!({}));
    assert_eq!(claim(&mut s), task);
    let old = dispatch(&mut s, &task);
    let receipt = old.perform(|_| unreachable!("read has no host checkpoint"));
    drop(s);
    let mut s = Store::open(c).unwrap();
    s.transaction("recover", broker::recover).unwrap();
    let result = execute(&mut s, &task);
    assert!(result["kernel_release"].is_string());
    let before = call(&mut s, "Task.Get", json!({"id": task})).unwrap();
    assert_ne!(before["current_run_id"], old.run.id);
    s.transaction("late", |db| broker::finish(db, &old, receipt))
        .unwrap();
    assert_eq!(
        before,
        call(&mut s, "Task.Get", json!({"id": task})).unwrap()
    );
    assert_eq!(
        call(&mut s, "Resource.List", json!({"kind": "LateReceipt"})).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let run = call(&mut s, "Run.Get", json!({"id": before["current_run_id"]})).unwrap();
    assert_eq!(run["claim_epoch"], "2");
}
fn publish_oci_declaration(s: &mut Store, specification: Value) -> Value {
    let bytes =
        serde_json::to_vec(&json!({"format":"PsiOSLocalOciInstallV1","install":specification}))
            .unwrap();
    let stage = call(
        s,
        "Artifact.Stage",
        json!({
            "key":id("oci-declaration"),"size_bytes":bytes.len(),"digest":psios_api::sha256(&bytes),
            "declared_media_type":"application/vnd.psios.oci-install.v1+json","ttl_seconds":120,
        }),
    )
    .unwrap();
    call(
        s,
        "Artifact.Stage.PutChunk",
        json!({"stage_id":stage["stage_id"],
        "offset":0,"data_base64":STANDARD.encode(bytes)}),
    )
    .unwrap();
    let published = call(s, "Artifact.Publish", json!({"stage_id":stage["stage_id"]})).unwrap();
    published["artifact_ref"].clone()
}

fn accepted_artifact_install(s: &mut Store) -> (String, Value) {
    let node = call(s, "Node.Get", json!({})).unwrap();
    call(s, "Grant.Issue", json!({
        "principal_id":"local-owner","actions":["artifact.upload","artifact.read","artifact.withdraw"],
        "tool_targets":[node["reference"]["resource_id"]],"units":4,"ttl_seconds":90,
    })).unwrap();
    let reference = publish_oci_declaration(
        s,
        json!({
            "name":"recovery-test","image_id":format!("sha256:{}","d".repeat(64)),
            "release_version":"1.0.0","argv":["/probe"],"license_expression":"0BSD",
            "cpu_millicores":200,"memory_max_bytes":33554432,"pids_max":8,"ephemeral_max_bytes":1048576,
        }),
    );
    let tool = apps::artifact_install::TOOL;
    let grant = call(
        s,
        "Grant.Issue",
        json!({"principal_id":"local-owner",
        "actions":["task.submit","task.read",tool],"ttl_seconds":90,"units":2,
        "tool_targets":["profile-local",reference["resource_id"]]}),
    )
    .unwrap();
    let prepared = call(s,"Application.PrepareArtifactInstall",json!({
        "reference":reference,"key":id("install"),"delegation_id":grant["delegation_ref"]["resource_id"],
    })).unwrap();
    let accepted = call(s, "Task.Submit", prepared["request"].clone()).unwrap();
    (accepted["task_id"].as_str().unwrap().to_owned(), reference)
}

#[test]
fn interrupted_oci_artifact_inspection_releases_the_uncommitted_install_pin() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let (task, reference) = accepted_artifact_install(&mut s);
    let task = task.as_str();
    approve(&mut s, task);
    assert_eq!(claim(&mut s), task);
    let work = dispatch(&mut s, task);
    assert!(matches!(work.action, host::Action::InstallArtifact(_)));
    let unfinished = call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(unfinished["outcome_knowledge"], "InFlight");
    let withdraw = json!({"reference":reference,"reason":"recovered uncommitted install"});
    assert_eq!(
        call(&mut s, "Artifact.Withdraw", withdraw.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    drop(s);
    let mut s = Store::open(cfg).unwrap();
    s.transaction("recover", broker::recover).unwrap();
    let failed = call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(failed["lifecycle"], "Failed");
    assert_eq!(failed["outcome_knowledge"], "Known");
    assert_eq!(failed["conditions"][0]["type"], "LeaseLost");
    assert!(
        call(&mut s, "Resource.List", json!({"kind":"Application"})).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(call(&mut s, "Artifact.Withdraw", withdraw).unwrap()["withdrawal_ref"].is_object());
}

// Settle a catalog operation against controlled OCI metadata. The integration
// test inspects the real engine and runs both selected releases.
fn settle_oci_catalog(s: &mut Store, task: &str) -> Value {
    approve(s, task);
    assert_eq!(claim(s), task);
    let work = dispatch(s, task);
    let inspection = json!({"architecture":"amd64","os":"linux","layers":[],"created":now()});
    s.transaction("catalog-fixture", |db| {
        broker::finish(db, &work, Ok(inspection))
    })
    .unwrap();
    let task = call(s, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(task["lifecycle"], "Succeeded", "{task}");
    call(
        s,
        "Artifact.Read",
        json!({"reference":task["result"]["artifact_refs"][0]}),
    )
    .unwrap()
}

fn install_agent_app(s: &mut Store, agent: &Value, name: &str) -> Value {
    let token = agent["token"].as_str().unwrap();
    let allow = call(s,"Grant.Issue",json!({"principal_id":agent["principal_id"],"units":4,"ttl_seconds":90,
        "actions":["application.install","task.submit","task.read"],"tool_targets":["profile-local"],
    })).unwrap();
    let prepared = call_as(s,"Task.Prepare",json!({"tool":"application.install","key":id("install"),
        "delegation_id":allow["delegation_ref"]["resource_id"],"input":{
            "name":name,"image_id":format!("sha256:{}","e".repeat(64)),"argv":["/probe"],"release_version":"1.0.0",
            "license_expression":"0BSD","cpu_millicores":200,"memory_max_bytes":33554432,"pids_max":8,"ephemeral_max_bytes":1048576,
        },
    }),token.into()).unwrap();
    let task = call_as(s, "Task.Submit", prepared, token.into()).unwrap();
    settle_oci_catalog(s, task["task_id"].as_str().unwrap())
}

#[test]
fn publisher_verification_persists_after_revocation_and_fences_new_launches() {
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };

    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let (unsigned, source) = accepted_artifact_install(&mut s);
    let unused = call(&mut s, "Resource.Get", json!({"id":unsigned})).unwrap()["reference"].clone();
    call(
        &mut s,
        "Task.Cancel",
        json!({"id":unused["resource_id"],"expected_resource_version":unused["version"]}),
    )
    .unwrap();
    let publisher = call(&mut s, "Agent.Register", json!({"name":"local-signer"})).unwrap();
    let private = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let keys = Ed25519KeyPair::from_pkcs8(private.as_ref()).unwrap();
    let registration = call(
        &mut s,
        "Artifact.PublisherKey.Register",
        json!({"principal_id":publisher["principal_id"],
        "public_key_base64":STANDARD.encode(keys.public_key().as_ref())}),
    )
    .unwrap();
    let signing_key = &registration["reference"];
    let uploaded = call(&mut s, "Artifact.Get", json!({"reference":source})).unwrap();
    let specification = &uploaded["resource"]["spec"];
    let statement = psios_api::canonical(
        &json!({"domain":"psios/local-oci-declaration-signature/1",
            "artifact_digest":source["digest"],"declared_media_type":specification["declared_media_type"],
            "size_bytes":specification["size_bytes"],
        }),
    );
    let signature = json!({"key_ref":signing_key,"signature_base64":STANDARD.encode(keys.sign(&statement).as_ref())});
    let permit = call(
        &mut s,
        "Grant.Issue",
        json!({"principal_id":"local-owner","units":2,"ttl_seconds":90,
            "actions":["task.submit","task.read",apps::artifact_install::TOOL],
            "tool_targets":["profile-local",source["resource_id"],signing_key["resource_id"]],
        }),
    )
    .unwrap();
    let draft = call(&mut s,"Application.PrepareArtifactInstall",json!({"key":id("signed-install"),
        "reference":source,"signature":signature,"delegation_id":permit["delegation_ref"]["resource_id"],
    })).unwrap();
    let accepted = call(&mut s, "Task.Submit", draft["request"].clone()).unwrap();
    let completed = settle_oci_catalog(&mut s, accepted["task_id"].as_str().unwrap());
    let manifest = call(
        &mut s,
        "Resource.Resolve",
        json!({"reference":completed["release_ref"]}),
    )
    .unwrap();
    assert_eq!(
        manifest["value"]["publisher_identity_ref"]["resource_id"],
        publisher["principal_id"]
    );
    let specification = call(
        &mut s,
        "Resource.Resolve",
        json!({"reference":manifest["value"]["release_spec_ref"]}),
    )
    .unwrap();
    let provenance = &specification["value"]["provenance"];
    assert_eq!(provenance["source_trust"], "PublisherVerified");
    assert!(
        provenance["source_refs"]
            .as_array()
            .unwrap()
            .contains(&source)
    );
    let evidence = call(
        &mut s,
        "Resource.Resolve",
        json!({"reference":provenance["trust_evidence_or_admission_policy_ref"]}),
    )
    .unwrap();
    assert_eq!(evidence["value"]["signature"], signature);
    let app = &completed["application_ref"];
    let managed = install_agent_app(&mut s, &publisher, "recovery-test");
    let stage = call(&mut s,"Grant.Issue",json!({"principal_id":"local-owner","units":2,"ttl_seconds":90,
        "actions":["task.submit","task.read",apps::artifact_install::STAGE_TOOL],
        "tool_targets":[managed["resource_id"],source["resource_id"],signing_key["resource_id"]],
    })).unwrap();
    let target =
        json!({"id":managed["resource_id"],"expected_resource_version":managed["version"]});
    let ready = call(&mut s,"Application.PrepareArtifactRelease",json!({"key":id("signed-stage"),"reference":source,
        "target":target,"signature":signature,"delegation_id":stage["delegation_ref"]["resource_id"],
    })).unwrap();
    let staged = call(&mut s, "Task.Submit", ready["request"].clone()).unwrap();
    let staged = settle_oci_catalog(&mut s, staged["task_id"].as_str().unwrap());
    let release = call(
        &mut s,
        "Resource.Resolve",
        json!({"reference":staged["release_ref"]}),
    )
    .unwrap();
    assert_eq!(
        release["resource"]["metadata"]["owner_principal_id"],
        publisher["principal_id"]
    );
    let select = submit(
        &mut s,
        "application.select_release",
        json!({"target":target,"release_ref":staged["release_ref"]}),
    );
    approve(&mut s, &select);
    let managed = execute(&mut s, &select);
    s.transaction("authorize-signed-launch", |db| {
        for public in [app, &managed] {
            assert_eq!(
                apps::launch_spec(db, &db.resolve(public)?, true)?.image_id,
                format!("sha256:{}", "d".repeat(64))
            );
        }
        Ok(())
    })
    .unwrap();
    let select_again = submit(
        &mut s,
        "application.select_release",
        json!({"target":{"id":managed["resource_id"],"expected_resource_version":managed["version"]},
            "release_ref":staged["release_ref"]}),
    );
    approve(&mut s, &select_again);
    assert_eq!(claim(&mut s), select_again);
    let selection_work = dispatch(&mut s, &select_again);
    let selection_receipt = selection_work.perform(|_| unreachable!("no host effect"));
    let start = submit(
        &mut s,
        "application.start",
        json!({"id":app["resource_id"],"expected_resource_version":app["version"]}),
    );
    approve(&mut s, &start);
    assert_eq!(claim(&mut s), start);
    let starting = dispatch(&mut s, &start);
    call(
        &mut s,
        "Artifact.PublisherKey.Revoke",
        json!({"id":signing_key["resource_id"],
        "expected_resource_version":signing_key["version"],"reason":"publisher rollover"}),
    )
    .unwrap();
    let denied = s.transaction("check-revoked-start", |db| {
        starting.checkpoint(
            db,
            host::Checkpoint::OciStart {
                engine: "test-engine",
                container: "test-created-instance",
            },
        )
    });
    assert_eq!(denied.as_ref().unwrap_err().0.code, "PermissionDenied");
    s.transaction("settle-revoked-publisher-effects", |db| {
        assert!(db.find(&format!("oci-start-{}", starting.op.id))?.is_none());
        broker::finish(db, &starting, denied.map(|_| Value::Null))?;
        broker::finish(db, &selection_work, selection_receipt)
    })
    .unwrap();
    for task in [&start, &select_again] {
        let failed = call(&mut s, "Task.Get", json!({"id":task})).unwrap();
        assert_eq!(failed["lifecycle"], "Failed", "{failed}");
        assert_eq!(failed["outcome_knowledge"], "Known");
        assert_eq!(failed["conditions"][0]["type"], "PermissionDenied");
    }
    assert_eq!(
        call(&mut s, "Resource.Get", json!({"id":managed["resource_id"]})).unwrap()["reference"],
        managed
    );
    s.transaction("verify-revoked-signer", |db| {
        let app = db.resolve(app)?;
        assert_eq!(
            apps::launch_spec(db, &app, true)
                .expect_err("revoked signer must block launch")
                .0
                .code,
            "PermissionDenied"
        );
        assert!(apps::launch_spec(db, &app, false).is_ok());
        apps::publisher::verify(db.c)?;
        db.c.execute_batch("SAVEPOINT wrong_publisher")?;
        db.c.execute("UPDATE versions SET body=json_set(body,'$.publisher_identity_ref',json(?1)) WHERE id=?2 AND rev=1",
            rusqlite::params![db.get(policy::OWNER)?.reference().to_string(),completed["release_ref"]["resource_id"].as_str().unwrap()])?;
        assert!(apps::publisher::verify(db.c).is_err());
        db.c.execute_batch("ROLLBACK TO wrong_publisher; RELEASE wrong_publisher")?;
        apps::publisher::verify(db.c)
    })
    .unwrap();
}

#[test]
fn application_release_snapshot_survives_switch_and_retirement_between_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let (install, artifact) = accepted_artifact_install(&mut s);
    let installed = settle_oci_catalog(&mut s, &install);
    let app = &installed["application_ref"];
    let app_id = &app["resource_id"];
    let list = "Application.ListReleases";
    let first_request = json!({"application_id":app_id,"limit":1});
    assert_eq!(
        call(&mut s, list, first_request.clone())
            .unwrap_err()
            .0
            .code,
        "PermissionDenied"
    );
    let grant = |s: &mut Store, actor: &Value, tool: &str, scope: Value| {
        call(
            s,
            "Grant.Issue",
            json!({
                "principal_id":actor,"actions":["task.submit","task.read",tool],
                "tool_targets":scope,"ttl_seconds":90,"units":4,
            }),
        )
        .unwrap()
    };
    let owner = json!("local-owner");
    let access = grant(&mut s, &owner, apps::inventory::TOOL, json!([app_id]));
    let target = |application: &Value| json!({"id":application["resource_id"],"expected_resource_version":application["version"]});
    let stage = |s: &mut Store, application: &Value, version: &str| {
        let task = submit(
            s,
            "application.stage_release",
            json!({"target":target(application),"release":{
                "name":"recovery-test","image_id":format!("sha256:{}","e".repeat(64)),
                "argv":["/probe"],"release_version":version,"license_expression":"0BSD",
                "cpu_millicores":200,"memory_max_bytes":33554432,"pids_max":8,"ephemeral_max_bytes":1048576,
            }}),
        );
        settle_oci_catalog(s, &task)
    };
    let second = stage(&mut s, app, "2.0.0");
    let third = stage(&mut s, app, "3.0.0");
    let (other, _) = accepted_artifact_install(&mut s);
    let other = settle_oci_catalog(&mut s, &other);
    let other_id = &other["application_ref"]["resource_id"];
    grant(&mut s, &owner, apps::inventory::TOOL, json!([other_id]));
    let retire_tool = apps::release_retirement::TOOL;
    let retire = grant(
        &mut s,
        &owner,
        retire_tool,
        json!([app_id, second["resource_id"]]),
    );
    let first = call(&mut s, list, first_request).unwrap();
    assert_eq!(first["application_ref"], *app);
    assert_eq!(first["selected_release_ref"], installed["release_ref"]);
    assert_eq!(first["items"][0]["state"], "Selected");
    assert_eq!(first["items"][0]["source_artifact_ref"], artifact);

    let prepared = call(
        &mut s,
        "Task.Prepare",
        json!({"tool":retire_tool,"key":id("retire"),
            "delegation_id":retire["delegation_ref"]["resource_id"],
            "input":{"target":target(app),"release_ref":second,"reason":"closed candidate"},
        }),
    )
    .unwrap();
    let retired = call(&mut s, "Task.Submit", prepared).unwrap();
    let retired = retired["task_id"].as_str().unwrap();
    approve(&mut s, retired);
    let tombstone = execute(&mut s, retired);
    let choose = submit(
        &mut s,
        "application.select_release",
        json!({"target":target(app),"release_ref":third}),
    );
    approve(&mut s, &choose);
    let new_app = execute(&mut s, &choose);
    let fourth = stage(&mut s, &new_app, "4.0.0");
    drop(s);
    let mut s = Store::open(cfg).unwrap();

    let second_request = json!({"application_id":app_id,"cursor":first["next_cursor"],"limit":1});
    let page_two = call(&mut s, list, second_request.clone()).unwrap();
    assert_eq!(page_two["application_ref"], *app);
    assert_eq!(page_two["items"][0]["release_ref"], second);
    assert_eq!(page_two["items"][0]["state"], "Staged");
    assert!(page_two["items"][0]["retirement_ref"].is_null());
    let page_three = call(
        &mut s,
        list,
        json!({"application_id":app_id,"cursor":page_two["next_cursor"],"limit":1}),
    )
    .unwrap();
    assert_eq!(page_three["items"][0]["release_ref"], third);
    assert_eq!(page_three["items"][0]["state"], "Staged");
    assert_eq!(page_three["complete"], true);

    let fresh = call(&mut s, list, json!({"application_id":app_id})).unwrap();
    assert_eq!(fresh["selected_release_ref"], third);
    assert_eq!(fresh["items"][1]["retirement_ref"], tombstone);
    assert_eq!(fresh["items"][3]["release_ref"], fourth);
    let states: Vec<&str> = fresh["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["state"].as_str().unwrap())
        .collect();
    assert_eq!(states, ["Unselected", "Retired", "Selected", "Staged"]);

    assert_eq!(
        call(
            &mut s,
            list,
            json!({"application_id":other_id,"cursor":first["next_cursor"]})
        )
        .unwrap_err()
        .0
        .code,
        "CursorExpired"
    );

    call(&mut s, "Grant.Revoke", target(&access["grant_ref"])).unwrap();
    assert_eq!(
        call(&mut s, list, second_request.clone())
            .unwrap_err()
            .0
            .code,
        "PermissionDenied"
    );
    grant(&mut s, &owner, apps::inventory::TOOL, json!([app_id]));
    assert_eq!(
        call(&mut s, list, second_request).unwrap_err().0.code,
        "CursorExpired"
    );
}

#[test]
fn agent_reads_release_inventory_only_for_its_authorized_application() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let agent = call(
        &mut s,
        "Agent.Register",
        json!({"name":"release-inspector"}),
    )
    .unwrap();
    let token = agent["token"].as_str().unwrap();
    let agent_call = |s: &mut Store, method: &str, params| call_as(s, method, params, token.into());
    let app = install_agent_app(&mut s, &agent, "agent-app");
    let input = json!({"application_id":app["resource_id"]});
    let (other, _) = accepted_artifact_install(&mut s);
    let other = settle_oci_catalog(&mut s, &other);
    let foreign = json!({"application_id":other["application_ref"]["resource_id"]});
    let tool = apps::inventory::TOOL;
    let viewer = call(&mut s,"Grant.Issue",json!({"principal_id":agent["principal_id"],"units":4,"ttl_seconds":90,
        "actions":[tool,"task.submit","task.read"],"tool_targets":[input["application_id"],foreign["application_id"]],
    })).unwrap();
    let capabilities = agent_call(&mut s, "Capability.Discover", json!({})).unwrap();
    assert!(
        capabilities
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| cap["capability_id"] == tool && cap["availability"] == "Ready")
    );
    let list = "Application.ListReleases";
    let direct = agent_call(&mut s, list, input.clone()).unwrap();
    assert_eq!(direct["items"][0]["state"], "Selected");
    assert_eq!(
        agent_call(&mut s, list, foreign.clone())
            .unwrap_err()
            .0
            .code,
        "PermissionDenied"
    );

    let prepare = |s: &mut Store, input| {
        agent_call(s,"Task.Prepare",json!({
        "key":id("read-release"),"tool":tool,"input":input,"delegation_id":viewer["delegation_ref"]["resource_id"],
    })).unwrap()
    };
    let denied = prepare(&mut s, foreign);
    assert_eq!(
        agent_call(&mut s, "Task.Submit", denied)
            .unwrap_err()
            .0
            .code,
        "PermissionDenied"
    );
    let request = prepare(&mut s, input.clone());
    let receipt = agent_call(&mut s, "Task.Submit", request).unwrap();
    let task = receipt["task_id"].as_str().unwrap();
    assert_eq!(execute(&mut s, task), direct);
    let result = agent_call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    let value = agent_call(
        &mut s,
        "Artifact.Read",
        json!({"reference":result["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    assert_eq!(value, direct);

    let request = prepare(&mut s, input);
    let receipt = agent_call(&mut s, "Task.Submit", request).unwrap();
    let task = receipt["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut s), task);
    let read = dispatch(&mut s, task);
    let sampled = read
        .perform(|_| unreachable!("inventory is a pinned catalog read"))
        .unwrap();
    call(
        &mut s,
        "Grant.Revoke",
        json!({"id":viewer["grant_ref"]["resource_id"],
            "expected_resource_version":viewer["grant_ref"]["version"],
        }),
    )
    .unwrap();
    s.transaction("revoked-inventory-read", |db| {
        broker::finish(db, &read, Ok(sampled))
    })
    .unwrap();
    let result = agent_call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(result["lifecycle"], "Failed");
    assert_eq!(result["conditions"][0]["type"], "PermissionDenied");
    assert_eq!(result["result"]["artifact_refs"], json!([]));
}

#[test]
fn artifact_release_stage_rolls_back_failed_receipts_and_checks_the_selected_source() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let (install, _) = accepted_artifact_install(&mut s);
    let installed = settle_oci_catalog(&mut s, &install);
    let app = &installed["application_ref"];
    let target = |reference: &Value| json!({"id":reference["resource_id"],"expected_resource_version":reference["version"]});
    let bundle = publish_oci_declaration(
        &mut s,
        json!({
            "name":"recovery-test","image_id":format!("sha256:{}","e".repeat(64)),
            "release_version":"2.1.0","argv":["/probe"],"license_expression":"0BSD",
            "cpu_millicores":200,"memory_max_bytes":33554432,"pids_max":8,"ephemeral_max_bytes":1048576,
        }),
    );
    let issue = |s: &mut Store, tool_targets: Value| {
        call(s,"Grant.Issue",json!({
        "principal_id":"local-owner","actions":["task.submit","task.read",apps::artifact_install::STAGE_TOOL],
        "tool_targets":tool_targets,"ttl_seconds":90,"units":4,
    })).unwrap()
    };
    let prepare = |s: &mut Store, grant: &Value| {
        call(
            s,
            "Application.PrepareArtifactRelease",
            json!({
                "target":target(app),"reference":bundle,"key":id("release"),"delegation_id":grant["delegation_ref"]["resource_id"],
            }),
        )
    };
    let incomplete = issue(&mut s, json!([app["resource_id"]]));
    assert_eq!(
        prepare(&mut s, &incomplete).unwrap_err().0.code,
        "PermissionDenied"
    );
    let permit = issue(&mut s, json!([app["resource_id"], bundle["resource_id"]]));
    let preview = prepare(&mut s, &permit).unwrap();
    assert_eq!(preview["resource_refs"], json!([app, bundle]));
    assert_eq!(preview["release"]["release_version"], "2.1.0");
    assert_eq!(preview["selected_release_ref"], installed["release_ref"]);

    let task = call(&mut s, "Task.Submit", preview["request"].clone()).unwrap();
    let task = task["task_id"].as_str().unwrap();
    let withdrawal = json!({"reference":bundle,"reason":"stage finished"});
    assert_eq!(
        call(&mut s, "Artifact.Withdraw", withdrawal.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    approve(&mut s, task);
    assert_eq!(claim(&mut s), task);
    let work = dispatch(&mut s, task);
    s.transaction("inject-catalog-abort",|db| {
        db.c.execute_batch("CREATE TRIGGER test_stage_receipt_abort BEFORE INSERT ON versions
            WHEN NEW.kind='ApplicationArtifactReleaseStage' BEGIN SELECT raise(ABORT,'withhold stage receipt'); END;")?;
        Ok(())
    }).unwrap();
    let observation = json!({"architecture":"amd64","os":"linux","layers":[],"created":now()});
    assert_eq!(
        s.transaction("reject-catalog", |db| broker::finish(
            db,
            &work,
            Ok(observation)
        ))
        .unwrap_err()
        .0
        .code,
        "StorageUnavailable"
    );
    s.transaction("record-catalog-failure", |db| {
        db.c.execute_batch("DROP TRIGGER test_stage_receipt_abort")?;
        broker::settlement_failed(db, &work)?;
        assert!(!apps::artifact_install::pinned(
            db,
            bundle["resource_id"].as_str().unwrap()
        )?);
        Ok(())
    })
    .unwrap();
    let failed = call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(failed["lifecycle"], "Failed");
    assert_eq!(failed["outcome_knowledge"], "Known");
    assert_eq!(
        call(
            &mut s,
            "Resource.List",
            json!({"kind":"ApplicationRelease"})
        )
        .unwrap()["items"],
        json!([installed["release_ref"]])
    );

    let ready = prepare(&mut s, &permit).unwrap();
    let stage_task = call(&mut s, "Task.Submit", ready["request"].clone()).unwrap();
    let settled = settle_oci_catalog(&mut s, stage_task["task_id"].as_str().unwrap());
    let stage = call(
        &mut s,
        "Resource.Resolve",
        json!({"reference":settled["stage_ref"]}),
    )
    .unwrap();
    assert_eq!(stage["value"]["source_artifact_ref"], bundle);
    assert_eq!(stage["value"]["application_ref"], *app);
    assert_eq!(
        call(&mut s, "Resource.Get", json!({"id":app["resource_id"]})).unwrap()["reference"],
        *app
    );
    assert_eq!(
        call(&mut s, "Artifact.Withdraw", withdrawal)
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    s.transaction("verify-source-pins", |db| {
        apps::artifact_install::verify(db.c)
    })
    .unwrap();

    let selection = submit(
        &mut s,
        "application.select_release",
        json!({"target":target(app),"release_ref":settled["release_ref"]}),
    );
    approve(&mut s, &selection);
    let selected = execute(&mut s, &selection);
    let start = submit(&mut s, "application.start", target(&selected));
    approve(&mut s, &start);
    s.transaction("damage-selected-source", |db| {
        db.c.execute(
            "UPDATE artifact_chunks SET payload=zeroblob(length(payload)) WHERE artifact_id=?1",
            [bundle["resource_id"].as_str().unwrap()],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(claim(&mut s), start);
    let corrupt = s
        .transaction("check-selected-source", |db| broker::dispatch(db, &start))
        .err()
        .expect("corrupt source must prevent host dispatch");
    assert_eq!(corrupt.0.code, "RecoveryRequired");
    s.transaction("settle-damaged-source", |db| {
        broker::dispatch_failed(db, &start, &corrupt)
    })
    .unwrap();
    let result = call(&mut s, "Task.Get", json!({"id":start})).unwrap();
    assert_eq!(result["lifecycle"], "Failed");
    assert_eq!(result["outcome_knowledge"], "NotStarted");
}

#[test]
fn retired_release_fences_approved_rollback_and_releases_its_catalog_pin() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let (install, source) = accepted_artifact_install(&mut s);
    let installed = settle_oci_catalog(&mut s, &install);
    let old = &installed["release_ref"];
    let app = &installed["application_ref"];
    let target =
        |app: &Value| json!({"id":app["resource_id"],"expected_resource_version":app["version"]});
    let retire = apps::release_retirement::TOOL;
    let replacement_grant = call(
        &mut s,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":["task.submit","task.read",retire],
            "tool_targets":[app["resource_id"],old["resource_id"]],"ttl_seconds":90,"units":4,
        }),
    )
    .unwrap();
    let delegation = &replacement_grant["delegation_ref"]["resource_id"];
    let request_retirement =
        |s: &mut Store, app: &Value, delegation: &Value| {
            call(s,"Task.Prepare",json!({
        "tool":retire,"key":id("retire"),"delegation_id":delegation,
        "input":{"target":target(app),"release_ref":old,"reason":"superseded installation"},
    })).unwrap()
        };
    let selected = request_retirement(&mut s, app, delegation);
    assert_eq!(
        call(&mut s, "Task.Submit", selected).unwrap_err().0.code,
        "Conflict"
    );

    let stage = submit(
        &mut s,
        "application.stage_release",
        json!({"target":target(app),"release":{
            "name":"recovery-test","image_id":format!("sha256:{}","e".repeat(64)),"argv":["/probe"],
            "release_version":"2.0.0","license_expression":"0BSD","cpu_millicores":200,
            "memory_max_bytes":33554432,"pids_max":8,"ephemeral_max_bytes":1048576,
        }}),
    );
    let new = settle_oci_catalog(&mut s, &stage);
    let selection = submit(
        &mut s,
        "application.select_release",
        json!({"target":target(app),"release_ref":new}),
    );
    approve(&mut s, &selection);
    let app = execute(&mut s, &selection);
    let rollback = submit(
        &mut s,
        "application.select_release",
        json!({"target":target(&app),"release_ref":old}),
    );
    let withdraw = json!({"reference":source,"reason":"release audit retained"});
    assert_eq!(
        call(&mut s, "Artifact.Withdraw", withdraw.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );

    let restricted = call(
        &mut s,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":["task.submit","task.read",retire],
            "tool_targets":[app["resource_id"]],"ttl_seconds":90,"units":1,
        }),
    )
    .unwrap();
    let only_app = request_retirement(&mut s, &app, &restricted["delegation_ref"]["resource_id"]);
    assert_eq!(
        call(&mut s, "Task.Submit", only_app).unwrap_err().0.code,
        "PermissionDenied"
    );

    let request = request_retirement(&mut s, &app, delegation);
    let interrupted = call(&mut s, "Task.Submit", request).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    approve(&mut s, &interrupted);
    assert_eq!(claim(&mut s), interrupted);
    let stale = dispatch(&mut s, &interrupted);
    drop(s);
    let mut s = Store::open(cfg.clone()).unwrap();
    s.transaction("recover-catalog", broker::recover).unwrap();
    let lost = call(&mut s, "Task.Get", json!({"id":interrupted})).unwrap();
    assert_eq!(lost["lifecycle"], "Failed");
    assert_eq!(lost["outcome_knowledge"], "Known");
    s.transaction("late-catalog", |db| {
        broker::finish(db, &stale, Ok(json!({})))
    })
    .unwrap();
    assert_eq!(
        call(&mut s, "Artifact.Withdraw", withdraw.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );

    let request = request_retirement(&mut s, &app, delegation);
    let accepted = call(&mut s, "Task.Submit", request.clone()).unwrap();
    let retirement = accepted["task_id"].as_str().unwrap();
    approve(&mut s, retirement);
    let receipt = execute(&mut s, retirement);
    let replay = call(&mut s, "Task.Submit", request).unwrap();
    assert_eq!(replay["task_id"], retirement);
    assert_eq!(replay["deduplicated"], true);
    let original = call(&mut s, "Resource.Resolve", json!({"reference":old})).unwrap();
    assert_eq!(original["reference"], *old);
    let retained = call(&mut s, "Resource.Resolve", json!({"reference":receipt})).unwrap();
    assert_eq!(retained["value"]["released_artifact_ref"], source);

    approve(&mut s, &rollback);
    assert_eq!(claim(&mut s), rollback);
    let rejected = s
        .transaction("dispatch-retired-rollback", |db| {
            broker::dispatch(db, &rollback)
        })
        .err()
        .expect("dispatch must fence the retired rollback");
    s.transaction("reject-retired-rollback", |db| {
        broker::dispatch_failed(db, &rollback, &rejected)
    })
    .unwrap();
    let refused = call(&mut s, "Task.Get", json!({"id":rollback})).unwrap();
    assert_eq!(refused["lifecycle"], "Failed");
    assert_eq!(refused["outcome_knowledge"], "NotStarted");
    assert_eq!(refused["conditions"][0]["type"], "PreconditionFailed");
    assert!(call(&mut s, "Artifact.Withdraw", withdraw).unwrap()["withdrawal_ref"].is_object());
    drop(s);
    let mut s = Store::open(cfg).unwrap();
    s.transaction("verify-retirement", |db| {
        apps::artifact_install::verify(db.c)?;
        apps::release_retirement::verify(db.c)
    })
    .unwrap();
    assert_eq!(
        call(&mut s, "Resource.Get", json!({"id":app["resource_id"]})).unwrap()["resource"]["spec"]
            ["release_ref"],
        new
    );
}

#[test]
fn revoked_process_observation_cannot_publish_an_artifact_or_a_late_body() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let pid = std::process::id();
    let list = submit(
        &mut store,
        "node.processes",
        json!({"after_pid":pid-1,"limit":1}),
    );
    let page = execute(&mut store, &list);
    let process = page["items"][0]["process_ref"].clone();
    assert_eq!(process["pid"], pid);
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":"local-owner","actions":["task.submit","task.read",node::process::TOOL],
            "tool_targets":[process["node_id"]],"units":2,"ttl_seconds":90}),
    ).unwrap();
    let request = call(
        &mut store,
        "Task.Prepare",
        json!({"tool":node::process::TOOL,"input":process,"key":id("inspect"),
            "delegation_id":grant["delegation_ref"]["resource_id"]}),
    )
    .unwrap();
    let submission = call(&mut store, "Task.Submit", request).unwrap();
    let task = submission["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    let observation = work
        .perform(|_| unreachable!("observations have no effect checkpoint"))
        .unwrap();
    assert_eq!(observation["process_ref"], process);
    call(
        &mut store,
        "Grant.Revoke",
        json!({"id":grant["grant_ref"]["resource_id"],
        "expected_resource_version":grant["grant_ref"]["version"]}),
    )
    .unwrap();
    store
        .transaction("finish", |db| {
            broker::finish(db, &work, Ok(observation.clone()))
        })
        .unwrap();
    let closed = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(closed["lifecycle"], "Failed");
    assert_eq!(closed["conditions"][0]["type"], "PermissionDenied");
    assert!(
        closed["result"]["artifact_refs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    store
        .transaction("late", |db| broker::finish(db, &work, Ok(observation)))
        .unwrap();
    let receipts = call(&mut store, "Resource.List", json!({"kind":"LateReceipt"})).unwrap();
    let late = call(
        &mut store,
        "Resource.Resolve",
        json!({"reference":receipts["items"][0]}),
    )
    .unwrap();
    assert_eq!(late["value"]["evidence"], json!({"code":"Fenced"}));
}

#[test]
fn cancellation_after_dispatch_preserves_successful_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "volume.create", json!({"name": "cancel-race"}));
    assert_eq!(claim(&mut s), task);
    let w = dispatch(&mut s, &task);
    let pending = call(
        &mut s,
        "Task.Cancel",
        json!({"id": task, "expected_resource_version": w.task.version()}),
    )
    .unwrap();
    assert_eq!(pending["lifecycle"], "Cancelling");
    let receipt = w.perform(|_| unreachable!("write has no host checkpoint"));
    s.transaction("finish", |db| broker::finish(db, &w, receipt))
        .unwrap();
    let done = call(&mut s, "Task.Get", json!({"id": task})).unwrap();
    assert_eq!(done["lifecycle"], "Cancelled");
    assert_eq!(done["outcome_knowledge"], "Known");
    assert_eq!(done["result"]["availability"], "Available");
    assert_eq!(
        call(
            &mut s,
            "Task.Retry",
            json!({"id": task, "expected_resource_version": done["task_revision"], "key": "other"})
        )
        .unwrap_err()
        .0
        .code,
        "PreconditionFailed"
    );
}
#[test]
fn interruption_before_dispatch_releases_no_duplicate_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "volume.create", json!({"name": "safe"}));
    assert_eq!(claim(&mut s), task);
    drop(s);
    let mut s = Store::open(c).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    execute(&mut s, &task);
    let t = call(&mut s, "Resource.Get", json!({"id": task})).unwrap();
    let budget = call(
        &mut s,
        "Budget.Get",
        json!({"id": t["resource"]["spec"]["budget_ref"]["resource_id"]}),
    )
    .unwrap();
    assert_eq!(budget["reserved"], 0);
    assert_eq!(budget["spent"], 1);
}
#[test]
fn commit_witness_recovers_only_adjacent_unacknowledged_transactions() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, s) = initialized(tmp.path());
    drop(s);
    let path = c.witness_dir.join("head");
    let stable: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    for pending in [
        json!({"committed": stable["committed"], "pending": "next"}),
        json!({"committed": "previous", "pending": stable["committed"]}),
    ] {
        fs::write(&path, serde_json::to_vec(&pending).unwrap()).unwrap();
        drop(Store::open(c.clone()).unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            stable
        );
    }
    fs::write(&path, b"{\"committed\":\"older\",\"pending\":\"other\"}").unwrap();
    assert!(matches!(Store::open(c),Err(e) if e.0.code=="RecoveryRequired"));
}

#[test]
fn queue_full_fails_atomically_and_revocation_between_claim_and_dispatch_is_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::local(&tmp.path().join("core"));
    cfg.queue_limit = 1;
    Store::init(&cfg).unwrap();
    let mut s = Store::open(cfg).unwrap();
    let agent = call(&mut s, "Agent.Register", json!({"name": "gated"})).unwrap();
    let grant = call(
        &mut s,
        "Grant.Issue",
        json!({
            "principal_id": agent["principal_id"],
            "actions": ["task.submit","system.status","task.read"],
            "units": 1,
            "ttl_seconds": 600
        }),
    )
    .unwrap();
    let as_agent = |s: &mut Store, m: &str, p: Value| {
        s.request(
            rustix::process::geteuid().as_raw(),
            Request {
                schema_version: VERSION.into(),
                request_id: id("test"),
                token: agent["token"].as_str().unwrap().into(),
                method: m.into(),
                params: p,
            },
        )
    };
    let request = as_agent(
        &mut s,
        "Task.Prepare",
        json!({
            "tool": "system.status",
            "input": {},
            "key": "authorized",
            "delegation_id": grant["delegation_ref"]["resource_id"]
        }),
    )
    .unwrap();
    let receipt = as_agent(&mut s, "Task.Submit", request.clone()).unwrap();
    let other = call(
        &mut s,
        "Task.Prepare",
        json!({
            "tool": "system.status",
            "input": {},
            "key": "overflow",
            "delegation_id": "owner-delegation"
        }),
    )
    .unwrap();
    assert_eq!(
        call(&mut s, "Task.Submit", other.clone())
            .unwrap_err()
            .0
            .code,
        "QueueFull"
    );
    assert_eq!(
        call(&mut s, "Task.LookupSubmission", other).unwrap()["status"],
        "AbsentWithinWindow"
    );
    let t = claim(&mut s);
    assert_eq!(receipt["task_id"], t);
    call(
        &mut s,
        "Grant.Revoke",
        json!({
            "id": grant["grant_ref"]["resource_id"],
            "expected_resource_version": grant["grant_ref"]["version"]
        }),
    )
    .unwrap();
    let denied = s
        .transaction("dispatch", |db| broker::dispatch(db, &t))
        .err()
        .unwrap();
    assert_eq!(denied.0.code, "PermissionDenied");
    s.transaction("close", |db| broker::dispatch_failed(db, &t, &denied))
        .unwrap();
    assert_eq!(
        call(&mut s, "Task.Get", json!({"id": t})).unwrap()["outcome_knowledge"],
        "NotStarted"
    );
    let b = call(
        &mut s,
        "Budget.Get",
        json!({"id": grant["budget_ref"]["resource_id"]}),
    )
    .unwrap();
    assert_eq!(b["reserved"], 0);
    assert_eq!(b["spent"], 0);
    assert_eq!(
        as_agent(&mut s, "Task.Submit", request).unwrap_err().0.code,
        "PermissionDenied"
    );
}

#[test]
fn chunk_receipt_loss_requires_exact_range_and_retains_exclusive_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let create = submit(&mut s, "volume.create", json!({"name":"chunked"}));
    let v = execute(&mut s, &create);
    let target = json!({"id":v["resource_id"],"expected_resource_version":v["version"]});
    let first = submit(
        &mut s,
        "volume.append",
        json!({"target":target,"path":"data","offset":0,"data_base64":"AP8="}),
    );
    approve(&mut s, &first);
    let r = execute(&mut s, &first);
    let target =
        json!({"id":v["resource_id"],"expected_resource_version":r["volume_ref"]["version"]});
    let next = submit(
        &mut s,
        "volume.append",
        json!({"target":target,"path":"data","offset":2,"data_base64":"AQI="}),
    );
    approve(&mut s, &next);
    assert_eq!(claim(&mut s), next);
    let work = dispatch(&mut s, &next);
    work.perform(|_| unreachable!("write has no host checkpoint"))
        .unwrap();
    drop(s);
    let mut restarted = Store::open(c.clone()).unwrap();
    restarted.transaction("restart", broker::recover).unwrap();
    let task = call(&mut restarted, "Task.Get", json!({"id":next})).unwrap();
    assert_eq!(task["outcome_knowledge"], "Unknown");
    let op = &task["operation_refs"][0];
    let args = json!({"id":op["resource_id"],"expected_resource_version":op["version"]});
    let path = c
        .state_dir
        .join("volumes")
        .join(v["resource_id"].as_str().unwrap())
        .join("data/data");
    assert_eq!(fs::read(&path).unwrap(), [0, 255, 1, 2]);
    fs::write(&path, [0, 255, 1]).unwrap();
    assert_eq!(
        call(&mut restarted, "Operation.Reconcile", args.clone())
            .unwrap_err()
            .0
            .code,
        "OutcomeUnknown"
    );
    fs::write(&path, [0, 255, 2, 1]).unwrap();
    assert_eq!(
        call(&mut restarted, "Operation.Reconcile", args.clone())
            .unwrap_err()
            .0
            .code,
        "OutcomeUnknown"
    );
    fs::write(&path, [0, 255, 1, 2]).unwrap();
    assert_eq!(
        call(&mut restarted, "Operation.Reconcile", args).unwrap()["outcome_knowledge"],
        "Known"
    );
    assert_eq!(fs::read(path).unwrap(), [0, 255, 1, 2]);
}

#[test]
fn fair_claims_do_not_starve_a_second_principal_behind_an_owner_backlog() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let agent = call(&mut s, "Agent.Register", json!({"name":"second"})).unwrap();
    let grant=call(&mut s,"Grant.Issue",json!({"principal_id":agent["principal_id"],"actions":["task.submit","task.read","system.status"],
        "units":2,"ttl_seconds":300})).unwrap();
    let first = submit(&mut s, "system.status", json!({}));
    for _ in 0..4 {
        submit(&mut s, "system.status", json!({}));
    }
    let as_agent = |s: &mut Store, method: &str, params: Value| {
        s.request(
            rustix::process::geteuid().as_raw(),
            Request {
                schema_version: VERSION.into(),
                request_id: id("test"),
                method: method.into(),
                token: agent["token"].as_str().unwrap().into(),
                params,
            },
        )
        .unwrap()
    };
    let req = as_agent(
        &mut s,
        "Task.Prepare",
        json!({"tool":"system.status","input":{},"key":"one","delegation_id":grant["delegation_ref"]["resource_id"]}),
    );
    let second = as_agent(&mut s, "Task.Submit", req);
    assert_eq!(claim(&mut s), first);
    assert_eq!(claim(&mut s), second["task_id"].as_str().unwrap());
}

#[test]
fn backup_check_excludes_restore_and_retries_after_lost_receipt() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "volume.create", json!({"name":"archive"}));
    let volume = execute(&mut s, &task);
    let task = submit(
        &mut s,
        "backup.create",
        json!({"id":volume["resource_id"],"expected_resource_version":volume["version"]}),
    );
    let backup = execute(&mut s, &task);
    let target = json!({"id":backup["resource_id"],"expected_resource_version":backup["version"]});
    let grant = call(
        &mut s,
        "Grant.Issue",
        json!({"principal_id":"local-owner","actions":["backup.verify","task.submit","task.read"],
        "tool_targets":[backup["resource_id"]],"ttl_seconds":120,"units":4}),
    )
    .unwrap();
    let request = call(
        &mut s,
        "Task.Prepare",
        json!({"tool":"backup.verify","input":target,
        "delegation_id":grant["delegation_ref"]["resource_id"],"key":"integrity"}),
    )
    .unwrap();
    let task = call(&mut s, "Task.Submit", request).unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(claim(&mut s), task);
    let worker = dispatch(&mut s, &task);
    let reading = worker.perform(|_| unreachable!()).unwrap();
    let competing = submit(&mut s, "backup.restore", target.clone());
    assert_eq!(claim(&mut s), competing);
    let conflict = s
        .transaction("restore", |db| broker::dispatch(db, &competing))
        .err()
        .unwrap();
    assert_eq!(conflict.0.code, "Conflict");
    s.transaction("close", |db| {
        broker::dispatch_failed(db, &competing, &conflict)
    })
    .unwrap();

    drop(s);
    let mut s = Store::open(cfg).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    let replay = execute(&mut s, &task);
    assert_eq!(replay["backup_ref"], backup);
    assert_eq!(replay["manifest_digest"], reading["manifest_digest"]);
    let restore = submit(&mut s, "backup.restore", target);
    assert!(
        execute(&mut s, &restore)["resource_id"]
            .as_str()
            .unwrap()
            .starts_with("volume-")
    );
}

#[test]
fn restored_volume_cannot_publish_after_pool_revision_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(tmp.path());
    let create = submit(&mut store, "volume.create", json!({"name":"restore-fence"}));
    let source = execute(&mut store, &create);
    let backup = submit(
        &mut store,
        "backup.create",
        json!({"id":source["resource_id"],"expected_resource_version":source["version"]}),
    );
    let backup = execute(&mut store, &backup);
    let restore = submit(
        &mut store,
        "backup.restore",
        json!({"id":backup["resource_id"],"expected_resource_version":backup["version"]}),
    );
    assert_eq!(claim(&mut store), restore);
    let work = dispatch(&mut store, &restore);
    let refused = work.perform(|event| {
        let pool = call(&mut store, "Resource.Get", json!({"id":"local"}))?;
        call(&mut store, "StoragePool.Configure", json!({
            "id":"local","expected_resource_version":pool["reference"]["version"],"allocation_enabled":false,
        }))?;
        store.transaction("restore-checkpoint", |db| work.checkpoint(db, event))
    });
    assert_eq!(refused.as_ref().unwrap_err().0.code, "Fenced");
    store
        .transaction("restore-refusal", |db| broker::finish(db, &work, refused))
        .unwrap();
    let task = call(&mut store, "Task.Get", json!({"id":restore})).unwrap();
    assert_eq!(task["lifecycle"], "Failed");
    assert_eq!(task["outcome_knowledge"], "Known");
    assert_eq!(
        fs::read_dir(config.state_dir.join("volumes"))
            .unwrap()
            .count(),
        1
    );
    let original = call(
        &mut store,
        "Resource.Get",
        json!({"id":backup["resource_id"]}),
    )
    .unwrap();
    assert_eq!(original["reference"], backup);
}

#[test]
fn activation_requires_a_completed_authority_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let create = submit(&mut s, "volume.create", json!({"name":"restore-source"}));
    let source = execute(&mut s, &create);
    let backup = submit(
        &mut s,
        "backup.create",
        json!({"id":source["resource_id"],"expected_resource_version":source["version"]}),
    );
    let backup = execute(&mut s, &backup);
    let restore = submit(
        &mut s,
        "backup.restore",
        json!({"id":backup["resource_id"],"expected_resource_version":backup["version"]}),
    );
    let clone = execute(&mut s, &restore);
    let target = json!({"id":clone["resource_id"],"expected_resource_version":clone["version"]});
    let first = submit(&mut s, "volume.activate", target.clone());
    approve(&mut s, &first);
    assert_eq!(claim(&mut s), first);
    let worker = dispatch(&mut s, &first);
    let receipt = worker.perform(|_| unreachable!()).unwrap();
    assert!(receipt["verified_at"].is_string());
    drop(s);

    let mut s = Store::open(cfg).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    let interrupted = call(&mut s, "Task.Get", json!({"id":first})).unwrap();
    assert_eq!(interrupted["lifecycle"], "Failed");
    assert_eq!(interrupted["outcome_knowledge"], "Known");
    assert_eq!(interrupted["conditions"][0]["type"], "LeaseLost");
    let pending = call(&mut s, "Resource.Get", json!({"id":clone["resource_id"]})).unwrap();
    assert_eq!(pending["reference"], clone);
    assert_eq!(
        pending["resource"]["spec"]["storage_restore_phase"],
        "AwaitingActivation"
    );

    let second = submit(&mut s, "volume.activate", target.clone());
    approve(&mut s, &second);
    assert_eq!(claim(&mut s), second);
    let worker = dispatch(&mut s, &second);
    let running = call(&mut s, "Task.Get", json!({"id":second})).unwrap();
    call(
        &mut s,
        "Task.Cancel",
        json!({"id":second,"expected_resource_version":running["task_revision"]}),
    )
    .unwrap();
    let receipt = worker.perform(|_| unreachable!());
    s.transaction("finish", |db| broker::finish(db, &worker, receipt))
        .unwrap();
    let canceled = call(&mut s, "Task.Get", json!({"id":second})).unwrap();
    assert_eq!(canceled["lifecycle"], "Cancelled");
    assert_eq!(canceled["outcome_knowledge"], "Known");
    assert_eq!(canceled["result"]["artifact_refs"], json!([]));
    let pending = call(&mut s, "Resource.Get", json!({"id":clone["resource_id"]})).unwrap();
    assert_eq!(pending["reference"], clone);

    let third = submit(&mut s, "volume.activate", target);
    approve(&mut s, &third);
    let activated = execute(&mut s, &third);
    assert_ne!(activated["version"], clone["version"]);
}

#[test]
fn interrupted_backup_requires_verified_original_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let created = submit(
        &mut s,
        "volume.create",
        json!({"name":"backup-after-crash"}),
    );
    let source = execute(&mut s, &created);
    let write = submit(
        &mut s,
        "volume.write",
        json!({
            "id":source["resource_id"], "expected_resource_version":source["version"],
            "name":"record", "content":"reserved-data"
        }),
    );
    approve(&mut s, &write);
    let source = execute(&mut s, &write)["volume_ref"].clone();
    let backup = submit(
        &mut s,
        "backup.create",
        json!({
            "id":source["resource_id"], "expected_resource_version":source["version"]
        }),
    );
    drop(s);
    cut_after_host_write(&cfg);

    let mut s = Store::open(cfg.clone()).unwrap();
    s.transaction("restart", broker::recover).unwrap();
    let lost = call(&mut s, "Task.Get", json!({"id":backup})).unwrap();
    assert_eq!(lost["outcome_knowledge"], "Unknown");
    let op = &lost["operation_refs"][0];
    let target = json!({"id":op["resource_id"],"expected_resource_version":op["version"]});
    let backup_id = format!("backup-{}", op["resource_id"].as_str().unwrap());
    let directory = cfg.state_dir.join("backups").join(&backup_id);
    let blob = directory.join("blob-0");
    assert_eq!(fs::read(&blob).unwrap(), b"reserved-data");
    assert_eq!(
        call(&mut s, "Resource.Get", json!({"id":backup_id}))
            .unwrap_err()
            .0
            .code,
        "NotFound"
    );
    assert_eq!(call(&mut s, "Task.Retry", json!({
        "id":backup, "key":"no-second-backup","expected_resource_version":lost["task_revision"]
    })).unwrap_err().0.code, "OutcomeUnknown");

    let tool = storage::publication::INSPECT;
    let prepare = |s: &mut Store, delegation: &str| {
        call(
            s,
            "Task.Prepare",
            json!({
                "tool":tool,"input":target,"delegation_id":delegation,"key":id("inspection")
            }),
        )
    };
    assert_eq!(
        prepare(&mut s, "owner-delegation").unwrap_err().0.code,
        "PermissionDenied"
    );
    let grant = call(
        &mut s,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":[tool,"task.submit","task.read"],
            "tool_targets":[op["resource_id"]],"units":4,"ttl_seconds":120
        }),
    )
    .unwrap();
    let delegation = grant["delegation_ref"]["resource_id"].as_str().unwrap();
    let inspect = |s: &mut Store| {
        let req = prepare(s, delegation).unwrap();
        call(s, "Task.Submit", req).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    fs::write(&blob, b"changed-bytes").unwrap();
    let corrupt = inspect(&mut s);
    assert_eq!(claim(&mut s), corrupt);
    let job = dispatch(&mut s, &corrupt);
    let refused = job.perform(|_| unreachable!());
    assert_eq!(refused.as_ref().unwrap_err().0.code, "PreconditionFailed");
    s.transaction("finish", |db| broker::finish(db, &job, refused))
        .unwrap();
    fs::write(&blob, b"reserved-data").unwrap();

    let inspection = inspect(&mut s);
    execute(&mut s, &inspection);
    let evidence =
        call(&mut s, "Task.Get", json!({"id":inspection})).unwrap()["result"]["artifact_refs"][0]
            .clone();
    let mut reconcile = target.clone();
    reconcile["evidence_ref"] = evidence;
    let moved = directory.with_extension("moved");
    fs::rename(&directory, &moved).unwrap();
    fs::create_dir(&directory).unwrap();
    assert_eq!(
        call(&mut s, "Operation.Reconcile", reconcile.clone())
            .unwrap_err()
            .0
            .code,
        "PreconditionFailed"
    );
    fs::remove_dir(&directory).unwrap();
    fs::rename(&moved, &directory).unwrap();

    fs::write(&blob, b"changed-bytes").unwrap();
    assert_eq!(
        call(&mut s, "Operation.Reconcile", reconcile.clone())
            .unwrap_err()
            .0
            .code,
        "OutcomeUnknown"
    );
    fs::write(&blob, b"reserved-data").unwrap();
    let refreshed = inspect(&mut s);
    execute(&mut s, &refreshed);
    reconcile["evidence_ref"] =
        call(&mut s, "Task.Get", json!({"id":refreshed})).unwrap()["result"]["artifact_refs"][0]
            .clone();

    let settled = call(&mut s, "Operation.Reconcile", reconcile).unwrap();
    assert_eq!(settled["outcome_knowledge"], "Known");
    let used = call(&mut s, "Task.Get", json!({"id":refreshed})).unwrap();
    assert_eq!(
        call(
            &mut s,
            "Task.EvictResults",
            json!({"id":refreshed,"expected_resource_version":used["task_revision"],"reason":"expiry"}),
        )
        .unwrap_err()
        .0
        .code,
        "Conflict"
    );
    let unused = call(&mut s, "Task.Get", json!({"id":inspection})).unwrap();
    call(
        &mut s,
        "Task.EvictResults",
        json!({"id":inspection,"expected_resource_version":unused["task_revision"],"reason":"expiry"}),
    )
    .unwrap();
    let task = call(&mut s, "Task.Get", json!({"id":backup})).unwrap();
    assert_eq!(task["lifecycle"], "Succeeded");
    let original = call(
        &mut s,
        "Artifact.Read",
        json!({
            "reference":task["result"]["artifact_refs"][0]
        }),
    )
    .unwrap();
    assert_eq!(original["resource_id"], backup_id);
    assert_eq!(
        fs::read_dir(cfg.state_dir.join("backups")).unwrap().count(),
        1
    );
    let restore = submit(
        &mut s,
        "backup.restore",
        json!({
            "id":original["resource_id"],"expected_resource_version":original["version"]
        }),
    );
    let restored = execute(&mut s, &restore);
    let read = submit(
        &mut s,
        "volume.read",
        json!({
            "id":restored["resource_id"],"expected_resource_version":restored["version"],"name":"record"
        }),
    );
    assert_eq!(execute(&mut s, &read)["content"], "reserved-data");
    assert!(
        s.transaction("holds", |db| {
            Ok(db.c.query_row(
                "SELECT count(*) FROM holds WHERE holder=?1",
                [op["resource_id"].as_str().unwrap()],
                |r| r.get::<_, u32>(0),
            )? == 0)
        })
        .unwrap()
    );
}

#[test]
fn cancelling_a_backup_before_rename_discards_the_staged_set() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut s) = initialized(tmp.path());
    let created = submit(
        &mut s,
        "volume.create",
        json!({"name":"cancel-before-publish"}),
    );
    let source = execute(&mut s, &created);
    let backup = submit(
        &mut s,
        "backup.create",
        json!({
            "id":source["resource_id"], "expected_resource_version":source["version"]
        }),
    );
    assert_eq!(claim(&mut s), backup);
    let job = dispatch(&mut s, &backup);
    let receipt = job.perform(|event| {
        let t = call(&mut s, "Task.Get", json!({"id":backup}))?;
        call(
            &mut s,
            "Task.Cancel",
            json!({"id":backup,"expected_resource_version":t["task_revision"]}),
        )?;
        s.transaction("checkpoint", |db| job.checkpoint(db, event))
    });
    assert_eq!(receipt.as_ref().unwrap_err().0.code, "Fenced");
    s.transaction("finish", |db| broker::finish(db, &job, receipt))
        .unwrap();
    let t = call(&mut s, "Task.Get", json!({"id":backup})).unwrap();
    assert_eq!(
        (t["lifecycle"].as_str(), t["outcome_knowledge"].as_str()),
        (Some("Cancelled"), Some("Known"))
    );
    assert_eq!(
        fs::read_dir(cfg.state_dir.join("backups")).unwrap().count(),
        0
    );
}

#[test]
fn elapsed_permit_fences_the_backup_publication_checkpoint() {
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, mut store) = initialized(tmp.path());
    let created = submit(&mut store, "volume.create", json!({"name":"permit-time"}));
    let source = execute(&mut store, &created);
    let task = submit(
        &mut store,
        "backup.create",
        json!({"id":source["resource_id"], "expected_resource_version":source["version"]}),
    );
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, &task);
    let receipt = work.perform(|checkpoint| {
        store.transaction("elapsed-permit", |db| {
            let permit = db.resolve(&work.effect.spec["permit_ref"])?;
            assert!(!expired(permit.body["not_after"].as_str().unwrap())?);
            assert_eq!(
                db.c.execute(
                    "UPDATE authorization_deadlines SET deadline_boot_ms=0 WHERE resource=?1",
                    [&permit.id],
                )?,
                1
            );
            Ok(())
        })?;
        store.transaction("checkpoint", |db| work.checkpoint(db, checkpoint))
    });
    assert_eq!(receipt.as_ref().unwrap_err().0.code, "Fenced");
    store
        .transaction("finish", |db| broker::finish(db, &work, receipt))
        .unwrap();
    let task = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(task["lifecycle"], "Failed");
    assert_eq!(task["outcome_knowledge"], "Known");
    assert_eq!(
        fs::read_dir(cfg.state_dir.join("backups")).unwrap().count(),
        0
    );
}

#[test]
fn detached_volume_readers_overlap_and_fence_writes_through_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(tmp.path());
    let create = submit(&mut store, "volume.create", json!({"name":"readers"}));
    let volume = execute(&mut store, &create);
    let identity = volume["resource_id"].as_str().unwrap();
    let file = config
        .state_dir
        .join("volumes")
        .join(identity)
        .join("data/entry");
    fs::write(&file, "original").unwrap();
    let read = json!({"id":identity,"expected_resource_version":volume["version"],"name":"entry"});
    let write = json!({"id":identity,"expected_resource_version":volume["version"],"name":"entry","content":"updated"});
    let first = submit(&mut store, "volume.read", read.clone());
    let second = submit(
        &mut store,
        "volume.read_chunk",
        json!({"target":{"id":identity,"expected_resource_version":volume["version"]},"path":"entry","offset":0,"limit":32}),
    );
    assert_eq!(claim(&mut store), first);
    let first_work = dispatch(&mut store, &first);
    assert_eq!(claim(&mut store), second);
    let second_work = dispatch(&mut store, &second);
    let count = |store: &mut Store| {
        store
            .transaction("count-volume-readers", |db| {
                Ok(db.c.query_row(
                    "SELECT count(*) FROM volume_read_holds WHERE volume=?1",
                    [identity],
                    |r| r.get::<_, u32>(0),
                )?)
            })
            .unwrap()
    };
    assert_eq!(count(&mut store), 2);

    let blocked_write = submit(&mut store, "volume.write", write.clone());
    approve(&mut store, &blocked_write);
    assert_eq!(claim(&mut store), blocked_write);
    let error = store
        .transaction("blocked-write", |db| broker::dispatch(db, &blocked_write))
        .err()
        .expect("active readers must prevent the write dispatch");
    assert_eq!(error.0.code, "Conflict");
    store
        .transaction("fail-blocked-write", |db| {
            broker::dispatch_failed(db, &blocked_write, &error)
        })
        .unwrap();
    let denied = call(&mut store, "Task.Get", json!({"id":blocked_write})).unwrap();
    assert_eq!(denied["outcome_knowledge"], "NotStarted");
    assert_eq!(fs::read_to_string(&file).unwrap(), "original");

    let receipt =
        first_work.perform(|_| panic!("read should not persist a host-effect checkpoint"));
    store
        .transaction("settle-first-reader", |db| {
            broker::finish(db, &first_work, receipt)
        })
        .unwrap();
    assert_eq!(count(&mut store), 1);
    drop(second_work);
    drop(store);

    let mut store = Store::open(config).unwrap();
    assert_eq!(count(&mut store), 1);
    store
        .transaction("recover-reader", broker::recover)
        .unwrap();
    assert_eq!(count(&mut store), 0);
    let retried = execute(&mut store, &second);
    assert_eq!(
        STANDARD
            .decode(retried["data_base64"].as_str().unwrap())
            .unwrap(),
        b"original"
    );
    assert_eq!(count(&mut store), 0);

    let writer = submit(&mut store, "volume.write", write);
    approve(&mut store, &writer);
    assert_eq!(claim(&mut store), writer);
    let writing = dispatch(&mut store, &writer);
    let blocked_read = submit(&mut store, "volume.read", read);
    assert_eq!(claim(&mut store), blocked_read);
    let error = store
        .transaction("blocked-reader", |db| broker::dispatch(db, &blocked_read))
        .err()
        .expect("an exclusive writer must prevent the reader dispatch");
    assert_eq!(error.0.code, "Conflict");
    store
        .transaction("fail-blocked-reader", |db| {
            broker::dispatch_failed(db, &blocked_read, &error)
        })
        .unwrap();
    assert_eq!(count(&mut store), 0);
    let receipt = writing.perform(|_| panic!("ordinary write settled by its native receipt"));
    store
        .transaction("finish-writer", |db| broker::finish(db, &writing, receipt))
        .unwrap();
    assert_eq!(fs::read_to_string(file).unwrap(), "updated");
}

#[test]
fn file_rename_recovery_distinguishes_unprepared_and_witnessed_original() {
    let root = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(root.path());
    let create = submit(&mut store, "volume.create", json!({"name":"rename-source"}));
    let volume = execute(&mut store, &create);
    let data = config
        .state_dir
        .join("volumes")
        .join(volume["resource_id"].as_str().unwrap())
        .join("data");
    fs::write(data.join("before"), "stable bytes").unwrap();
    let target = json!({"id":volume["resource_id"],"expected_resource_version":volume["version"]});
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read","volume.verify_file","volume.rename_file"],
        "tool_targets":[volume["resource_id"]],"units":6,"ttl_seconds":120}),
    )
    .unwrap();
    let scoped = |store: &mut Store, tool: &str, input: Value, grant: &Value| {
        let request = call(
            store,
            "Task.Prepare",
            json!({"tool":tool,"input":input,"delegation_id":grant["delegation_ref"]["resource_id"],"key":id("key")}),
        )
        .unwrap();
        call(store, "Task.Submit", request).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let hashed = scoped(
        &mut store,
        "volume.verify_file",
        json!({"target":target,"path":"before"}),
        &grant,
    );
    let proof = execute(&mut store, &hashed);
    let input = json!({"target":target,"source":"before","destination":"after","expected_observation_digest":proof["observation_digest"]});

    let early = scoped(&mut store, "volume.rename_file", input.clone(), &grant);
    approve(&mut store, &early);
    assert_eq!(claim(&mut store), early);
    drop(dispatch(&mut store, &early));
    drop(store);
    let mut store = Store::open(config).unwrap();
    store
        .transaction("recover-unprepared-rename", broker::recover)
        .unwrap();
    let failed = call(&mut store, "Task.Get", json!({"id":early})).unwrap();
    assert_eq!(failed["lifecycle"], "Failed");
    assert_eq!(failed["outcome_knowledge"], "Known");
    assert_eq!(failed["conditions"][0]["type"], "LeaseLost");
    assert!(!data.join("after").exists());

    let interrupted = scoped(&mut store, "volume.rename_file", input, &grant);
    approve(&mut store, &interrupted);
    assert_eq!(claim(&mut store), interrupted);
    let work = dispatch(&mut store, &interrupted);
    let result = work.perform(|event| {
        store.transaction("witness-before-rename", |db| work.checkpoint(db, event))?;
        Err(err(
            "OutcomeUnknown",
            "worker interrupted at the witnessed boundary",
        ))
    });
    store
        .transaction("lose-worker", |db| broker::finish(db, &work, result))
        .unwrap();
    let waiting = call(&mut store, "Task.Get", json!({"id":interrupted})).unwrap();
    assert_eq!(waiting["outcome_knowledge"], "Unknown");
    let operation = &waiting["operation_refs"][0];
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read","volume.inspect_rename"],
        "tool_targets":[operation["resource_id"]],"units":2,"ttl_seconds":120}),
    )
    .unwrap();
    let inspect = scoped(
        &mut store,
        "volume.inspect_rename",
        json!({"id":operation["resource_id"],"expected_resource_version":operation["version"]}),
        &grant,
    );
    assert_eq!(execute(&mut store, &inspect)["placement"], "Original");
    let evidence =
        call(&mut store, "Task.Get", json!({"id":inspect})).unwrap()["result"]["artifact_refs"][0]
            .clone();
    let reconciled = call(
        &mut store, "Operation.Reconcile",
        json!({"id":operation["resource_id"],"expected_resource_version":operation["version"],"evidence_ref":evidence}),
    ).unwrap();
    assert_eq!(reconciled["outcome_knowledge"], "Known");
    let failed = call(&mut store, "Task.Get", json!({"id":interrupted})).unwrap();
    assert_eq!(failed["lifecycle"], "Failed");
    assert_eq!(failed["conditions"][0]["type"], "LeaseLost");
    let read = submit(
        &mut store,
        "volume.read",
        json!({"id":volume["resource_id"],"expected_resource_version":volume["version"],"name":"before"}),
    );
    assert_eq!(execute(&mut store, &read)["content"], "stable bytes");
    assert!(!data.join("after").exists());
}

#[test]
fn result_byte_ceiling_checks_json_encoding_before_publication() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::local(&directory.path().join("bounded"));
    config.read_result_bytes_per_principal = Some(artifact::capacity::RESERVATION);
    Store::init(&config).unwrap();
    let mut store = Store::open(config).unwrap();
    let task = submit(&mut store, "system.status", json!({}));
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, &task);
    let bytes = "\0".repeat(artifact::capacity::RESERVATION as usize / 6);
    store
        .transaction("large-read-result", |db| {
            broker::finish(db, &work, Ok(json!({"bytes":bytes})))
        })
        .unwrap();
    let state = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(state["lifecycle"], "Failed");
    assert_eq!(state["outcome_knowledge"], "Known");
    assert_eq!(state["conditions"][0]["type"], "QuotaExceeded");
    assert_eq!(state["result"]["artifact_refs"], json!([]));
    let usage = call(&mut store, "Task.Admission", json!({})).unwrap();
    assert_eq!(usage["read_results"]["retained_bytes"], "0");
    assert_eq!(usage["read_results"]["reserved_bytes"], "0");
}
