use super::*;

#[test]
fn draining_preserves_authorized_model_replay_and_commits_strict_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let cfg = Config::local(&directory.path().join("core"));
    Store::init(&cfg).unwrap();
    let mut store = Store::open(cfg).unwrap();
    let actor = policy::OWNER;
    let (model_ref, direct, queued, waiting) = store
        .transaction("model-drain-fixture", |db| {
            let grant = policy::grant(
                db,
                actor,
                &json!({
                    "principal_id":actor,
                    "actions":["model.serve","session.run","task.submit","task.read","system.status"],
                    "model_calls":6,"units":4,"ttl_seconds":180,
                }),
            )?;
            let model_ref = model(
                db,
                actor,
                &json!({
                    "name":"drain-model","revision":"1","provider_principal_id":actor,
                    "inference_outcome_policy":"Strict","lease_seconds":120,
                    "max_context_bytes":16384,"max_output_bytes":2048,
                }),
            )?;
            let skill_ref = skill(
                db,
                actor,
                &json!({
                    "name":"drain-skill","version":"1","license":"0BSD","source":"fixture",
                    "tools":["system.status"],"instructions":"Report the current status.",
                }),
            )?;
            let create = |key, provider_queue| {
                open(
                    db,
                    actor,
                    &json!({
                        "goal":"Report this node","key":key,"model_ref":model_ref,"skill_ref":skill_ref,
                        "delegation_id":grant["delegation_ref"]["resource_id"],
                        "context_policy":"ReferencesOnly","max_turns":3,"provider_queue":provider_queue,
                    }),
                )
            };
            let direct = create("direct-session", false)?;
            let queued = create("queued-session", true)?;
            let waiting = create("waiting-session", true)?;
            Ok((model_ref, direct, queued, waiting))
        })
        .unwrap();
    let direct_id = &direct["reference"]["resource_id"];
    let queued_id = &queued["reference"]["resource_id"];
    let direct_request = json!({"session_id":direct_id,"key":"direct-lease"});
    let queue_request = json!({"model_ref":model_ref,"key":"queued-lease"});
    let direct_lease = store
        .transaction("claim-before-drain", |db| claim(db, actor, &direct_request))
        .unwrap()
        .unwrap();
    let queued_lease = store
        .transaction("claim-next-before-drain", |db| {
            claim_next(db, actor, &queue_request)
        })
        .unwrap();
    assert_eq!(queued_lease["session_id"], *queued_id);
    store
        .transaction("close-model-admission", |db| {
            let previous = work_admission::get(db, actor, &json!({}))?;
            work_admission::set(
                db,
                actor,
                &json!({"expected_resource_version":previous["reference"]["version"],
                    "mode":"Draining","reason":"Prepare local maintenance"}),
            )?;
            let replay = claim(db, actor, &direct_request)??;
            assert_eq!(replay["lease_id"], direct_lease["lease_id"]);
            assert_eq!(replay["recovered"], true);
            let replay = claim_next(db, actor, &queue_request)?;
            assert_eq!(replay["lease_id"], queued_lease["lease_id"]);
            assert_eq!(replay["recovered"], true);
            for value in [&direct_lease["lease_id"], &queued_lease["lease_id"]] {
                let lease = value.as_str().unwrap();
                // Make real issued leases due without running a timer or sleeping.
                db.c.execute(
                    "UPDATE session_turns SET deadline_boot_ms=0 WHERE lease=?1",
                    [lease],
                )?;
                db.c.execute(
                    "UPDATE model_inference_leases SET deadline_boot_ms=0 WHERE lease=?1",
                    [lease],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let closed = store
        .transaction("deny-fresh-model-admission", |db| {
            claim(
                db,
                actor,
                &json!({
                    "session_id":waiting["reference"]["resource_id"], "key":"unissued-direct-lease",
                }),
            )
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(closed.0.code, "PreconditionFailed");
    assert_eq!(closed.0.retry_condition, "SameRequestAfterBackoff");
    let refusal = store
        .transaction("fresh-admission-after-strict-expiry", |db| {
            claim(
                db,
                actor,
                &json!({"session_id":direct_id,"key":"new-direct-lease"}),
            )
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(refusal.0.code, "OutcomeUnknown");
    store
        .transaction("verify-addressed-expiry-committed", |db| {
            let session = db.of(direct_id.as_str().unwrap(), "Session")?;
            assert_eq!(session.body["pause_reason"], "ModelOutcomeUnknown");
            assert_eq!(
                session.body["model_inference_failure"]["source"],
                "LeaseExpired"
            );
            assert_eq!(
                state(&db.of(queued_id.as_str().unwrap(), "Session")?),
                "ModelInFlight"
            );
            Ok(())
        })
        .unwrap();
    let empty = store
        .transaction("advance-queued-expiry-without-new-lease", |db| {
            claim_next(
                db,
                actor,
                &json!({"model_ref":model_ref,"key":"new-queued-lease"}),
            )
        })
        .unwrap();
    assert_eq!(empty, json!({"available":false}));
    store
        .transaction("verify-model-drain-expiry", |db| {
            let view = work_admission::get(db, actor, &json!({}))?;
            assert_eq!(view["model_counts"]["current_leases"], 0);
            assert_eq!(view["model_counts"]["paused_outcomes_unknown"], 2);
            let replay = claim(db, actor, &direct_request)??;
            assert_eq!(replay["completed"]["outcome"], "Unknown");
            let replay = claim_next(db, actor, &queue_request)?;
            assert_eq!(replay["completed"]["outcome"], "Unknown");
            let calls: u32 =
                db.c.query_row("SELECT count(*) FROM session_turns", [], |r| r.get(0))?;
            assert_eq!(calls, 2);
            Ok(())
        })
        .unwrap();
}
