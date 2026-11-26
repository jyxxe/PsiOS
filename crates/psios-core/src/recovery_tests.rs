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

#[test]
fn volume_create_restart_before_checkpoint_fences_the_former_worker() {
    let directory = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(directory.path());
    let task_id = submit(&mut store, "volume.create", json!({"name":"empty-create"}));
    assert_eq!(claim(&mut store), task_id);
    let former = dispatch(&mut store, &task_id);
    assert_eq!(former.effect.body["phase"], "PermitBound");
    let final_path = config
        .state_dir
        .join("volumes")
        .join(format!("volume-{}", former.op.id));
    assert!(!final_path.exists());
    drop(store);
    let mut store = Store::open(config).unwrap();
    store.transaction("recover", broker::recover).unwrap();
    let task = call(&mut store, "Task.Get", json!({"id":task_id})).unwrap();
    assert_eq!(task["lifecycle"], "Failed");
    assert_eq!(task["outcome_knowledge"], "NotStarted");
    assert_eq!(task["conditions"][0]["type"], "LeaseLost");
    let refused = former
        .perform(|event| store.transaction("former-worker", |db| former.checkpoint(db, event)))
        .unwrap_err();
    assert_eq!(refused.0.code, "Fenced");
    assert!(!final_path.exists());
}

