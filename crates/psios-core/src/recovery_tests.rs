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
    let r = w.perform(|_, _| unreachable!("not an OCI start"));
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

#[test]
fn kill_after_host_write_requires_observed_postcondition() {
    const FLAG: &str = "PSIOS_TEST_CRASH_STATE";
    if let Some(path) = std::env::var_os(FLAG) {
        let cfg = Config::local(Path::new(&path));
        let mut s = Store::open(cfg.clone()).unwrap();
        let task = claim(&mut s);
        let w = dispatch(&mut s, &task);
        w.perform(|_, _| unreachable!("not an OCI start")).unwrap();
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
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recovery_tests::kill_after_host_write_requires_observed_postcondition",
            "--nocapture",
        ])
        .env(FLAG, &c.state_dir)
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
fn lost_read_gets_new_claim_and_late_receipt_is_fenced() {
    let tmp = tempfile::tempdir().unwrap();
    let (c, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "system.status", json!({}));
    assert_eq!(claim(&mut s), task);
    let old = dispatch(&mut s, &task);
    let receipt = old.perform(|_, _| unreachable!("not an OCI start"));
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
    let receipt = w.perform(|_, _| unreachable!("not an OCI start"));
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
    work.perform(|_, _| unreachable!("not an OCI start"))
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
