use super::*;
use crate::host::Work;
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
          {"id":"read","tool":"system.status","after":["write"],"input":{}}
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
    let active = call(&mut s, "Workflow.Get", lookup.clone()).unwrap();
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