#[test]
fn work_item_evidence_dispatch_rolls_back_receipt_failures_and_keeps_exact_once_cas() {
    use rusqlite::params;

    let directory = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(directory.path());
    let producer = submit(&mut store, "system.status", json!({}));
    execute(&mut store, &producer);
    let produced = call(&mut store, "Resource.Get", json!({"id":producer})).unwrap();
    let source = &produced["value"];
    let project = call(
        &mut store,
        "Project.Create",
        json!({"key":id("dispatch-project"),"title":"Atomic evidence dispatch"}),
    )
    .unwrap();
    call(&mut store, "Grant.Issue", json!({
        "principal_id":policy::OWNER,"actions":["project.read","project.manage","project.contribute"],
        "tool_targets":[project["reference"]["resource_id"]],"units":1,"ttl_seconds":120,
    })).unwrap();
    let profile = call(&mut store, "Resource.Get", json!({"id":policy::PROFILE})).unwrap();
    let item = call(&mut store, "WorkItem.Create", json!({
        "project_id":project["reference"]["resource_id"],"expected_project_version":project["reference"]["version"],
        "key":id("dispatch-item"),"title":"Verify node status","assignee_principal_id":policy::OWNER,
        "criterion":{"kind":"TaskResult","tool":"system.status","target_ref":profile["reference"]},
    })).unwrap();
    let id = item["reference"]["resource_id"].as_str().unwrap();
    let grant = call(&mut store, "Grant.Issue", json!({
        "principal_id":policy::OWNER,"actions":["project.submit_evidence","task.submit","task.read"],
        "tool_targets":[id],"units":3,"ttl_seconds":120,
    })).unwrap();
    let budget = grant["budget_ref"]["resource_id"].as_str().unwrap();
    let input = json!({
        "id":id,"expected_resource_version":item["reference"]["version"],
        "task_ref":produced["reference"],"artifact_ref":source["result"]["artifact_refs"][0],
    });
    let enqueue = |store: &mut Store, key: &str| {
        let prepared = call(
            store,
            "Task.Prepare",
            json!({
                "tool":"project.submit_evidence","input":input,"key":key,
                "delegation_id":grant["delegation_ref"]["resource_id"],
            }),
        )
        .unwrap();
        let accepted = call(store, "Task.Submit", prepared.clone()).unwrap();
        (prepared, accepted["task_id"].as_str().unwrap().to_owned())
    };
    let (prepared, first) = enqueue(&mut store, "evidence-first");
    let (_, competing) = enqueue(&mut store, "evidence-competing");
    let (_, cancelled) = enqueue(&mut store, "evidence-cancelled");
    let before = call(&mut store, "Task.Get", json!({"id":cancelled})).unwrap();
    let cancelled = call(
        &mut store,
        "Task.Cancel",
        json!({
            "id":cancelled, "expected_resource_version":before["task_revision"],
        }),
    )
    .unwrap();
    assert_eq!(cancelled["lifecycle"], "Cancelled");
    assert_eq!(cancelled["outcome_knowledge"], "NotStarted");
    assert_eq!(claim(&mut store), first);
    assert_eq!(claim(&mut store), competing);

    let assert_unchanged = |store: &mut Store| {
        store.transaction("verify-evidence-rollback", |db| {
            let current = db.of(id, "WorkItem")?;
            assert_eq!(current.reference(), item["reference"]);
            assert_eq!(current.body["lifecycle"], "Ready");
            let count: i64 = db.c.query_row(
                "SELECT count(*) FROM versions WHERE kind='WorkItemEvidence' AND body->>'work_item_id'=?1",
                [id], |r| r.get(0),
            )?;
            assert_eq!(count, 0);
            let count: i64 = db.c.query_row(
                "SELECT count(*) FROM versions WHERE kind='Artifact' AND spec->>'task_id'=?1",
                [&first], |r| r.get(0),
            )?;
            assert_eq!(count, 0);
            let task = db.of(&first, "Task")?;
            let op = db.resolve(&task.body["operation_refs"][0])?;
            assert_eq!(task.body["outcome_knowledge"], "NotStarted");
            assert_eq!(op.body["outcome_knowledge"], "NotStarted");
            assert_eq!(op.body["effect_refs"], json!([]));
            let charged: (i64, i64) = db.c.query_row(
                "SELECT reserved,spent FROM budgets WHERE id=?1", [budget],
                |r| Ok((r.get(0)?,r.get(1)?)),
            )?;
            assert_eq!(charged, (2, 0));
            Ok(())
        }).unwrap();
    };

    // A pre-publication check can reject before inserting an Artifact. Dispatch
    // must propagate that error and roll back the WorkItem change as well.
    let refused = store
        .transaction("fail-evidence-result-limit", |db| {
            db.c.execute(
            "INSERT INTO read_result_charges(resource,owner,bytes,kind) VALUES(?1,?2,1,'Limited')",
            params![first,policy::OWNER],
        )?;
            broker::dispatch(db, &first)
        })
        .err()
        .expect("the undersized result reservation must abort atomic dispatch");
    assert_eq!(refused.0.code, "QuotaExceeded");
    assert_unchanged(&mut store);

    store
        .transaction("install-evidence-receipt-fault", |db| {
            db.c.execute_batch(
                "CREATE TEMP TRIGGER fail_evidence_receipt BEFORE INSERT ON versions
            WHEN NEW.kind='Artifact' AND EXISTS (
                SELECT 1 FROM current WHERE kind='WorkItem' AND body->>'lifecycle'='InReview'
            ) BEGIN SELECT RAISE(ABORT,'fixture aborted evidence receipt'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    let aborted = store
        .transaction("abort-evidence-receipt", |db| broker::dispatch(db, &first))
        .err()
        .expect("the work item was updated before the injected receipt write failure");
    assert_eq!(aborted.0.code, "StorageUnavailable");
    assert_unchanged(&mut store);
    store
        .transaction("remove-evidence-receipt-fault", |db| {
            db.c.execute_batch("DROP TRIGGER fail_evidence_receipt")?;
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .transaction("settle-atomic-evidence", |db| broker::dispatch(db, &first))
            .unwrap()
            .is_none()
    );

    let stale = store
        .transaction("competing-evidence-cas", |db| {
            broker::dispatch(db, &competing)
        })
        .err()
        .expect("the competing WorkItem revision must be fenced");
    assert_eq!(stale.0.code, "Conflict");
    store
        .transaction("settle-competing-evidence-cas", |db| {
            broker::dispatch_failed(db, &competing, &stale)
        })
        .unwrap();
    let refused = call(&mut store, "Task.Get", json!({"id":competing})).unwrap();
    assert_eq!(refused["lifecycle"], "Failed");
    assert_eq!(refused["outcome_knowledge"], "NotStarted");
    let replayed = call(&mut store, "Task.Submit", prepared).unwrap();
    assert_eq!(replayed["task_id"], first);
    assert_eq!(replayed["deduplicated"], true);

    let done = call(&mut store, "Task.Get", json!({"id":first})).unwrap();
    let receipt = call(
        &mut store,
        "Artifact.Read",
        json!({"reference":done["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    let closed = call(&mut store, "WorkItem.Get", json!({"id":id})).unwrap();
    assert_eq!(done["lifecycle"], "Succeeded");
    assert_eq!(done["outcome_knowledge"], "Known");
    assert_eq!(
        receipt,
        json!({"work_item_ref":closed["reference"],"lifecycle":"InReview"})
    );
    store.transaction("verify-atomic-evidence-result", |db| {
        let sealed: i64 = db.c.query_row(
            "SELECT count(*) FROM versions WHERE kind='WorkItemEvidence' AND body->>'work_item_id'=?1",
            [id], |r| r.get(0),
        )?;
        assert_eq!(sealed, 1);
        let charged: (i64, i64) = db.c.query_row(
            "SELECT reserved,spent FROM budgets WHERE id=?1", [budget],
            |r| Ok((r.get(0)?,r.get(1)?)),
        )?;
        assert_eq!(charged, (0, 1));
        project::evidence::verify(db.c)
    }).unwrap();
}

#[test]
fn strict_model_lease_binding_is_witnessed_and_cannot_become_legacy() {
    use rusqlite::params;

    let directory = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(directory.path());
    let provider = call(
        &mut store,
        "Agent.Register",
        json!({"name":"binding-provider"}),
    )
    .unwrap();
    let model = call(
        &mut store,
        "Model.Register",
        json!({
            "name":"binding-model","revision":"1","provider_principal_id":provider["principal_id"],
            "max_context_bytes":16384,"max_output_bytes":8192,"lease_seconds":30,
            "inference_outcome_policy":"Strict",
        }),
    )
    .unwrap();
    let issue_provider = |store: &mut Store| {
        call(
            store,
            "Grant.Issue",
            json!({
                "principal_id":provider["principal_id"],"actions":["model.serve"],
                "tool_targets":[model["resource_id"]],"ttl_seconds":120,"units":1,
            }),
        )
        .unwrap()
    };
    let issuer = issue_provider(&mut store);
    let skill = call(
        &mut store,
        "Skill.Publish",
        json!({
            "name":"binding-skill","version":"1","license":"0BSD","source":"local",
            "instructions":"Read local status","tools":["system.status"],
        }),
    )
    .unwrap();
    let runner = call(
        &mut store,
        "Agent.Register",
        json!({"name":"binding-runner"}),
    )
    .unwrap();
    let authority = call(
        &mut store,
        "Grant.Issue",
        json!({
            "principal_id":runner["principal_id"],
            "actions":["session.run","task.submit","task.read","system.status"],
            "tool_targets":[model["resource_id"],"profile-local"],"ttl_seconds":120,
            "units":2,"model_calls":2,"model_lease_ms":60000,
        }),
    )
    .unwrap();
    let session = call_as(&mut store, "Session.Open", json!({
        "key":id("provider-binding"),"goal":"Check local status with an authority-bound inference lease",
        "skill_ref":skill,"model_ref":model,"context_policy":"ReferencesOnly","max_turns":2,
        "delegation_id":authority["delegation_ref"]["resource_id"],
    }), runner["token"].as_str().unwrap().into()).unwrap();
    let session = session["reference"]["resource_id"].as_str().unwrap();
    let provider_token = provider["token"].as_str().unwrap();
    let claim_request = json!({"session_id":session,"key":id("bound-claim")});
    let issued = call_as(
        &mut store,
        "Model.Claim",
        claim_request.clone(),
        provider_token.into(),
    )
    .unwrap();
    let lease = issued["lease_id"].as_str().unwrap();
    let replacement = issue_provider(&mut store);
    store.transaction("verify-model-provider-binding", |db| {
        use crate::agent::provider_authority::verify;
        verify(db.c)?;
        let (root, binding): (String, Value) = db.c.query_row(
            "SELECT root_grant,binding FROM model_provider_authorities WHERE lease=?1",
            [lease], |r| Ok((r.get(0)?,r.get(1)?)),
        )?;
        assert_eq!(issuer["grant_ref"]["resource_id"], root);
        assert_eq!(binding["model_ref"], model);
        assert_eq!(binding["delegation_refs"], json!([issuer["delegation_ref"]]));
        assert_eq!(binding["grant_ref"], issuer["grant_ref"]);

        db.c.execute_batch("SAVEPOINT altered_model_provider")?;
        assert_eq!(db.c.execute(
            "UPDATE model_provider_authorities SET root_grant=?1,binding=?2 WHERE lease=?3",
            params![replacement["grant_ref"]["resource_id"].as_str(),
                json!({"model_ref":model, "delegation_refs":[replacement["delegation_ref"]], "grant_ref":replacement["grant_ref"]}),
                lease],
        )?, 1);
        assert_eq!(verify(db.c).unwrap_err().0.code, "RecoveryRequired");
        db.c.execute_batch("ROLLBACK TO altered_model_provider; RELEASE altered_model_provider")?;
        db.c.execute_batch("SAVEPOINT missing_model_provider")?;
        assert_eq!(db.c.execute("DELETE FROM model_provider_authorities WHERE lease=?1", [lease])?, 1);
        assert_eq!(verify(db.c).unwrap_err().0.code, "RecoveryRequired");
        db.c.execute_batch("ROLLBACK TO missing_model_provider; RELEASE missing_model_provider")?;
        verify(db.c)
    }).unwrap();

    let unused = issue_provider(&mut store);
    let revoked = call(
        &mut store,
        "Grant.Revoke",
        json!({
            "id":unused["grant_ref"]["resource_id"],"expected_resource_version":unused["grant_ref"]["version"],
        }),
    ).unwrap();
    assert_eq!(revoked["status"], "Complete");
    let replay = call_as(
        &mut store,
        "Model.Claim",
        claim_request.clone(),
        provider_token.into(),
    )
    .unwrap();
    assert_eq!(replay["lease_id"], lease);
    assert_eq!(replay["context"], issued["context"]);

    let grant = &issuer["grant_ref"];
    let revoked = call(
        &mut store,
        "Grant.Revoke",
        json!({
            "id":grant["resource_id"],"expected_resource_version":grant["version"],
        }),
    )
    .unwrap();
    assert_eq!(revoked["status"], "Partial");
    assert_eq!(
        call_as(
            &mut store,
            "Model.Claim",
            claim_request,
            provider_token.into()
        )
        .unwrap_err()
        .0
        .code,
        "Fenced",
    );
    store
        .transaction("retain-historical-provider-binding", |db| {
            crate::agent::provider_authority::verify(db.c)
        })
        .unwrap();
}

#[test]
fn network_probe_projection_fences_older_receipts_and_rolls_back_with_task_result() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::local(&tmp.path().join("core"));
    let endpoint = "127.0.0.1:9".parse().unwrap();
    cfg.network_probes.push(crate::config::NetworkProbeConfig {
        name: "projection-test".into(),
        endpoint,
        timeout_ms: 100,
        dns: None,
    });
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg).unwrap();
    let listing = call(&mut store, "Resource.List", json!({"kind":"NetworkProbe"})).unwrap();
    let probe = &listing["items"][0];
    let states = call(
        &mut store,
        "Resource.List",
        json!({"kind":"NetworkProbeState"}),
    )
    .unwrap();
    let state_id = states["items"][0]["resource_id"].as_str().unwrap();
    let tool = node::network::probe::state::TOOL;
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":["task.submit","task.read",tool],
            "tool_targets":[probe["resource_id"]],"ttl_seconds":120,"units":8,
        }),
    )
    .unwrap();
    let enqueue = |store: &mut Store| {
        let ready = call(store, "Task.Prepare", json!({
            "tool":tool,"key":id("probe"),"delegation_id":grant["delegation_ref"]["resource_id"],
            "input":{"id":probe["resource_id"],"expected_resource_version":probe["version"]},
        })).unwrap();
        call(store, "Task.Submit", ready).unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let sample = |work: &Work, tcp: &str| {
        let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
        let namespace = fs::metadata("/proc/self/ns/net").unwrap().ino().to_string();
        json!({
            "target_ref":work.target.reference(),"boot_id":boot_id.trim(),"network_namespace_inode":namespace,
            "observed_at":now(),"observation":{"endpoint":endpoint,"dns":null,"tcp":{"status":tcp}},
        })
    };
    let original = enqueue(&mut store);
    assert_eq!(claim(&mut store), original);
    let old = dispatch(&mut store, &original);
    let latest = enqueue(&mut store);
    assert_eq!(claim(&mut store), latest);
    let new = dispatch(&mut store, &latest);
    store
        .transaction("settle-new-probe", |db| {
            broker::finish(db, &new, Ok(sample(&new, "Connected")))
        })
        .unwrap();
    let get_state =
        |store: &mut Store| call(store, "Resource.Get", json!({"id":state_id})).unwrap();
    let healthy = get_state(&mut store);
    assert_eq!(healthy["value"]["lifecycle"], "Healthy");
    store
        .transaction("settle-stale-probe", |db| {
            broker::finish(db, &old, Ok(sample(&old, "Refused")))
        })
        .unwrap();
    assert_eq!(get_state(&mut store)["reference"], healthy["reference"]);
    let stale = call(&mut store, "Task.Get", json!({"id":original})).unwrap();
    let result = call(
        &mut store,
        "Artifact.Read",
        json!({"reference":stale["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    assert_eq!(result["projection"], "Superseded");

    let outage = enqueue(&mut store);
    assert_eq!(claim(&mut store), outage);
    let outage = dispatch(&mut store, &outage);
    store
        .transaction("inject-probe-receipt-write-failure", |db| {
            db.c.execute_batch(
                "CREATE TEMP TRIGGER fail_probe_receipt BEFORE INSERT ON versions
            WHEN NEW.kind='Artifact' BEGIN SELECT RAISE(ABORT,'probe receipt rolled back'); END",
            )?;
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .transaction("abort-probe-settlement", |db| broker::finish(
                db,
                &outage,
                Ok(sample(&outage, "Refused"))
            ))
            .is_err()
    );
    assert_eq!(get_state(&mut store)["reference"], healthy["reference"]);
    store
        .transaction("remove-probe-receipt-write-failure", |db| {
            db.c.execute_batch("DROP TRIGGER fail_probe_receipt")?;
            node::network::probe::state::verify(db.c)
        })
        .unwrap();
    store
        .transaction("commit-probe-settlement", |db| {
            broker::finish(db, &outage, Ok(sample(&outage, "Refused")))
        })
        .unwrap();
    let unhealthy = get_state(&mut store);
    assert_eq!(unhealthy["value"]["lifecycle"], "Unhealthy");
    assert_eq!(unhealthy["value"]["reason"], "TcpRefused");
    store
        .transaction("verify-probe-provenance", |db| {
            node::network::probe::state::verify(db.c)
        })
        .unwrap();
}

#[test]
fn witnessed_unpublished_restore_reconciles_without_replaying_and_resumes_disposal() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let (cfg, mut store) = initialized(directory.path());
    let target = |reference: &Value| {
        json!({
            "id":reference["resource_id"],"expected_resource_version":reference["version"],
        })
    };
    let volume = submit(
        &mut store,
        "volume.create",
        json!({"name":"restore-stage-custody"}),
    );
    let volume = execute(&mut store, &volume);
    let backup = submit(&mut store, "backup.create", target(&volume));
    let backup = execute(&mut store, &backup);
    let restoration = submit(&mut store, "backup.restore", target(&backup));
    assert_eq!(claim(&mut store), restoration);
    let work = dispatch(&mut store, &restoration);
    let result = work.perform(|checkpoint| {
        store.transaction("restore-checkpoint", |db| work.checkpoint(db, checkpoint))?;
        Err(err(
            "OutcomeUnknown",
            "test interruption before the restore rename",
        ))
    });
    assert_eq!(result.unwrap_err().0.code, "OutcomeUnknown");
    let operation_id = work.op.id.clone();
    let checkpoint = call(
        &mut store,
        "Resource.Get",
        json!({"id":format!("restore-publication-{operation_id}")}),
    )
    .unwrap();
    let volumes = cfg.state_dir.join("volumes");
    let stage_name = checkpoint["value"]["placement"]["stage_name"]
        .as_str()
        .unwrap();
    let staged = volumes.join(stage_name);
    let final_path = volumes.join(checkpoint["value"]["receipt"]["id"].as_str().unwrap());
    assert!(staged.is_dir() && !final_path.exists());
    drop(store);

    let mut store = Store::open(cfg.clone()).unwrap();
    let retained = store
        .transaction(
            "restore-stage-custody",
            storage::restore_publication::protected_stages,
        )
        .unwrap();
    assert!(retained.contains(stage_name));
    storage::reclaim_on_startup(&cfg, Some(&retained));
    store
        .transaction("recover-restore", broker::recover)
        .unwrap();
    let pending = call(&mut store, "Task.Get", json!({"id":restoration})).unwrap();
    assert_eq!(pending["outcome_knowledge"], "Unknown");
    assert!(staged.is_dir() && !final_path.exists());
    store
        .transaction("verify-active-restore-custody", |db| {
            use storage::restore_publication::{protected_stages, verify};
            let intent = format!("restore-intent-{operation_id}");
            let checkpoint = format!("restore-publication-{operation_id}");
            for (statement, target) in [
                ("DELETE FROM versions WHERE id=?1", &intent),
                ("DELETE FROM versions WHERE id=?1", &checkpoint),
                ("DELETE FROM holds WHERE holder=?1", &operation_id),
                ("DELETE FROM queue WHERE operation=?1", &operation_id),
            ] {
                db.c.execute_batch("SAVEPOINT damage_restore_custody")?;
                assert!(db.c.execute(statement, [target])? > 0);
                assert_eq!(verify(db.c).unwrap_err().0.code, "RecoveryRequired");
                assert_eq!(protected_stages(db).unwrap_err().0.code, "RecoveryRequired");
                db.c.execute_batch(
                    "ROLLBACK TO damage_restore_custody; RELEASE damage_restore_custody",
                )?;
            }
            verify(db.c)
        })
        .unwrap();
    let original = &pending["operation_refs"][0];
    let tool = storage::restore_publication::INSPECT;
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":["task.submit","task.read",tool],
            "tool_targets":[operation_id],"ttl_seconds":90,"units":4,
        }),
    )
    .unwrap();
    let request = call(
        &mut store,
        "Task.Prepare",
        json!({
            "tool":tool,"input":target(original),"key":id("restore-observation"),
            "delegation_id":grant["delegation_ref"]["resource_id"],
        }),
    )
    .unwrap();
    let accepted = call(&mut store, "Task.Submit", request).unwrap();
    let inspected = accepted["task_id"].as_str().unwrap();
    let proof = execute(&mut store, inspected);
    assert_eq!(proof["publication"], "NotApplied");
    let decision = call(&mut store, "Task.Get", json!({"id":inspected})).unwrap();
    let mut reconcile = target(original);
    reconcile["evidence_ref"] = decision["result"]["artifact_refs"][0].clone();
    fs::create_dir(&final_path).unwrap();
    assert_eq!(
        call(&mut store, "Operation.Reconcile", reconcile.clone())
            .unwrap_err()
            .0
            .code,
        "PreconditionFailed"
    );
    fs::remove_dir(&final_path).unwrap();
    let known = call(&mut store, "Operation.Reconcile", reconcile).unwrap();
    assert_eq!(
        (
            known["lifecycle"].as_str(),
            known["outcome_knowledge"].as_str()
        ),
        (Some("Failed"), Some("Known"))
    );
    assert!(!final_path.exists());
    let job = store
        .transaction("restore-disposal-ready", |db| {
            assert!(!db.c.query_row(
                "SELECT EXISTS(SELECT 1 FROM holds WHERE holder=?1)",
                [&operation_id],
                |r| r.get::<_, bool>(0)
            )?);
            use storage::restore_publication::{Disposal, next_disposal, protected_stages, verify};
            assert_eq!(
                next_disposal(db, "")?.as_deref(),
                Some(operation_id.as_str())
            );
            verify(db.c)?;
            for statement in [
                "DELETE FROM restore_stage_disposals WHERE operation=?1",
                "UPDATE restore_stage_disposals SET checkpoint_ref='{}' WHERE operation=?1",
                "UPDATE restore_stage_disposals SET evidence_ref='{}' WHERE operation=?1",
            ] {
                db.c.execute_batch("SAVEPOINT damage_restore_disposal")?;
                assert_eq!(db.c.execute(statement, [&operation_id])?, 1);
                assert_eq!(verify(db.c).unwrap_err().0.code, "RecoveryRequired");
                assert_eq!(protected_stages(db).unwrap_err().0.code, "RecoveryRequired");
                db.c.execute_batch(
                    "ROLLBACK TO damage_restore_disposal; RELEASE damage_restore_disposal",
                )?;
            }
            Disposal::open(db, &operation_id)
        })
        .unwrap();
    let post_settlement = store
        .transaction(
            "discard-custody",
            storage::restore_publication::protected_stages,
        )
        .unwrap();
    assert!(post_settlement.contains(stage_name));
    let trash = volumes.join(format!(".restore-discard-{operation_id}"));
    fs::create_dir(&trash).unwrap();
    assert!(job.run(&cfg).is_err());
    assert!(staged.is_dir());
    fs::remove_dir(&trash).unwrap();
    // Resume a quarantine already renamed and partially unlinked before a crash.
    fs::rename(&staged, &trash).unwrap();
    fs::remove_file(trash.join("identity")).unwrap();
    assert!(job.run(&cfg).unwrap());
    let removed = job.run(&cfg).unwrap();
    assert!(!removed);
    assert!(!staged.exists() && !trash.exists() && !final_path.exists());
    store
        .transaction("restore-disposal-finished", |db| {
            job.finish(db, removed)?;
            storage::restore_publication::verify(db.c)
        })
        .unwrap();
    drop(store);
    assert_eq!(
        Store::verify(&cfg, directory.path(), 64 * 1024 * 1024).unwrap()["database_format"],
        crate::db::CURRENT_FORMAT,
    );
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
fn approved_network_trial_fences_interrupted_confirmation_and_rollback() {
    use crate::node::network::{managed, trial};
    use crate::policy::deadline::Clock;
    use host::{Checkpoint::NetworkTrial, Target};
    use trial::Boundary;

    for interruption in [
        "cancel",
        "revoke",
        "lost-confirm",
        "restart",
        "mismatched-rollback",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut cfg = Config::local(&directory.path().join("core"));
        let initial = "1aad7685-9a7d-49ad-8132-0c9c56cfb34e";
        let candidate = "597da7b3-c8ba-4134-bfa7-57377f1ab670";
        let mac = "02:50:53:00:00:01";
        let link = "network-link-025053000001";
        cfg.network_link = Some(config::NetworkLinkConfig {
            interface: "private-spare".into(),
            mac: mac.into(),
            original_uuid: initial.into(),
            candidate_uuid: candidate.into(),
            confirmation_seconds: 40,
        });
        Store::init(&cfg).unwrap();
        let mut store = Store::open(cfg.clone()).unwrap();
        let grant = call(
            &mut store,
            "Grant.Issue",
            json!({
                "principal_id":policy::OWNER, "actions":["task.submit","task.read",trial::TOOL],
                "tool_targets":[link], "ttl_seconds":120, "units":4,
            }),
        )
        .unwrap();
        let link_ref =
            call(&mut store, "Resource.Get", json!({"id":link})).unwrap()["reference"].clone();
        let request = call(&mut store, "Task.Prepare", json!({
            "tool":trial::TOOL,"input":{"id":link,"expected_resource_version":link_ref["version"]},
            "delegation_id":grant["delegation_ref"]["resource_id"],"key":id("network-trial"),
        })).unwrap();
        let accepted = call(&mut store, "Task.Submit", request).unwrap();
        let task_id = accepted["task_id"].as_str().unwrap();
        assert_eq!(
            call(&mut store, "Task.Get", json!({"id":task_id})).unwrap()["lifecycle"],
            "WaitingApproval"
        );
        approve(&mut store, task_id);
        assert_eq!(claim(&mut store), task_id);
        let work = dispatch(&mut store, task_id);
        assert_eq!(work.effect.body["phase"], "PermitBound");

        // Feed typed adapter receipts through the real witnessed broker transactions.
        // The isolated NetworkManager integration test exercises the backend itself.
        let clock = Clock::sample().unwrap();
        let snapshot = json!({
            "identity":{"boot_id":clock.boot.boot_id,"network_namespace_device":1,
                "network_namespace_inode":2,"time_namespace_device":1,"time_namespace_inode":3,
                "bus_id":"11111111111111111111111111111111",
                "transport_guid":"22222222222222222222222222222222",
                "manager_owner":":1.2","manager_pid":12},
            "link":{"interface":"private-spare","ifindex":2,"mac":mac},
            "device_path":"/org/freedesktop/NetworkManager/Devices/2",
            "profiles":[
                {"uuid":initial,"path":"/org/freedesktop/NetworkManager/Settings/1",
                 "ipv4":{"address":"192.0.2.2","prefix":24},"settings_digest":"original"},
                {"uuid":candidate,"path":"/org/freedesktop/NetworkManager/Settings/2",
                 "ipv4":{"address":"192.0.2.3","prefix":24},"settings_digest":"candidate"}
            ],
            "baseline":{"path":"/org/freedesktop/NetworkManager/ActiveConnection/1","profile_uuid":initial}
        });
        let typed_scope: managed::Snapshot = decode(&snapshot).unwrap();
        let start = clock.boot.millis;
        let checkpoint_path = "/org/freedesktop/NetworkManager/Checkpoint/1";
        let checkpoint: managed::Checkpoint = decode(&json!({
            "path":checkpoint_path, "snapshot_digest":psios_api::sha256(&psios_api::canonical(&snapshot)),
            "created_boottime_ms":start,"rollback_timeout_seconds":40,"deadline_boottime_ms":start+40000,
        })).unwrap();
        let active = json!({"path":"/org/freedesktop/NetworkManager/ActiveConnection/2","profile_uuid":candidate});
        let activation: managed::Activation =
            decode(&json!({"checkpoint_path":checkpoint_path,"active":active})).unwrap();
        for boundary in [
            Boundary::BeforeCreate(&typed_scope, start),
            Boundary::Created(&checkpoint),
            Boundary::BeforeActivate,
            Boundary::Activated(&activation),
        ] {
            store
                .transaction("network-trial-boundary", |db| {
                    work.checkpoint(db, NetworkTrial(boundary))
                })
                .unwrap();
        }
        let observed = json!({"checkpoint_present":true,"active":active});
        store
            .transaction("network-trial-park", |db| {
                broker::finish(db, &work, Ok(observed.clone()))
            })
            .unwrap();
        let waiting = call(&mut store, "Task.Get", json!({"id":task_id})).unwrap();
        assert_eq!(waiting["lifecycle"], "WaitingInput");
        assert_eq!(waiting["outcome_knowledge"], "InFlight");
        assert_eq!(waiting["conditions"][0]["type"], "AwaitingConfirmation");
        let operation: Target = decode(&json!({
            "id":waiting["operation_refs"][0]["resource_id"],
            "expected_resource_version":waiting["operation_refs"][0]["version"],
        }))
        .unwrap();
        let query = serde_json::to_value(&operation).unwrap();
        let challenge = store
            .transaction("network-trial-get-confirmation", |db| {
                broker::network_trial_confirmation(db, policy::OWNER, &query)
            })
            .unwrap();
        let mut confirmation = query.clone();
        confirmation["confirmation_token"] = challenge["confirmation_token"].clone();
        let first = store
            .transaction("network-trial-confirm-first", |db| {
                broker::prepare_network_trial_confirm(db, policy::OWNER, &confirmation)
            })
            .unwrap();
        let second = store
            .transaction("network-trial-confirm-second", |db| {
                broker::prepare_network_trial_confirm(db, policy::OWNER, &confirmation)
            })
            .unwrap();
        let ordinary = submit(&mut store, "system.status", json!({}));
        execute(&mut store, &ordinary);
        store
            .transaction("network-late-receipt", |db| {
                broker::finish(db, &work, Ok(observed))
            })
            .unwrap();
        store
            .transaction("network-custody-check", |db| {
                trial::verify_active(db.c, crate::db::CURRENT_FORMAT)
            })
            .unwrap();
        match interruption {
            "cancel" => {
                call(
                    &mut store,
                    "Task.Cancel",
                    json!({
                        "id":task_id,"expected_resource_version":waiting["task_revision"],
                    }),
                )
                .unwrap();
            }
            "revoke" => {
                let revoked = call(
                    &mut store,
                    "Grant.Revoke",
                    json!({
                        "id":grant["grant_ref"]["resource_id"],
                        "expected_resource_version":grant["grant_ref"]["version"],
                    }),
                )
                .unwrap();
                assert_eq!(revoked["status"], "Partial");
                assert!(
                    store
                        .transaction("network-trial-denied-after-revoke", |db| {
                            broker::authorize_network_trial_confirm(
                                db,
                                policy::OWNER,
                                &confirmation,
                                &first,
                            )
                        })
                        .is_err()
                );
                store
                    .transaction(
                        "network-trial-expire-revoked",
                        broker::expire_network_trials,
                    )
                    .unwrap();
            }
            "lost-confirm" => {
                store
                    .transaction("network-trial-issue-first", |db| {
                        broker::authorize_network_trial_confirm(
                            db,
                            policy::OWNER,
                            &confirmation,
                            &first,
                        )
                    })
                    .unwrap();
                assert!(
                    store
                        .transaction("network-trial-deny-concurrent", |db| {
                            broker::authorize_network_trial_confirm(
                                db,
                                policy::OWNER,
                                &confirmation,
                                &second,
                            )
                        })
                        .is_err()
                );
                store
                    .transaction("network-trial-lost-confirm", |db| {
                        broker::finish_network_trial_confirm(
                            db,
                            policy::OWNER,
                            &first,
                            Err(err(
                                "OutcomeUnknown",
                                "fixture lost checkpoint destruction receipt",
                            )),
                        )
                    })
                    .unwrap();
            }
            "restart" => {
                drop(store);
                store = Store::open(cfg).unwrap();
                store
                    .transaction("network-trial-restart", broker::recover)
                    .unwrap();
            }
            "mismatched-rollback" => {
                let rollback = store
                    .transaction("prepare-rollback", |db| {
                        broker::prepare_network_trial_rollback(db, policy::OWNER, &query)
                    })
                    .unwrap();
                store
                    .transaction("dispatch-rollback", |db| {
                        broker::authorize_network_trial_rollback(
                            db,
                            policy::OWNER,
                            &query,
                            &rollback,
                        )
                    })
                    .unwrap();
                store
                    .transaction("fence-network-trial-decisions", |db| {
                        assert!(
                            broker::authorize_network_trial_confirm(
                                db,
                                policy::OWNER,
                                &confirmation,
                                &first
                            )
                            .is_err()
                        );
                        assert!(
                            broker::authorize_network_trial_rollback(
                                db,
                                policy::OWNER,
                                &query,
                                &rollback
                            )
                            .is_err()
                        );
                        Ok(())
                    })
                    .unwrap();
                let wrong_device: trial::RollbackReceipt = decode(&json!({
                    "results":{"/org/freedesktop/NetworkManager/Devices/19":0},
                    "observed":{"checkpoint_present":false,
                        "active":{"path":"/org/freedesktop/NetworkManager/ActiveConnection/3",
                            "profile_uuid":initial}}
                }))
                .unwrap();
                store
                    .transaction("persist-unverified-rollback-outcome", |db| {
                        broker::finish_network_trial_rollback(
                            db,
                            policy::OWNER,
                            &rollback,
                            Ok(wrong_device),
                        )
                    })
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let pending = call(&mut store, "Task.Get", json!({"id":task_id})).unwrap();
        assert_eq!(pending["outcome_knowledge"], "Unknown", "{interruption}");
        assert_eq!(
            pending["lifecycle"],
            if interruption == "cancel" {
                "Cancelled"
            } else {
                "Paused"
            }
        );
        assert_eq!(pending["result"]["availability"], "Unavailable");
        store
            .transaction("network-trial-retains-held-link", |db| {
                trial::verify_active(db.c, crate::db::CURRENT_FORMAT)?;
                let holder: String =
                    db.c.query_row("SELECT holder FROM holds WHERE volume=?1", [link], |r| {
                        r.get(0)
                    })?;
                assert_eq!(holder, operation.id);
                assert!(
                    broker::prepare_network_trial_confirm(db, policy::OWNER, &confirmation)
                        .is_err()
                );
                assert!(broker::prepare_network_trial_rollback(db, policy::OWNER, &query).is_err());
                let late: trial::ConfirmationReceipt =
                    decode(&json!({"checkpoint_present":false,"active":active}))?;
                assert!(
                    broker::finish_network_trial_confirm(db, policy::OWNER, &second, Ok(late))
                        .is_err()
                );
                Ok(())
            })
            .unwrap();
    }
}

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
        let mut cleanup = storage::reclamation::Cursor::default();
        assert!(
            reopened
                .transaction("no-cleanup", |db| storage::retirement::next(
                    db,
                    &mut cleanup
                ))
                .unwrap()
                .is_none()
        );

        let retried = submit(&mut reopened, "volume.delete", deletion);
        approve(&mut reopened, &retried);
        let receipt = execute(&mut reopened, &retried);
        assert_eq!(receipt["volume_ref"]["resource_id"], volume["resource_id"]);
        assert!(!directory.exists());
        let job = reopened
            .transaction("inspect-cleanup", |db| {
                storage::retirement::next(db, &mut cleanup)
            })
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
fn task_submission_window_checks_the_exact_ancestors_elapsed_cutoff() {
    let root = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(root.path());
    let parent = call(
        &mut store,
        "Agent.Register",
        json!({"name":"window-parent"}),
    )
    .unwrap();
    let intermediate = call(
        &mut store,
        "Agent.Register",
        json!({"name":"window-intermediate"}),
    )
    .unwrap();
    let leaf = call(&mut store, "Agent.Register", json!({"name":"window-leaf"})).unwrap();
    call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":leaf["principal_id"],"actions":["task.read"],"units":1,"ttl_seconds":180}),
    )
    .unwrap();
    let actions = json!(["task.submit", "task.read", "system.status"]);
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":parent["principal_id"], "actions":actions, "units":2, "ttl_seconds":180,
            "allow_delegation":true, "max_delegation_depth":2, "tool_targets":["profile-local"]}),
    )
    .unwrap();
    let upstream = call_as(
        &mut store,
        "Delegation.Create",
        json!({"parent_id":grant["delegation_ref"]["resource_id"], "principal_id":intermediate["principal_id"],
            "actions":actions, "ttl_seconds":160, "allow_delegation":true}),
        parent["token"].as_str().unwrap().to_owned(),
    )
    .unwrap();
    let delegation = call_as(
        &mut store,
        "Delegation.Create",
        json!({"parent_id":upstream["resource_id"], "principal_id":leaf["principal_id"],
            "actions":actions, "ttl_seconds":120}),
        intermediate["token"].as_str().unwrap().to_owned(),
    )
    .unwrap();
    let token = leaf["token"].as_str().unwrap().to_owned();
    let prepare = |store: &mut Store, key| {
        call_as(
            store,
            "Task.Prepare",
            json!({"key":key,"tool":"system.status","input":{},"delegation_id":delegation["resource_id"]}),
            token.clone(),
        )
        .unwrap()
    };
    let existing = prepare(&mut store, "long-lived-draft");
    let lookup = |store: &mut Store, request: &Value| {
        call_as(
            store,
            "Task.LookupSubmission",
            request.clone(),
            token.clone(),
        )
        .unwrap()
    };
    let remaining = |reply: Value| {
        assert_eq!(reply["status"], "AbsentWithinWindow", "{reply}");
        let until =
            chrono::DateTime::parse_from_rfc3339(reply["window_not_after"].as_str().unwrap())
                .unwrap();
        (until.to_utc() - chrono::Utc::now()).num_milliseconds()
    };
    assert!(remaining(lookup(&mut store, &existing)) > 75_000);
    let adjust = |store: &mut Store, remaining_ms: i64| {
        store
            .transaction("test-intermediate-deadline", |db| {
                let cutoff = policy::deadline::Clock::sample()?.boot.millis + remaining_ms;
                assert_eq!(
                    db.c.execute(
                        "UPDATE authorization_deadlines SET deadline_boot_ms=?1 WHERE resource=?2",
                        rusqlite::params![cutoff, upstream["resource_id"].as_str().unwrap()],
                    )?,
                    1
                );
                Ok(cutoff)
            })
            .unwrap()
    };
    let capped = adjust(&mut store, 25_000);
    let updated = prepare(&mut store, "bounded-fresh-draft");
    store
        .transaction("inspect-preparation-cutoff", |db| {
            let stored: i64 = db.c.query_row(
                "SELECT deadline_boot_ms FROM task_preparations WHERE owner=?1 AND key=?2",
                rusqlite::params![
                    leaf["principal_id"].as_str().unwrap(),
                    "bounded-fresh-draft"
                ],
                |r| r.get(0),
            )?;
            assert_eq!(stored, capped);
            Ok(())
        })
        .unwrap();
    drop(store);
    let mut store = Store::open(config).unwrap();
    for request in [&existing, &updated] {
        assert!((0..=25_000).contains(&remaining(lookup(&mut store, request))));
    }
    adjust(&mut store, -1);
    for request in [existing, updated] {
        assert_eq!(lookup(&mut store, &request)["status"], "Expired");
        assert_eq!(
            call_as(&mut store, "Task.Submit", request, token.clone())
                .unwrap_err()
                .0
                .code,
            "PermissionDenied"
        );
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
    let request = Request {
        schema_version: VERSION.into(),
        request_id: id("file-proof-cas"),
        token: fs::read_to_string(c.witness_dir.join("owner.token")).unwrap(),
        method: "Operation.Reconcile".into(),
        params: p.clone(),
    };
    let peer = rustix::process::geteuid().as_raw();
    let sampled = s
        .prepare_reconcile(peer, &request)
        .unwrap()
        .unwrap()
        .read()
        .unwrap();
    let replacement = file.with_file_name("same-bytes-new-inode");
    fs::write(&replacement, "verified").unwrap();
    fs::rename(&replacement, &file).unwrap();
    assert_eq!(
        s.finish_reconcile(peer, &request, Ok(Some(sampled)))
            .unwrap_err()
            .0
            .code,
        "OutcomeUnknown"
    );
    let still_held = call(&mut s, "Task.Get", json!({"id": task})).unwrap();
    assert_eq!(still_held["operation_refs"], t["operation_refs"]);
    assert_eq!(still_held["outcome_knowledge"], "Unknown");
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
fn workflow_cancel_between_session_launch_and_observation_stops_the_original_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(tmp.path());
    let provider = call(
        &mut store,
        "Agent.Register",
        json!({"name":"joined-provider"}),
    )
    .unwrap();
    let model = call(
        &mut store, "Model.Register",
        json!({"name":"joined-model","revision":"1","provider_principal_id":provider["principal_id"],
               "inference_outcome_policy":"Strict","lease_seconds":30,"max_context_bytes":8192,
               "max_output_bytes":2048}),
    ).unwrap();
    call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":provider["principal_id"],"actions":["model.serve"],
               "tool_targets":[model["resource_id"]],"units":1,"ttl_seconds":120}),
    )
    .unwrap();
    let skill = call(
        &mut store,
        "Skill.Publish",
        json!({"name":"joined-skill","version":"1","license":"0BSD","source":"local-test",
               "tools":["system.status"],"instructions":"Read local status."}),
    )
    .unwrap();
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,
               "actions":["workflow.manage","agent.session.start","session.run","task.submit",
                          "task.read","system.status"],
               "tool_targets":[model["resource_id"],skill["resource_id"],"profile-local"],
               "units":3,"model_calls":2,"ttl_seconds":120}),
    )
    .unwrap();
    let definition = call(
        &mut store,
        "Workflow.Define",
        json!({"key":"joined-cancellation","steps":[
            {"id":"launch","tool":"agent.session.start","after":[],"completion":"SessionTerminal",
             "input":{"goal":"Read local status.","skill_ref":skill,"model_ref":model,
                      "context_policy":"ReferencesOnly","max_turns":2}},
            {"id":"after","tool":"system.status","input":{},"after":["launch"]}
        ]}),
    )
    .unwrap();
    let run = call(
        &mut store,
        "Workflow.Start",
        json!({"key":"joined-cancellation","definition_ref":definition["reference"],
               "delegation_id":grant["delegation_ref"]["resource_id"],"deadline_at":after(60)}),
    )
    .unwrap();
    let run_id = run["reference"]["resource_id"].as_str().unwrap();
    workflow::tick(&mut store).unwrap();
    let task = claim(&mut store);
    assert!(
        store
            .transaction("atomic-session-launch", |db| broker::dispatch(db, &task))
            .unwrap()
            .is_none()
    );
    let launch = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(launch["lifecycle"], "Succeeded");
    let receipt = call(
        &mut store,
        "Artifact.Read",
        json!({"reference":launch["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    let session = receipt["session_ref"]["resource_id"].as_str().unwrap();
    let still_unobserved = call(&mut store, "Workflow.Get", json!({"id":run_id})).unwrap();
    assert!(still_unobserved["state"]["steps"][0]["session_ref"].is_null());
    let cancelled = call(
        &mut store,
        "Workflow.Cancel",
        json!({"id":run_id,"expected_resource_version":still_unobserved["reference"]["version"]}),
    )
    .unwrap();
    assert_eq!(cancelled["state"]["lifecycle"], "Cancelled");
    assert_eq!(
        cancelled["state"]["steps"][0]["session_lifecycle"],
        "Cancelled"
    );
    assert!(cancelled["state"]["steps"][1]["task_receipt"].is_null());
    let observed = call(&mut store, "Session.Get", json!({"id":session})).unwrap();
    assert_eq!(
        cancelled["state"]["steps"][0]["session_ref"],
        observed["reference"]
    );
    let next = call_as(
        &mut store,
        "Model.ClaimNext",
        json!({"model_ref":model,"key":"cancelled-before-observation"}),
        provider["token"].as_str().unwrap().into(),
    )
    .unwrap();
    assert_eq!(next, json!({"available":false}));
    store
        .transaction("verify-cancelled-session-launch", |db| {
            workflow::session_join::verify(db.c)?;
            let clock = policy::deadline::Clock::sample()?;
            let cutoff = |resource: &str| {
                db.c.query_row(
            "SELECT deadline_boot_ms FROM authorization_deadlines WHERE resource=?1 AND boot_id=?2",
            rusqlite::params![resource,clock.boot.boot_id], |row| row.get::<_,i64>(0),
        )
            };
            assert!(cutoff(session)? <= cutoff(run_id)?);
            db.c.execute_batch("SAVEPOINT lost_workflow_join")?;
            db.c.execute(
                "DELETE FROM workflow_session_joins WHERE session=?1",
                [session],
            )?;
            assert_eq!(
                workflow::session_join::verify(db.c).unwrap_err().0.code,
                "RecoveryRequired"
            );
            db.c.execute_batch("ROLLBACK TO lost_workflow_join; RELEASE lost_workflow_join")?;
            Ok(())
        })
        .unwrap();
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

fn witness_fixture_create(
    db: &crate::db::Db<'_>,
    work: &host::Work,
    engine: &str,
    container: &str,
) -> Result<()> {
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    work.checkpoint(
        db,
        host::Checkpoint::OciCreateIssued {
            engine,
            attempt: &"ca".repeat(32),
            boot: boot.trim(),
        },
    )?;
    work.checkpoint(db, host::Checkpoint::OciCreateAcknowledged(container))
}

#[test]
fn oci_restart_custody_fences_missing_holds_and_corrupt_bound_samples() {
    let directory = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(directory.path());
    let (install, _) = accepted_artifact_install(&mut store);
    let installed = settle_oci_catalog(&mut store, &install);
    let app = &installed["application_ref"];
    let task = submit(
        &mut store,
        "application.start",
        json!({"id":app["resource_id"],"expected_resource_version":app["version"]}),
    );
    approve(&mut store, &task);
    assert_eq!(claim(&mut store), task);
    let start = dispatch(&mut store, &task);
    let container = "b".repeat(64);
    let app = store
        .transaction("restart-fixture-start", |db| {
            witness_fixture_create(db, &start, "restart-fixture-engine", &container)?;
            start.checkpoint(
                db,
                host::Checkpoint::OciStart {
                    engine: "restart-fixture-engine",
                    container: &container,
                },
            )?;
            broker::finish(
                db,
                &start,
                Ok(
                    json!({"engine_id":"restart-fixture-engine","container_id":container,
            "process_running":true,"observed_at":now()}),
                ),
            )?;
            Ok(db.of(&start.target.id, "Application")?.reference())
        })
        .unwrap();
    let grant = call(&mut store,"Grant.Issue",json!({"principal_id":"local-owner","units":4,"ttl_seconds":90,
        "actions":["task.submit","task.read",apps::restart::TOOL],"tool_targets":[app["resource_id"]]})).unwrap();
    let boot = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let observed = json!({"application_ref":app,"engine_id":"restart-fixture-engine","container_id":container,
        "start_operation_id":start.op.id,"boot_id":boot.trim(),"started_at":"2026-11-25T12:00:00Z",
        "process_id":1,"process_running":true,"runtime_state":"Running","readiness":"Unknown","health":"Unknown",
        "effective_limits":{"source":"DockerCgroupV1"},"observed_at":now()});
    let task = call(
        &mut store,
        "Task.Prepare",
        json!({"tool":apps::restart::TOOL,"key":id("restart-custody"),
        "delegation_id":grant["delegation_ref"]["resource_id"],"input":{"id":app["resource_id"],
            "expected_resource_version":app["version"],"expected_boot_id":observed["boot_id"],
            "expected_started_at":observed["started_at"]}}),
    )
    .unwrap();
    let accepted = call(&mut store, "Task.Submit", task).unwrap();
    let task = accepted["task_id"].as_str().unwrap();
    approve(&mut store, task);
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    assert_eq!(work.effect.body["phase"], "PermitBound");
    store
        .transaction("restart-fixture-checkpoint", |db| {
            work.checkpoint(db, host::Checkpoint::OciRestart(&observed))?;
            apps::restart::verify(db.c, apps::restart::CUSTODY_FORMAT)
        })
        .unwrap();

    store
        .transaction("restart-fixture-recover", broker::recover)
        .unwrap();
    let paused = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(paused["outcome_knowledge"], "Unknown");
    store.transaction("restart-fixture-corruption", |db| {
        apps::restart::verify(db.c,apps::restart::CUSTODY_FORMAT)?;
        db.c.execute_batch("SAVEPOINT lost_restart_hold")?;
        db.c.execute("DELETE FROM holds WHERE holder=?1",[&work.op.id])?;
        assert_eq!(apps::restart::verify_active(db.c).unwrap_err().0.code,"RecoveryRequired");
        assert_eq!(apps::restart::verify(db.c,apps::restart::CUSTODY_FORMAT).unwrap_err().0.code,"RecoveryRequired");
        db.c.execute_batch("ROLLBACK TO lost_restart_hold; RELEASE lost_restart_hold")?;
        db.c.execute_batch("SAVEPOINT changed_restart_sample")?;
        db.c.execute("UPDATE versions SET body=json_set(body,'$.before.started_at','2025-11-25T12:00:00Z')
            WHERE kind='OCIRestartCheckpoint' AND spec->>'operation_id'=?1",[&work.op.id])?;
        assert_eq!(apps::restart::verify(db.c,apps::restart::CUSTODY_FORMAT).unwrap_err().0.code,"RecoveryRequired");
        db.c.execute_batch("ROLLBACK TO changed_restart_sample; RELEASE changed_restart_sample")?;
        apps::restart::verify(db.c,apps::restart::CUSTODY_FORMAT)
    }).unwrap();
}

#[test]
fn oci_stop_recovery_rejects_missing_and_mismatched_active_custody() {
    let directory = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(directory.path());
    let (install, _) = accepted_artifact_install(&mut store);
    let installed = settle_oci_catalog(&mut store, &install);
    let target = |reference: &Value| {
        json!({
            "id":reference["resource_id"],"expected_resource_version":reference["version"],
        })
    };
    let start = submit(
        &mut store,
        "application.start",
        target(&installed["application_ref"]),
    );
    approve(&mut store, &start);
    assert_eq!(claim(&mut store), start);
    let start = dispatch(&mut store, &start);
    let container = "a".repeat(64);
    let running = json!({"engine_id":"oci-stop-fixture-engine","container_id":container,
        "process_running":true,"observed_at":now()});
    let app = store
        .transaction("start-fixture", |db| {
            witness_fixture_create(db, &start, "oci-stop-fixture-engine", &container)?;
            start.checkpoint(
                db,
                host::Checkpoint::OciStart {
                    engine: "oci-stop-fixture-engine",
                    container: &container,
                },
            )?;
            broker::finish(db, &start, Ok(running))?;
            Ok(db.of(&start.target.id, "Application")?.reference())
        })
        .unwrap();
    let stop_task = submit(&mut store, "application.stop", target(&app));
    assert_eq!(claim(&mut store), stop_task);
    let initial_stop = dispatch(&mut store, &stop_task);
    assert_eq!(initial_stop.effect.body["phase"], "PermitBound");
    store
        .transaction("recover-before-stop", broker::recover)
        .unwrap();
    let not_started = call(&mut store, "Task.Get", json!({"id":stop_task})).unwrap();
    assert_eq!(not_started["outcome_knowledge"], "NotStarted");
    let retried = call(&mut store, "Task.Retry", json!({
        "id":stop_task,"expected_resource_version":not_started["task_revision"],"key":id("retry-stop"),
    })).unwrap();
    let stop_task = retried["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), stop_task);
    let stop = dispatch(&mut store, stop_task);
    assert_ne!(stop.op.id, initial_stop.op.id);
    let before = json!({"engine_id":"oci-stop-fixture-engine","container_id":container,
        "start_operation_id":start.op.id,"started_at":"2026-11-25T12:00:00Z",
        "docker_state":"running","process_running":true,"runtime_state":"Running"});
    store
        .transaction("stop-checkpoint", |db| {
            stop.checkpoint(db, host::Checkpoint::OciStop(&before))
        })
        .unwrap();
    store.transaction("recover-stop", broker::recover).unwrap();
    let stopped = call(&mut store, "Task.Get", json!({"id":stop_task})).unwrap();
    assert_eq!(stopped["outcome_knowledge"], "Unknown");
    let original = &stopped["operation_refs"][0];
    let health = submit(&mut store, "application.health", target(&app));
    assert_eq!(claim(&mut store), health);
    let work = dispatch(&mut store, &health);
    let mut exited = before;
    exited["docker_state"] = json!("exited");
    exited["process_running"] = json!(false);
    exited["runtime_state"] = json!("Exited");
    exited["exit_confirmed"] = json!(true);
    store
        .transaction("observe-exit", |db| {
            broker::finish(db, &work, Ok(exited.clone()))
        })
        .unwrap();
    let health = call(&mut store, "Task.Get", json!({"id":health})).unwrap();
    let tool = apps::stop::TOOL;
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({
            "principal_id":"local-owner","actions":["task.submit","task.read",tool],
            "tool_targets":[app["resource_id"]],"ttl_seconds":90,"units":2,
        }),
    )
    .unwrap();
    let request = call(&mut store, "Task.Prepare", json!({
        "tool":tool,"key":id("cleanup"),"delegation_id":grant["delegation_ref"]["resource_id"],
        "input":{"target":target(&app),"operation_ref":original,"evidence_ref":health["result"]["artifact_refs"][0]},
    })).unwrap();
    let cleanup = call(&mut store, "Task.Submit", request).unwrap();
    let cleanup = cleanup["task_id"].as_str().unwrap();
    let evict = json!({"id":health["task_id"],"expected_resource_version":health["task_revision"],
        "reason":"reclaim worker observation"});
    assert_eq!(
        call(&mut store, "Task.EvictResults", evict.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    approve(&mut store, cleanup);
    assert_eq!(claim(&mut store), cleanup);
    let initial_cleanup = dispatch(&mut store, cleanup);
    assert_eq!(initial_cleanup.effect.body["phase"], "PermitBound");
    store
        .transaction("recover-before-removal-authorization", broker::recover)
        .unwrap();
    let not_started = call(&mut store, "Task.Get", json!({"id":cleanup})).unwrap();
    assert_eq!(not_started["outcome_knowledge"], "NotStarted");
    assert_eq!(not_started["lifecycle"], "Failed");
    store
        .transaction("check-released-proof", |db| {
            assert!(!apps::stop::retains_result(
                db,
                health["task_id"].as_str().unwrap()
            )?);
            assert_eq!(
                db.of(&initial_cleanup.effect.id, "Effect")?.body["phase"],
                "PermitBound"
            );
            Ok(())
        })
        .unwrap();
    let retried = call(&mut store, "Task.Retry", json!({
        "id":cleanup,"expected_resource_version":not_started["task_revision"],"key":id("retry-cleanup"),
    })).unwrap();
    let cleanup = retried["task_id"].as_str().unwrap();
    assert_eq!(
        call(&mut store, "Task.Get", json!({"id":cleanup})).unwrap()["lifecycle"],
        "WaitingApproval"
    );
    approve(&mut store, cleanup);
    assert_eq!(claim(&mut store), cleanup);
    let work = dispatch(&mut store, cleanup);
    assert_ne!(work.op.id, initial_cleanup.op.id);
    assert_eq!(
        call(&mut store, "Task.EvictResults", evict.clone())
            .unwrap_err()
            .0
            .code,
        "Conflict"
    );
    store
        .transaction("remove-checkpoint", |db| {
            work.checkpoint(db, host::Checkpoint::OciStopCleanup(&exited))
        })
        .unwrap();
    assert!(call(&mut store, "Task.EvictResults", evict).unwrap()["eviction"].is_object());
    store
        .transaction("recover-removal", broker::recover)
        .unwrap();

    store
        .transaction("validate-damaged-active-oci-stop", |db| {
            apps::stop::verify(db.c)?;
            for (statement, resource) in [
                (
                    "DELETE FROM oci_stop_cleanups WHERE cleanup=?1",
                    work.op.id.clone(),
                ),
                (
                    "UPDATE oci_stop_cleanups SET original='unrelated-stop' WHERE cleanup=?1",
                    work.op.id.clone(),
                ),
                (
                    "DELETE FROM versions WHERE id=?1",
                    format!("oci-stop-intent-{}", stop.op.id),
                ),
                (
                    "DELETE FROM versions WHERE id=?1",
                    format!("oci-stop-cleanup-dispatch-{}", work.op.id),
                ),
            (
                "UPDATE versions SET body=json_set(body,'$.snapshot.observation.exit_code',111) WHERE id=?1",
                format!("oci-stop-exit-evidence-{cleanup}"),
            ),
                ("DELETE FROM holds WHERE volume=?1", start.target.id.clone()),
            ] {
                db.c.execute_batch("SAVEPOINT corrupt_oci_stop")?;
                assert!(db.c.execute(statement, [&resource])? >= 1);
                let failure =
                    broker::recover(db).expect_err("startup must reject damaged OCI stop custody");
                assert_eq!(failure.0.code, "RecoveryRequired", "{statement}");
                assert!(apps::stop::verify(db.c).is_err(), "{statement}");
                db.c.execute_batch("ROLLBACK TO corrupt_oci_stop; RELEASE corrupt_oci_stop")?;
            }
            apps::stop::verify(db.c)
        })
        .unwrap();
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
    let created_container = "ca".repeat(32);
    s.transaction("publisher-fixture-created", |db| {
        witness_fixture_create(
            db,
            &starting,
            "publisher-fixture-engine",
            &created_container,
        )
    })
    .unwrap();
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
                engine: "publisher-fixture-engine",
                container: &created_container,
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
fn revoked_pressure_scope_fences_the_sample_and_late_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (_, mut store) = initialized(directory.path());
    let node = call(&mut store, "Node.Get", json!({})).unwrap();
    let tool = node::pressure::TOOL;
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read",tool],
            "tool_targets":[node["reference"]["resource_id"]],"units":1,"ttl_seconds":90}),
    )
    .unwrap();
    let request = call(
        &mut store,
        "Task.Prepare",
        json!({"tool":tool,"input":{},"key":id("pressure-revocation"),
            "delegation_id":grant["delegation_ref"]["resource_id"]}),
    )
    .unwrap();
    let submitted = call(&mut store, "Task.Submit", request).unwrap();
    let task = submitted["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    let sample = work
        .perform(|_| unreachable!("pressure has no host effect checkpoint"))
        .unwrap();
    assert_eq!(sample["kernel"]["source"], "LinuxProcfs");
    assert_eq!(sample["node_ref"], node["reference"]);
    let mut unbound = sample.clone();
    unbound["boot_id"] = json!("unbound-boot");
    assert_eq!(
        node::pressure::check_receipt(&work, &unbound)
            .unwrap_err()
            .0
            .code,
        "Fenced"
    );
    call(
        &mut store,
        "Grant.Revoke",
        json!({"id":grant["grant_ref"]["resource_id"],
            "expected_resource_version":grant["grant_ref"]["version"]}),
    )
    .unwrap();
    store
        .transaction("finish-revoked-pressure", |db| {
            broker::finish(db, &work, Ok(sample.clone()))
        })
        .unwrap();
    let closed = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(closed["lifecycle"], "Failed");
    assert_eq!(closed["conditions"][0]["type"], "PermissionDenied");
    assert_eq!(closed["result"]["artifact_refs"], json!([]));

    let legacy = store.transaction("late-revoked-pressure", |db| {
        broker::finish(db, &work, Ok(sample.clone()))?;
        let artifact: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM versions WHERE kind='Artifact' AND spec->>'task_id'=?1)",
            [task], |row| row.get(0),
        )?;
        assert!(!artifact);
        let late: Value = db.c.query_row(
            "SELECT body FROM versions WHERE kind='LateReceipt' AND body->'operation_ref'->>'resource_id'=?1",
            [&work.op.id], |row| row.get(0),
        )?;
        assert_eq!(late["evidence"], json!({"code":"Fenced"}));
        let record = db.record("LateReceipt", &work.task.owner, json!({
            "run_ref":work.run.reference(),"operation_ref":work.op.reference(),
            "effect_ref":work.effect.reference(),"evidence":{"receipt":sample},
        }))?;
        Ok(record.reference())
    }).unwrap();
    for (method, params) in [
        ("Resource.Get", json!({"id":legacy["resource_id"]})),
        ("Resource.Resolve", json!({"reference":legacy})),
    ] {
        let redacted = call(&mut store, method, params).unwrap();
        assert_eq!(redacted["reference"], legacy);
        assert_eq!(redacted["value"]["evidence"], json!({"code":"Fenced"}));
        assert_eq!(
            redacted["resource"]["spec"]["evidence"],
            json!({"code":"Fenced"})
        );
    }
}

#[test]
fn volume_read_chunks_cannot_publish_from_fenced_runs_or_old_late_receipts() {
    let directory = tempfile::tempdir().unwrap();
    let (config, mut store) = initialized(directory.path());
    let creation = submit(
        &mut store,
        "volume.create",
        json!({"name":"late-volume-read"}),
    );
    let volume = execute(&mut store, &creation);
    let root = config
        .state_dir
        .join("volumes")
        .join(volume["resource_id"].as_str().unwrap())
        .join("data");
    let payload = b"private volume bytes";
    fs::write(root.join("sample"), payload).unwrap();
    let input = json!({"target":{
        "id":volume["resource_id"],"expected_resource_version":volume["version"]},
        "path":"sample","offset":0,"limit":1024});
    let prior = submit(&mut store, "volume.read_chunk", input.clone());
    let prior_result = execute(&mut store, &prior);
    let prior_status = call(&mut store, "Task.Get", json!({"id":prior})).unwrap();
    let prior_artifact = &prior_status["result"]["artifact_refs"][0];

    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({
            "principal_id":policy::OWNER,"actions":["task.submit","task.read","volume.read_chunk"],
            "tool_targets":[volume["resource_id"]],"units":1,"ttl_seconds":90,
        }),
    )
    .unwrap();
    let prepared = call(
        &mut store,
        "Task.Prepare",
        json!({
            "tool":"volume.read_chunk","input":input,"key":id("volume-read-authority"),
            "delegation_id":grant["delegation_ref"]["resource_id"],
        }),
    )
    .unwrap();
    let accepted = call(&mut store, "Task.Submit", prepared).unwrap();
    let task = accepted["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    let sample = work
        .perform(|_| unreachable!("chunk read has no host checkpoint"))
        .unwrap();
    assert_eq!(sample["data_base64"], STANDARD.encode(payload));

    call(
        &mut store,
        "Grant.Revoke",
        json!({"id":grant["grant_ref"]["resource_id"],
            "expected_resource_version":grant["grant_ref"]["version"],
        }),
    )
    .unwrap();
    let legacy = store.transaction("fenced-volume-read", |db| {
        broker::finish(db, &work, Ok(sample.clone()))?;
        broker::finish(db, &work, Ok(sample.clone()))?;
        let accounted: bool = db.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM volume_read_holds WHERE task=?1)
               OR EXISTS(SELECT 1 FROM versions WHERE kind='Artifact' AND spec->>'task_id'=?1)",
            [task], |row| row.get(0),
        )?;
        assert!(!accounted);
        let late: Value = db.c.query_row(
            "SELECT body FROM versions WHERE kind='LateReceipt' AND body->'operation_ref'->>'resource_id'=?1",
            [&work.op.id], |row| row.get(0),
        )?;
        assert_eq!(late["evidence"], json!({"code":"Fenced"}));
        Ok(db.record("LateReceipt", &work.task.owner, json!({
            "run_ref":work.run.reference(),"operation_ref":work.op.reference(),
            "effect_ref":work.effect.reference(),"evidence":{"receipt":sample},
        }))?.reference())
    }).unwrap();
    let settled = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(settled["lifecycle"], "Failed");
    assert_eq!(settled["outcome_knowledge"], "Known");
    assert_eq!(settled["conditions"][0]["type"], "PermissionDenied");
    assert_eq!(settled["result"]["artifact_refs"], json!([]));
    let retained = call(
        &mut store,
        "Artifact.Read",
        json!({"reference":prior_artifact}),
    )
    .unwrap();
    assert_eq!(retained, prior_result);
    for (method, params) in [
        ("Resource.Get", json!({"id":legacy["resource_id"]})),
        ("Resource.Resolve", json!({"reference":legacy})),
    ] {
        let redacted = call(&mut store, method, params).unwrap();
        assert_eq!(redacted["reference"], legacy);
        assert_eq!(redacted["value"]["evidence"], json!({"code":"Fenced"}));
        assert_eq!(
            redacted["resource"]["spec"]["evidence"],
            json!({"code":"Fenced"})
        );
    }
}

#[test]
fn service_observation_keeps_superseded_reads_and_redacts_fenced_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = Config::local(&directory.path().join("core"));
    let unit = "psios-service-observation-fence.service";
    config.services.push(config::ServiceConfig {
        unit: unit.into(),
        manager: config::Manager::User,
        allow_control: false,
        definition_digest: None,
    });
    Store::init(&config).unwrap();
    let mut store = Store::open(config).unwrap();
    let service = format!("service-{unit}");
    let state = format!("state-{service}");
    let initial = call(&mut store, "Resource.Get", json!({"id":state})).unwrap();
    let boot = initial["value"]["boot_id"].clone();
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read","service.observe"],
            "tool_targets":[service],"units":1,"ttl_seconds":90}),
    )
    .unwrap();
    let request = call(
        &mut store,
        "Task.Prepare",
        json!({"tool":"service.observe","key":"superseded-observation",
            "delegation_id":grant["delegation_ref"]["resource_id"],
            "input":{"id":service,"expected_spec_generation":"1"}}),
    )
    .unwrap();
    let accepted = call(&mut store, "Task.Submit", request).unwrap();
    let task = accepted["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    let sample = json!({"unit":unit,"manager":"user","boot_id":boot,"load_state":"loaded",
        "active_state":"active","main_pid":"42","observed_at":now()});
    let (late, legacy) = store
        .transaction("finish-superseded-observation", |db| {
            let mut status = work.target.body.clone();
            status["observation"] = json!({"active_state":"inactive"});
            db.status(&work.target, status)?;
            broker::finish(db, &work, Ok(sample.clone()))?;
            broker::finish(db, &work, Ok(sample.clone()))?;
            let late = db.c.query_row(
                "SELECT body FROM current WHERE kind='LateReceipt'
                 AND body->'operation_ref'->>'resource_id'=?1",
                [&work.op.id],
                |row| row.get::<_, Value>(0),
            )?;
            let legacy = db.record(
                "LateReceipt",
                &work.task.owner,
                json!({
                    "run_ref":work.run.reference(),"operation_ref":work.op.reference(),
                    "effect_ref":work.effect.reference(),"evidence":{"receipt":sample},
                }),
            )?;
            Ok((late, legacy.reference()))
        })
        .unwrap();
    let settled = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(settled["lifecycle"], "Succeeded");
    let result = call(
        &mut store,
        "Artifact.Read",
        json!({"reference":settled["result"]["artifact_refs"][0]}),
    )
    .unwrap();
    assert_eq!(result["projection"], "Superseded");
    assert_eq!(result["service_state_ref"], initial["reference"]);
    assert_eq!(
        call(&mut store, "Resource.Get", json!({"id":state})).unwrap(),
        initial
    );
    assert_eq!(late["evidence"], json!({"code":"Fenced"}));
    for (method, params) in [
        ("Resource.Get", json!({"id":legacy["resource_id"]})),
        ("Resource.Resolve", json!({"reference":legacy})),
    ] {
        let result = call(&mut store, method, params).unwrap();
        assert_eq!(result["reference"], legacy);
        assert_eq!(result["value"]["evidence"], json!({"code":"Fenced"}));
        assert_eq!(
            result["resource"]["spec"]["evidence"],
            json!({"code":"Fenced"})
        );
    }
}

