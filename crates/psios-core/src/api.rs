use crate::{
    db::{Db, Store},
    host, policy, *,
};
use psios_api::{MAX_ARTIFACT_CHUNK, MAX_FRAME, Request, VERSION, contract};
use rusqlite::params;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    reference: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactPage {
    reference: Value,
    offset: usize,
    length: usize,
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
        "Node.Get" => node::get(db, actor, p),
        "Node.Configure" => node::configure(db, actor, p),
        "NetworkProbe.Get" => node::network::probe::get(db, actor, p),
        "Skill.Publish" => agent::skill(db, actor, p),
        "Skill.Retire" => agent::retire(db, actor, p, "Skill"),
        "Model.Register" => agent::model(db, actor, p),
        "Model.Retire" => agent::retire(db, actor, p, "Model"),
        "Model.ClaimNext" => agent::claim_next(db, actor, p),
        "Model.ClaimStatus" => agent::claim_status(db, actor, p),
        "Model.Respond" => agent::respond(db, actor, p),
        "Model.ReportFailure" => agent::inference::report(db, actor, p),
        "Mailbox.Bind" => agent::mail::bind(db, actor, p),
        "Mailbox.Close" => agent::mail::close(db, actor, p),
        "Mailbox.Read" => agent::mail::read(db, actor, p),
        "Mailbox.Ack" => agent::mail::ack(db, actor, p),
        "Signal.Send" => agent::mail::send(db, actor, p),
        "Session.Open" => agent::open(db, actor, p),
        "Session.RetryTask" => agent::retry_task(db, actor, p),
        "Session.Extend" => agent::extend(db, actor, p),
        "Session.Get" => agent::get(db, actor, p),
        "Session.Resume" => agent::resume(db, actor, p),
        "Session.UseFallback" => agent::fallback::handoff(db, actor, p),
        "Session.History" => agent::history(db, actor, p),
        "Session.Cancel" => agent::cancel(db, actor, p),
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
            let d = policy::delegation(db, actor, &p.delegation_id, &p.tool)?;
            policy::tool_scope(db, &d.reference(), policy::PROFILE)?;
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
        "Resource.Get" | "Resource.Resolve" | "Resource.List" | "Resource.Watch" | "Event.Poll"
        | "Event.Ack" | "Event.Offset" => {
            policy::can(db, actor, "resource.read")?;
            policy::can(db, actor, "task.read")?;
            if method == "Resource.Watch" || method.starts_with("Event.") {
                policy::can(db, actor, "event.read")?
            }
            match method {
                "Resource.List" => events::list(db, actor, p),
                "Event.Poll" | "Resource.Watch" => events::poll(db, actor, p),
                "Event.Ack" => events::ack(db, actor, p),
                "Event.Offset" => events::offset(db, actor, p),
                _ => {
                    let mut d = if method == "Resource.Get" {
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
                    apps::log_tail::authorize_artifact(db, actor, &d)?;
                    let reference = d.reference();
                    apps::log_tail::redact_status(&d.kind, &mut d.body);
                    apps::log_tail::redact_status(&d.kind, &mut d.spec);
                    apps::log_tail::redact_late_receipt(db, &mut d)?;
                    let env = d.envelope(&db.meta("authority")?);
                    contract("resource-envelope", &env)?;
                    let value = if artifact::is_json(&d) {
                        artifact::read(db, &d, Some(MAX_FRAME))?
                    } else {
                        d.body
                    };
                    Ok(json!({"reference": reference, "resource": env, "value": value}))
                }
            }
        }
        "Artifact.UploadProfile" => artifact::ingress::profile(db, actor, p),
        "Artifact.Stage" => artifact::ingress::stage(db, actor, p),
        "Artifact.Stage.Get" => artifact::ingress::get(db, actor, p),
        "Artifact.Stage.PutChunk" => artifact::ingress::put(db, actor, p),
        "Artifact.Stage.Discard" => artifact::ingress::discard(db, actor, p),
        "Artifact.Publish" => artifact::ingress::publish(db, actor, p),
        "Artifact.Withdraw" => artifact::ingress::withdraw(db, actor, p),
        "Artifact.PublisherKey.Register" => apps::publisher::register(db, actor, p),
        "Artifact.PublisherKey.Revoke" => apps::publisher::revoke(db, actor, p),
        "Artifact.Read" | "Artifact.Get" | "Artifact.ReadChunk" => {
            let (reference, page) = if method == "Artifact.ReadChunk" {
                let p: ArtifactPage = decode(p)?;
                require(
                    (1..=MAX_ARTIFACT_CHUNK).contains(&p.length),
                    "InvalidArgument",
                    "artifact chunk length must be 1 to 131072 bytes",
                )?;
                (p.reference, Some((p.offset, p.length)))
            } else {
                (decode::<Reference>(p)?.reference, None)
            };
            let (d, shared) = broker::result_access::resolve_artifact(db, actor, &reference)?;
            if method != "Artifact.Get" {
                apps::log_tail::authorize_artifact(db, actor, &d)?;
            }
            let value = match page {
                Some((offset, length)) => artifact::page(db, &d, offset, length)?,
                None if method == "Artifact.Get" => artifact::retention::describe(db, &d, shared)?,
                None => artifact::read(db, &d, Some(MAX_FRAME))?,
            };
            if shared {
                broker::result_access::audit_read(db, actor, method, &d.id)?;
            }
            Ok(value)
        }
        "Artifact.RepairChunk" => artifact::repair::chunk(db, actor, p),
        "Secret.Rotate" => policy::secret_rotate(db, actor, p),
        "Secret.Put" => policy::secret_put(db, actor, p),
        "Secret.Revoke" => policy::secret_revoke(db, actor, p),
        "StoragePool.Get" => storage::pool_status(db, actor, p),
        "StoragePool.Configure" => storage::pool_configure(db, actor, p),
        "Application.Configure" => apps::configure(db, actor, p),
        "Application.ListReleases" => apps::inventory::list(db, actor, p),
        "Application.PrepareArtifactInstall" => {
            apps::artifact_install::prepare(db, actor, p, apps::artifact_install::TOOL)
        }
        "Application.PrepareArtifactRelease" => {
            apps::artifact_install::prepare(db, actor, p, apps::artifact_install::STAGE_TOOL)
        }
        "Schedule.Create" => schedule::create(db, actor, p),
        "Schedule.Get" => schedule::get(db, actor, p, "Schedule"),
        "Schedule.TriggerGet" => schedule::get(db, actor, p, "ScheduleTrigger"),
        "Schedule.Cancel" => schedule::cancel(db, actor, p),
        "Workflow.Define" => workflow::define(db, actor, p),
        "Workflow.DefinitionGet" => workflow::get(db, actor, p, "WorkflowDefinition"),
        "Workflow.Start" => workflow::start(db, actor, p),
        "Workflow.Get" => workflow::get(db, actor, p, "WorkflowRun"),
        "Workflow.Cancel" => workflow::cancel(db, actor, p),
        "Workflow.Resume" => workflow::resume(db, actor, p),
        "Conversation.OpenDirect" => chat::open(db, actor, p),
        "Conversation.OpenRoom" => chat::open_room(db, actor, p),
        "Conversation.List" => chat::list(db, actor, p),
        "Conversation.Get" => chat::get(db, actor, p),
        "Conversation.Respond" => chat::respond(db, actor, p),
        "Conversation.Archive" => chat::archive(db, actor, p),
        "Conversation.Reopen" => chat::reopen(db, actor, p),
        "Conversation.Invite" => chat::invite(db, actor, p),
        "Conversation.RemoveMember" => chat::remove(db, actor, p),
        "Conversation.TransferOwner" => chat::transfer_owner(db, actor, p),
        "Conversation.Leave" => chat::leave(db, actor, p),
        "Message.OpenSendWindow" => chat::open_window(db, actor, p),
        "Message.Prepare" => chat::prepare(db, actor, p),
        "Message.Send" => chat::send(db, actor, p),
        "Message.List" => chat::list_messages(db, actor, p),
        "Message.Get" => chat::get_message(db, actor, p),
        "Message.AckRead" => chat::ack(db, actor, p),
        "Task.Prepare" => broker::prepare(db, actor, p),
        "Task.EvictResults" => artifact::retention::evict(db, actor, p),
        "Task.VerifyResults" => artifact::availability::verify(db, actor, p),
        "Task.Submit" => broker::submit(db, actor, p, None),
        "Task.LookupSubmission" => broker::lookup(db, actor, p),
        "Task.Cancel" => broker::cancel(db, actor, p),
        "Task.CancelOrigin" => broker::cancel_origin(db, actor, p),
        "Task.Retry" => broker::retry(db, actor, p),
        "Task.History" => broker::history(db, actor, p),
        "Task.Archive" => broker::archive(db, actor, p),
        "Task.Admission" => broker::admission::principal(db, actor, p),
        "Task.Get" => broker::get(db, actor, p, "Task"),
        "Task.Results" => broker::result_access::get(db, actor, p),
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
        if r.method == storage::export::METHOD {
            let job = self.prepare_export(peer, &r)?;
            return self.finish_export(peer, &r, job.read());
        }
        if r.method == "Model.Claim" {
            return self.authorized_request(peer, &r, |db, actor| {
                let claimed = agent::claim(db, actor, &r.params)?;
                if claimed.is_ok() {
                    db.audit(actor, "Model.Claim", None, "Allowed")?;
                }
                Ok(claimed)
            })?;
        }
        self.authorized_request(peer, &r, |db, actor| {
            let method = r.method.as_str();
            let out = invoke(db, actor, method, &r.params)?;
            if matches!(
                method,
                "Agent.Register"
                    | "Skill.Publish"
                    | "Skill.Retire"
                    | "Model.Register"
                    | "Model.Retire"
                    | "Model.Respond"
                    | "Mailbox.Bind"
                    | "Mailbox.Close"
                    | "Mailbox.Ack"
                    | "Signal.Send"
                    | "Session.RetryTask"
                    | "Session.Extend"
                    | "Session.Open"
                    | "Session.Cancel"
                    | "Node.Configure"
                    | "Principal.Revoke"
                    | "Delegation.Create"
                    | "Delegation.Revoke"
                    | "Grant.Issue"
                    | "Grant.Revoke"
                    | "Secret.Rotate"
                    | "Secret.Put"
                    | "Secret.Revoke"
                    | "Approval.Decide"
                    | "StoragePool.Configure"
                    | "Application.Configure"
                    | "Schedule.Create"
                    | "Schedule.Cancel"
                    | "Workflow.Define"
                    | "Workflow.Start"
                    | "Workflow.Cancel"
                    | "Workflow.Resume"
                    | "Conversation.OpenDirect"
                    | "Conversation.OpenRoom"
                    | "Conversation.Invite"
                    | "Conversation.Respond"
                    | "Conversation.RemoveMember"
                    | "Conversation.TransferOwner"
                    | "Conversation.Leave"
                    | "Conversation.Archive"
                    | "Conversation.Reopen"
                    | "Task.Cancel"
                    | "Task.CancelOrigin"
                    | "Task.Archive"
                    | "Task.Retry"
                    | "Event.Ack"
            ) {
                let target = if method.starts_with("Conversation.") {
                    r.params["id"].as_str().or_else(|| out["id"].as_str())
                } else {
                    None
                };
                db.audit(actor, method, target, "Allowed")?;
            }
            Ok(out)
        })
    }

    pub(crate) fn prepare_export(
        &mut self,
        peer: u32,
        r: &Request,
    ) -> Result<storage::export::Job> {
        let cache = self.export_cache.clone();
        self.authorized_request(peer, r, |db, actor| {
            storage::export::prepare(db, actor, &r.params, cache)
        })
    }

    pub(crate) fn finish_export(
        &mut self,
        peer: u32,
        r: &Request,
        read: Result<psios_api::BackupChunk>,
    ) -> Result<Value> {
        self.authorized_request(peer, r, |db, actor| {
            storage::export::finish(db, actor, &r.params, read)
        })
    }

    fn authorized_request<T>(
        &mut self,
        peer: u32,
        r: &Request,
        execute: impl FnOnce(&Db<'_>, &str) -> Result<T>,
    ) -> Result<T> {
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
            execute(db, &actor)
        });
        // Repeated headroom refusals must leave space for in-flight work and security records.
        if let Err(e) = &result
            && !(e.0.code == "QuotaExceeded" && e.0.retry_condition == "SameRequestAfterBackoff")
        {
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
