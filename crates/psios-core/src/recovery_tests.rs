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