#[test]
fn revoked_service_resource_grant_fences_the_result_and_late_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = Config::local(&directory.path().join("core"));
    let unit = "psios-service-resource-revocation.service";
    cfg.services.push(config::ServiceConfig {
        unit: unit.into(),
        manager: config::Manager::User,
        allow_control: false,
        definition_digest: None,
    });
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg).unwrap();
    let id = format!("service-{unit}");
    let service = call(&mut store, "Resource.Get", json!({"id":id})).unwrap()["reference"].clone();
    let boot = call(&mut store, "Node.Get", json!({})).unwrap()["state"]["boot_id"].clone();
    let tool = node::service_resources::TOOL;
    let grant = call(
        &mut store,
        "Grant.Issue",
        json!({"principal_id":policy::OWNER,
        "actions":["task.submit","task.read",tool],"tool_targets":[id],"units":1,"ttl_seconds":90}),
    )
    .unwrap();
    let request = call(
        &mut store,
        "Task.Prepare",
        json!({"tool":tool,"key":"resource-revocation",
        "delegation_id":grant["delegation_ref"]["resource_id"],
        "input":{"id":id,"expected_resource_version":service["version"],"expected_boot_id":boot,
        "expected_invocation_id":"11111111111111111111111111111111"}}),
    )
    .unwrap();
    let submission = call(&mut store, "Task.Submit", request).unwrap();
    let task = submission["task_id"].as_str().unwrap();
    assert_eq!(claim(&mut store), task);
    let work = dispatch(&mut store, task);
    store
        .transaction("check-service-resource-scope", |db| {
            work.check_result_publication(db)
        })
        .unwrap();
    call(
        &mut store,
        "Grant.Revoke",
        json!({"id":grant["grant_ref"]["resource_id"],
        "expected_resource_version":grant["grant_ref"]["version"]}),
    )
    .unwrap();
    // Simulate the return of a sample after a real authority revocation.
    let receipt = json!({"service_ref":service,"cgroup":{"cpu":{"usage_usec":42}}});
    store
        .transaction("finish-revoked-resource", |db| {
            broker::finish(db, &work, Ok(receipt.clone()))
        })
        .unwrap();
    let closed = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
    assert_eq!(closed["lifecycle"], "Failed");
    assert_eq!(closed["conditions"][0]["type"], "PermissionDenied");
    assert_eq!(closed["result"]["artifact_refs"], json!([]));
    store
        .transaction("late-revoked-resource", |db| {
            broker::finish(db, &work, Ok(receipt))
        })
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
fn revoked_service_log_grants_cannot_publish_artifacts_or_late_bodies() {
    let directory = tempfile::tempdir().unwrap();
    let mut cfg = Config::local(&directory.path().join("core"));
    let unit = "psios-journal-revocation-fixture.service";
    cfg.services.push(config::ServiceConfig {
        unit: unit.into(),
        manager: config::Manager::User,
        allow_control: false,
        definition_digest: None,
    });
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg).unwrap();
    let service_id = format!("service-{unit}");
    let service =
        call(&mut store, "Resource.Get", json!({"id":service_id})).unwrap()["reference"].clone();
    let node = call(&mut store, "Node.Get", json!({})).unwrap();
    let boot = &node["state"]["boot_id"];

    for tool in [node::journal::TOOL, node::journal::SINCE_TOOL] {
        let grant = call(
            &mut store,
            "Grant.Issue",
            json!({"principal_id":policy::OWNER,"actions":["task.submit","task.read",tool],
                "tool_targets":[service_id],"units":1,"ttl_seconds":90}),
        )
        .unwrap();
        let mut input = json!({
            "id":service_id,"expected_resource_version":service["version"],
        });
        let mut receipt = json!({
            "service_ref":service,"unit":unit,"manager":"user","boot_id":boot,
            "limit":16,"observed_at":now(),
            "entries":[{
                "journal_cursor":"i=2","realtime_usec":"2","priority":6,"pid":42,
                "message":{"encoding":"Utf8","data":"private journal output"}
            }],
        });
        // Only the returned bytes are a fixture. Claim, dispatch, revocation
        // and settlement below run through the normal authority.
        if tool == node::journal::SINCE_TOOL {
            input["boot_id"] = boot.clone();
            input["after_cursor"] = json!("i=1");
            receipt["after_cursor"] = json!("i=1");
            receipt["next_cursor"] = json!("i=2");
            receipt["has_more"] = json!(false);
        }
        let request = call(
            &mut store,
            "Task.Prepare",
            json!({"tool":tool,"input":input,"key":id("journal-revocation"),
                "delegation_id":grant["delegation_ref"]["resource_id"]}),
        )
        .unwrap();
        let submitted = call(&mut store, "Task.Submit", request).unwrap();
        let task = submitted["task_id"].as_str().unwrap();
        assert_eq!(claim(&mut store), task);
        let work = dispatch(&mut store, task);
        store
            .transaction("check-live-journal-scope", |db| {
                work.check_result_publication(db)
            })
            .unwrap();
        call(
            &mut store,
            "Grant.Revoke",
            json!({"id":grant["grant_ref"]["resource_id"],
                "expected_resource_version":grant["grant_ref"]["version"]}),
        )
        .unwrap();
        store
            .transaction("finish-revoked-journal", |db| {
                broker::finish(db, &work, Ok(receipt.clone()))
            })
            .unwrap();
        let closed = call(&mut store, "Task.Get", json!({"id":task})).unwrap();
        assert_eq!(closed["lifecycle"], "Failed", "{tool}: {closed}");
        assert_eq!(
            closed["conditions"][0]["type"], "PermissionDenied",
            "{tool}: {closed}"
        );
        assert_eq!(
            closed["result"]["artifact_refs"],
            json!([]),
            "{tool}: {closed}"
        );

        let legacy = store
            .transaction("late-revoked-journal", |db| {
                broker::finish(db, &work, Ok(receipt.clone()))?;
                let artifact: bool = db.c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM versions WHERE kind='Artifact' AND spec->>'task_id'=?1)",
                    [task],
                    |row| row.get(0),
                )?;
                assert!(!artifact, "{tool} persisted an unauthorized result");
                let late: Value = db.c.query_row(
                    "SELECT body FROM versions WHERE kind='LateReceipt'
                     AND body->'operation_ref'->>'resource_id'=?1",
                    [&work.op.id],
                    |row| row.get(0),
                )?;
                assert_eq!(late["evidence"], json!({"code":"Fenced"}), "{tool}");
                let record = db.record(
                    "LateReceipt",
                    &work.task.owner,
                    json!({
                        "run_ref":work.run.reference(),"operation_ref":work.op.reference(),
                        "effect_ref":work.effect.reference(),"evidence":{"receipt":receipt},
                    }),
                )?;
                Ok(record.reference())
            })
            .unwrap();
        for (method, params) in [
            ("Resource.Get", json!({"id":legacy["resource_id"]})),
            ("Resource.Resolve", json!({"reference":legacy})),
        ] {
            let redacted = call(&mut store, method, params).unwrap();
            assert_eq!(redacted["reference"], legacy, "{tool} {method}");
            assert_eq!(redacted["value"]["evidence"], json!({"code":"Fenced"}));
            assert_eq!(
                redacted["resource"]["spec"]["evidence"],
                json!({"code":"Fenced"})
            );
            assert!(!redacted.to_string().contains("private journal output"));
        }
    }
}

#[test]
fn cancellation_after_publication_preserves_successful_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let (_, mut s) = initialized(tmp.path());
    let task = submit(&mut s, "volume.create", json!({"name": "cancel-race"}));
    assert_eq!(claim(&mut s), task);
    let w = dispatch(&mut s, &task);
    let receipt = w.perform(|event| s.transaction("checkpoint", |db| w.checkpoint(db, event)));
    assert!(receipt.is_ok());
    let current = call(&mut s, "Task.Get", json!({"id":task})).unwrap();
    let pending = call(
        &mut s,
        "Task.Cancel",
        json!({"id": task, "expected_resource_version": current["task_revision"]}),
    )
    .unwrap();
    assert_eq!(pending["lifecycle"], "Cancelling");
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
