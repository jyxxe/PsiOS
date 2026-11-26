//! Exercise a third-party SDK worker and real Core across a lost response.
use psios_api::{
    ApiResult, Client, ModelDecision, ModelOutput, ModelTokenUsage, ResourceRef, SessionModel,
    read_frame, write_frame,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::{
    env, fs,
    fs::OpenOptions,
    io::{ErrorKind, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const POLICY: &str = "UndeclaredToolV1";
const TOOL: &str = "not-in-pinned-skill";
const INPUT: &str = "retained-only-in-private-provider-journal";
const PENDING: &str = "psios-model-pending.json";
const USAGE: ModelTokenUsage = ModelTokenUsage {
    input_tokens: 23,
    output_tokens: 11,
};

struct Process(Child);

impl Process {
    fn kill(&mut self) {
        self.0.kill().unwrap();
        assert!(!self.0.wait().unwrap().success());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct FixtureProvider<'a> {
    calls: &'a Path,
    finish: bool,
}

impl SessionModel for FixtureProvider<'_> {
    fn accepts_undeclared_tool_rejection(&self) -> bool {
        true
    }

    fn decide(&self, _: &Value) -> ApiResult<ModelDecision> {
        writeln!(
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.calls)
                .unwrap(),
            "called"
        )
        .unwrap();
        Ok(if self.finish {
            ModelDecision::Finish {
                summary: "An independent job completed.".into(),
            }
        } else {
            ModelDecision::Tool {
                tool: TOOL.into(),
                input: json!({"payload":INPUT}),
            }
        })
    }

    fn decide_with_usage(&self, context: &Value) -> ApiResult<ModelOutput> {
        Ok(ModelOutput {
            decision: self.decide(context)?,
            usage: Some(USAGE),
        })
    }
}

#[test]
fn rejection_fixture_worker() {
    let Some(socket) = env::var_os("PSIOS_REJECTION_TEST_SOCKET") else {
        return;
    };
    let token = fs::read_to_string(env::var_os("PSIOS_REJECTION_TEST_TOKEN").unwrap()).unwrap();
    let model: ResourceRef =
        serde_json::from_str(&env::var("PSIOS_REJECTION_TEST_MODEL").unwrap()).unwrap();
    let journal = PathBuf::from(env::var_os("PSIOS_REJECTION_TEST_JOURNAL").unwrap());
    let calls = PathBuf::from(env::var_os("PSIOS_REJECTION_TEST_CALLS").unwrap());
    Client::new(socket, token)
        .model_next_durable(
            &model,
            "lost-receipt",
            &FixtureProvider {
                calls: &calls,
                finish: false,
            },
            &journal,
            Duration::ZERO,
        )
        .unwrap();
}

fn start(command: &mut Command, owner: &Client) -> Process {
    let mut child = Process(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while owner.call("Principal.WhoAmI", json!({})).is_err() {
        assert!(
            Instant::now() < deadline && child.0.try_wait().unwrap().is_none(),
            "Core did not start"
        );
        thread::sleep(Duration::from_millis(25));
    }
    child
}

fn intercept(listener: &UnixListener, unix: &Path) -> (UnixStream, Value, Value) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        assert!(Instant::now() < deadline, "SDK did not send the decision");
        let (mut client, _) = match listener.accept() {
            Ok(peer) => peer,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => panic!("fixture socket: {error}"),
        };
        client
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        let request = read_frame(&mut client).unwrap();
        let value: Value = serde_json::from_slice(&request).unwrap();
        let mut authority = UnixStream::connect(unix).unwrap();
        authority
            .set_read_timeout(Some(Duration::from_secs(6)))
            .unwrap();
        write_frame(&mut authority, &request).unwrap();
        let response = read_frame(&mut authority).unwrap();
        if value["method"] == "Model.Respond" {
            let envelope: Value = serde_json::from_slice(&response).unwrap();
            assert!(envelope.get("result").is_some(), "{envelope}");
            return (client, value["params"].clone(), envelope["result"].clone());
        }
        write_frame(&mut client, &response).unwrap();
    }
}

#[test]
fn rejected_decision_replays_without_tool_effect_or_second_inference() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
    let config = base.join("core.json");
    let state = base.join("state");
    let witness = base.join("witness");
    let socket = base.join("authority.sock");
    let relay = base.join("relay.sock");
    fs::write(
        &config,
        serde_json::to_vec(&json!({"state_dir":state,"witness_dir":witness,
        "socket":socket,"tick_ms":25}))
        .unwrap(),
    )
    .unwrap();
    let run = |subcommand: &str| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_psiosd"));
        command
            .arg("--config")
            .arg(&config)
            .arg(subcommand)
            .stdout(Stdio::null());
        command
    };
    assert!(run("init").status().unwrap().success());
    let owner = Client::new(
        &socket,
        fs::read_to_string(witness.join("owner.token")).unwrap(),
    );
    let mut daemon = start(&mut run("serve"), &owner);
    let register = |name| owner.call("Agent.Register", json!({"name":name})).unwrap();
    let provider = register("rejection-provider");
    let actor = register("rejection-actor");
    owner
        .call(
            "Grant.Issue",
            json!({"principal_id":provider["principal_id"],"actions":["model.serve"],
        "units":4,"ttl_seconds":180}),
        )
        .unwrap();
    let grant = owner
        .call(
            "Grant.Issue",
            json!({"principal_id":actor["principal_id"],
        "actions":["session.run","task.submit","task.read","resource.read","system.status"],"units":4,
        "ttl_seconds":180,"model_calls":4,"model_lease_ms":120000}),
        )
        .unwrap();
    let model: ResourceRef = serde_json::from_value(owner.call("Model.Register", json!({"name":"reject-tool",
        "revision":"1","provider_principal_id":provider["principal_id"],"inference_outcome_policy":"Strict",
        "max_context_bytes":16384,"max_output_bytes":4096,"max_in_flight":1,"lease_seconds":30})).unwrap()).unwrap();
    let skill = owner
        .call(
            "Skill.Publish",
            json!({"name":"allowed-status","version":"1","license":"0BSD",
        "source":"unix-fixture","tools":["system.status"],"instructions":"Report status."}),
        )
        .unwrap();
    let owner_session = Client::new(&socket, actor["token"].as_str().unwrap());
    let worker_client = Client::new(&socket, provider["token"].as_str().unwrap());
    let open = |key, queued| {
        owner_session
            .call(
                "Session.Open",
                json!({"key":key,"goal":"Report status.",
        "skill_ref":skill,"model_ref":model,"max_turns":2,"context_policy":"ReferencesOnly",
        "provider_queue":queued,"delegation_id":grant["delegation_ref"]["resource_id"]}),
            )
            .unwrap()
    };
    let session = open("first", true);
    let session_id = session["reference"]["resource_id"].as_str().unwrap();
    let calls = base.join("inference-calls");
    let journal = base.join("provider");
    fs::create_dir(&journal).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o700)).unwrap();
    let token_path = base.join("provider.token");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&token_path)
        .unwrap()
        .write_all(provider["token"].as_str().unwrap().as_bytes())
        .unwrap();
    let listener = UnixListener::bind(&relay).unwrap();
    let mut worker = Process(
        Command::new(env::current_exe().unwrap())
            .args(["--exact", "rejection_fixture_worker"])
            .env("PSIOS_REJECTION_TEST_SOCKET", &relay)
            .env("PSIOS_REJECTION_TEST_TOKEN", &token_path)
            .env(
                "PSIOS_REJECTION_TEST_MODEL",
                serde_json::to_string(&model).unwrap(),
            )
            .env("PSIOS_REJECTION_TEST_JOURNAL", &journal)
            .env("PSIOS_REJECTION_TEST_CALLS", &calls)
            .spawn()
            .unwrap(),
    );
    let (withheld, request, receipt) = intercept(&listener, &socket);
    worker.kill();
    drop(withheld);
    assert_eq!(request["session_id"], session_id);
    assert_eq!(request["decision_rejection_policy"], POLICY);
    let pending: Value = serde_json::from_slice(&fs::read(journal.join(PENDING)).unwrap()).unwrap();
    assert_eq!(pending["format"], 5);
    assert_eq!(pending["request"], request);
    assert_eq!(
        receipt["rejection"],
        json!({"policy":POLICY,"code":"ToolNotDeclared"})
    );
    assert_eq!(receipt["lifecycle"], "Failed");
    assert_eq!(receipt["outcome_knowledge"], "Known");
    assert!(receipt["task_id"].is_null());
    assert!(!receipt.to_string().contains(INPUT) && !receipt.to_string().contains(TOOL));
    let budget = || {
        owner_session
            .call(
                "Budget.Get",
                json!({"id":grant["budget_ref"]["resource_id"]}),
            )
            .unwrap()
    };
    let spent = budget();
    assert_eq!(spent["model_calls_used"], 1);
    assert!(spent["model_lease_ms_charged"].as_u64().unwrap() < 30_000);
    let verify_usage = || {
        let saved = owner_session
            .call(
                "Resource.Resolve",
                json!({"reference":receipt["usage_ref"]}),
            )
            .unwrap();
        assert_eq!(saved["value"]["reported_tokens"], json!(USAGE));
    };
    verify_usage();
    daemon.kill();
    assert!(run("verify").status().unwrap().success());
    daemon = start(&mut run("serve"), &owner);
    let provider_fixture = FixtureProvider {
        calls: &calls,
        finish: true,
    };
    assert_eq!(
        worker_client
            .model_next_durable(
                &model,
                "restart",
                &provider_fixture,
                &journal,
                Duration::ZERO
            )
            .unwrap(),
        Some(receipt.clone())
    );
    assert!(!journal.join(PENDING).exists());
    assert_eq!(fs::read_to_string(&calls).unwrap(), "called\n");
    assert_eq!(budget(), spent);
    verify_usage();
    let history = owner_session
        .call("Session.History", json!({"id":session_id}))
        .unwrap();
    assert_eq!(history["tasks"], json!([]));
    assert_eq!(history["turns"][0]["decision"], receipt);
    assert!(
        history["turns"][0]
            .get("late_decision_observation")
            .is_none()
    );
    assert_eq!(
        worker_client
            .call(
                "Model.ClaimNext",
                json!({"model_ref":model,"key":"lost-receipt"})
            )
            .unwrap()["completed"],
        receipt
    );
    let mut changed = request.clone();
    changed["decision"]["input"] = json!({"other":true});
    assert_eq!(
        worker_client
            .call("Model.Respond", changed)
            .unwrap_err()
            .code,
        "IdempotencyConflict"
    );
    assert_eq!(
        owner_session
            .call("Model.Respond", request.clone())
            .unwrap_err()
            .code,
        "PermissionDenied"
    );
    let mut late = request;
    late.as_object_mut()
        .unwrap()
        .remove("decision_rejection_policy");
    assert_eq!(
        worker_client
            .call("Model.ObserveLateDecision", late)
            .unwrap_err()
            .code,
        "Fenced"
    );

    let second = open("next-queued-session", true);
    let answer = worker_client
        .model_next_durable(
            &model,
            "next-claim",
            &provider_fixture,
            &journal,
            Duration::ZERO,
        )
        .unwrap()
        .unwrap();
    assert_eq!(answer["lifecycle"], "Succeeded");
    assert_eq!(
        answer["session_ref"]["resource_id"],
        second["reference"]["resource_id"]
    );
    assert_eq!(fs::read_to_string(&calls).unwrap(), "called\ncalled\n");
    assert_eq!(budget()["model_calls_used"], 2);

    // Without optional negotiation, a strict worker can still correct a rejected decision.
    let third = open("legacy-correction", false);
    let third_id = &third["reference"]["resource_id"];
    let claim = worker_client
        .call(
            "Model.Claim",
            json!({"session_id":third_id,"key":"retry-correction"}),
        )
        .unwrap();
    let mut direct = json!({"session_id":third_id,"lease_id":claim["lease_id"],"decision":{"action":"tool","tool":TOOL,"input":{}}});
    assert_eq!(
        worker_client
            .call("Model.Respond", direct.clone())
            .unwrap_err()
            .code,
        "PermissionDenied"
    );
    direct["decision"] = json!({"action":"finish","summary":"Corrected"});
    assert_eq!(
        worker_client.call("Model.Respond", direct).unwrap()["lifecycle"],
        "Succeeded"
    );
    daemon.kill();
    let sqlite =
        Connection::open_with_flags(state.join("state.db"), OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    let count: i64 = sqlite
        .query_row(
            "SELECT count(*) FROM current WHERE kind IN ('Task','Operation','ApprovalRequest')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    let original: String = sqlite
        .query_row(
            "SELECT receipt FROM session_turns WHERE session=?1",
            [session_id],
            |row| row.get(0),
        )
        .unwrap();
    sqlite
        .execute(
            "UPDATE session_turns SET receipt=json_remove(receipt,'$.rejection') WHERE session=?1",
            [session_id],
        )
        .unwrap();
    let invalid = run("verify").stderr(Stdio::piped()).output().unwrap();
    assert!(
        !invalid.status.success(),
        "offline verified a changed rejection receipt"
    );
    sqlite
        .execute(
            "UPDATE session_turns SET receipt=?1 WHERE session=?2",
            (&original, session_id),
        )
        .unwrap();
    drop(sqlite);
    assert!(run("verify").status().unwrap().success());
}
