use crate::{
    db::{Db, Store},
    host, policy, *,
};
use psios_api::{Request, VERSION, contract};
use rusqlite::params;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    reference: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    tool: String,
    input: Value,
    delegation_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Audit {
    after: Option<u64>,
    limit: Option<u32>,
}
fn invoke(db: &Db<'_>, actor: &str, method: &str, p: &Value) -> Result<Value> {
    match method {
        "Principal.WhoAmI" => {
            let _: Empty = decode(p)?;
            policy::who(db, actor)
        }
        "Agent.Register" => policy::register(db, actor, p),
        "Principal.Revoke" => policy::disable(db, actor, p),
        "Delegation.Create" => policy::delegate(db, actor, p),
        "Delegation.Revoke" => policy::revoke_delegation(db, actor, p),
        "Grant.Issue" => policy::grant(db, actor, p),
        "Grant.Revoke" => policy::revoke(db, actor, p),
        "Budget.Get" => policy::budget(db, actor, p),
        "Capability.Discover" => {
            let _: Empty = decode(p)?;
            policy::discover(db, actor)
        }
        "Tool.Call" => {
            let p: ToolCall = decode(p)?;
            policy::delegation(db, actor, &p.delegation_id, &p.tool)?;
            match host::Action::parse(&p.tool, &p.input)? {
                host::Action::Status {} => {
                    db.audit(actor, "system.status", Some(policy::PROFILE), "Allowed")?;
                    host::status(db)
                }
                _ => Err(err(
                    "UnsupportedCapability",
                    "use Task.Submit for this registered tool",
                )),
            }
        }
        "Resource.Get" | "Resource.Resolve" | "Resource.List" | "Event.Poll" | "Event.Ack"
        | "Event.Offset" => {
            policy::can(db, actor, "resource.read")?;
            policy::can(db, actor, "task.read")?;
            if method.starts_with("Event.") {
                policy::can(db, actor, "event.read")?
            }
            match method {
                "Resource.List" => events::list(db, actor, p),
                "Event.Poll" => events::poll(db, actor, p),
                "Event.Ack" => events::ack(db, actor, p),
                "Event.Offset" => events::offset(db, actor, p),
                _ => {
                    let d = if method == "Resource.Get" {
                        let p: ById = decode(p)?;
                        db.get(&p.id)?
                    } else {
                        let p: Reference = decode(p)?;
                        db.resolve(&p.reference)?
                    };
                    require(
                        policy::visible(actor, &d),
                        "PermissionDenied",
                        "resource outside authorized scope",
                    )?;
                    let env = d.envelope(&db.meta("authority")?);
                    contract("resource-envelope", &env)?;
                    Ok(json!({"reference": d.reference(), "resource": env, "value": d.body}))
                }
            }
        }
        "Artifact.Read" => {
            policy::can(db, actor, "task.read")?;
            let p: Reference = decode(p)?;
            let d = db.resolve(&p.reference)?;
            require(
                d.kind == "Artifact",
                "InvalidArgument",
                "expected result artifact",
            )?;
            policy::owned(actor, &d)?;
            Ok(d.body)
        }
        "Secret.Put" => policy::secret_put(db, actor, p),
        "Secret.Revoke" => policy::secret_revoke(db, actor, p),
        "StoragePool.Get" => storage::pool_status(db, actor, p),
        "StoragePool.Configure" => storage::pool_configure(db, actor, p),
        "Application.Configure" => apps::configure(db, actor, p),
        "Task.Prepare" => broker::prepare(db, actor, p),
        "Task.Submit" => broker::submit(db, actor, p, None),
        "Task.LookupSubmission" => broker::lookup(db, actor, p),
        "Task.Cancel" => broker::cancel(db, actor, p),
        "Task.CancelOrigin" => broker::cancel_origin(db, actor, p),
        "Task.Retry" => broker::retry(db, actor, p),
        "Task.Get" => broker::get(db, actor, p, "Task"),
        "Run.Get" => broker::get(db, actor, p, "Run"),
        "Operation.Reconcile" => broker::reconcile(db, actor, p),
        "Operation.Get" => broker::get(db, actor, p, "Operation"),
        "Approval.Decide" => broker::decide(db, actor, p),
        "Audit.List" => {
            policy::admin(actor)?;
            let p: Audit = decode(p)?;
            let limit = p.limit.unwrap_or(32);
            require(
                (1..=128).contains(&limit),
                "InvalidArgument",
                "invalid audit limit",
            )?;
            let out=db.c.prepare("SELECT seq,at,actor,method,resource,decision,request FROM audit WHERE seq>?1 ORDER BY seq LIMIT ?2")?
                .query_map(params![i64::try_from(p.after.unwrap_or(0)).map_err(|_|err("InvalidArgument","audit offset outside range"))?,limit],|r|Ok(json!({"seq":r.get::<_,i64>(0)?.to_string(),"at":r.get::<_,String>(1)?,
                    "actor":r.get::<_,String>(2)?,"method":r.get::<_,String>(3)?,"resource":r.get::<_,Option<String>>(4)?,"decision":r.get::<_,String>(5)?,
                    "request_id":r.get::<_,String>(6)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!(out))
        }
        _ => Err(err("UnsupportedCapability", "method is not registered")),
    }
}
impl Store {
    pub fn request(&mut self, peer: u32, r: Request) -> Result<Value> {
        require(
            r.schema_version == VERSION,
            "SchemaVersionUnsupported",
            "unsupported system API version",
        )?;
        label(&r.request_id)?;
        label(&r.method)?;
        let method = r.method.as_str();
        let mut audit_actor = format!("uid-{peer}");
        let result = self.transaction(&r.request_id, |db| {
            let actor = policy::authenticate(db, &r.token)?;
            audit_actor = actor.clone();
            let subject = db.of(&actor, "Principal")?;
            require(
                subject.spec["peer_uid"]
                    .as_u64()
                    .is_none_or(|uid| uid == u64::from(peer)),
                "PermissionDenied",
                "credential UID mismatch",
            )?;
            require(
                actor != policy::OWNER || peer == rustix::process::geteuid().as_raw(),
                "PermissionDenied",
                "owner credential requires service UID",
            )?;
            let out = invoke(db, &actor, method, &r.params)?;
            if matches!(
                method,
                "Agent.Register"
                    | "Principal.Revoke"
                    | "Delegation.Create"
                    | "Delegation.Revoke"
                    | "Grant.Issue"
                    | "Grant.Revoke"
                    | "Secret.Put"
                    | "Secret.Revoke"
                    | "Approval.Decide"
                    | "StoragePool.Configure"
                    | "Application.Configure"
                    | "Task.Cancel"
                    | "Task.CancelOrigin"
                    | "Task.Retry"
                    | "Event.Ack"
            ) {
                db.audit(&actor, method, None, "Allowed")?;
            }
            Ok(out)
        });
        if let Err(e) = &result {
            // Never persist untrusted method strings or payloads as diagnostic text.
            let audit_method = if method.len() <= 64
                && method.bytes().all(|b| b.is_ascii_alphabetic() || b == b'.')
            {
                method
            } else {
                "InvalidMethod"
            };
            self.transaction(&r.request_id, |db| {
                db.audit(&audit_actor, audit_method, None, &e.0.code)
            })?;
        }
        result
    }
}
