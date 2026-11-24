use super::*;
use psios_api::VERSION;

fn envelope(config: &Config, method: &str, params: Value) -> Request {
    Request {
        schema_version: VERSION.into(),
        request_id: id("drain-test"),
        token: fs::read_to_string(config.witness_dir.join("owner.token")).unwrap(),
        method: method.into(),
        params,
    }
}

fn call(store: &mut Store, method: &str, params: Value) -> Value {
    let request = envelope(&store.config, method, params);
    store
        .request(rustix::process::geteuid().as_raw(), request)
        .unwrap()
}

fn prepared(store: &mut Store) -> Value {
    call(
        store,
        "Task.Prepare",
        json!({"tool":"system.status","input":{},"key":"drain","delegation_id":"owner-delegation"}),
    )
}

#[test]
fn accepted_socket_cannot_commit_a_new_submission_after_stop() {
    let temp = tempfile::tempdir().unwrap();
    let config = Config::local(&temp.path().join("core"));
    Store::init(&config).unwrap();
    let mut store = Store::open(config.clone()).unwrap();
    let submit = envelope(&config, "Task.Submit", prepared(&mut store));
    let shared = Arc::new(Mutex::new(store));
    let (mut client, mut stream) = UnixStream::pair().unwrap();
    write_frame(&mut client, &serde_json::to_vec(&submit).unwrap()).unwrap();
    let stop = AtomicBool::new(true);
    connection(&shared, &mut stream, &stop).unwrap();
    let answer = parse(&read_frame(&mut client).unwrap()).unwrap();
    assert_eq!(answer["request_id"], submit.request_id);
    assert_eq!(answer["error"]["code"], "DependencyUnavailable");
    assert!(answer.get("result").is_none());
    let mut store = state(&shared).unwrap();
    let recovered = call(&mut store, "Task.Submit", submit.params);
    assert_eq!(recovered["deduplicated"], false);
}

#[test]
fn stop_between_claim_and_dispatch_returns_the_unstarted_task() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config::local(&temp.path().join("core"));
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg.clone()).unwrap();
    let draft = prepared(&mut store);
    let budget = json!({"id":draft["budget_ref"]["resource_id"]});
    let t = call(&mut store, "Task.Submit", draft)["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        store.transaction("claim", broker::claim).unwrap(),
        Some(t.clone())
    );
    let request = json!({"id":t});
    let claimed = call(&mut store, "Task.Get", request.clone());
    let previous = claimed["current_run_id"].as_str().unwrap();
    let reserved = call(&mut store, "Budget.Get", budget.clone());
    assert_eq!(reserved["reserved"], 1);
    assert_eq!(reserved["spent"], 0);
    let stop = AtomicBool::new(true);
    assert!(
        dispatch_when_active(&mut store, &stop, "drain", &t)
            .unwrap()
            .is_none()
    );
    let queued = call(&mut store, "Task.Get", request.clone());
    assert_eq!(queued["lifecycle"], "Queued");
    assert_eq!(queued["outcome_knowledge"], "NotStarted");
    assert!(queued.get("current_run_id").is_none());
    assert_eq!(queued["operation_refs"], claimed["operation_refs"]);
    assert_eq!(call(&mut store, "Budget.Get", budget.clone()), reserved);
    let lost = call(&mut store, "Run.Get", json!({"id":previous}));
    assert_eq!(lost["lifecycle"], "Lost");
    assert_eq!(lost["outcome_knowledge"], "NotStarted");
    drop(store);

    let mut store = Store::open(cfg).unwrap();
    store.transaction("restart", broker::recover).unwrap();
    assert_eq!(call(&mut store, "Task.Get", request.clone()), queued);
    assert_eq!(
        store.transaction("claim-again", broker::claim).unwrap(),
        Some(t.clone())
    );
    let stop = AtomicBool::new(false);
    let work = dispatch_when_active(&mut store, &stop, "dispatch", &t)
        .unwrap()
        .unwrap();
    stop.store(true, Ordering::Release);
    assert!(
        dispatch_when_active(&mut store, &stop, "already-dispatched", &t)
            .unwrap()
            .is_none()
    );
    let in_flight = call(&mut store, "Task.Get", request.clone());
    assert_eq!(in_flight["outcome_knowledge"], "InFlight");
    let receipt = work.perform(|_| panic!("status requires no effect checkpoint"));
    store
        .transaction("finish", |db| broker::finish(db, &work, receipt))
        .unwrap();
    let completed = call(&mut store, "Task.Get", request);
    assert_eq!(completed["lifecycle"], "Succeeded");
    let run = call(
        &mut store,
        "Run.Get",
        json!({"id":completed["current_run_id"]}),
    );
    assert_eq!(run["claim_epoch"], "2");
    let budget = call(&mut store, "Budget.Get", budget);
    assert_eq!(budget["spent"], 1);
    assert_eq!(budget["reserved"], 0);
}
