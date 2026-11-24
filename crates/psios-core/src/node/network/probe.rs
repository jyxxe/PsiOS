//! Operator-configured DNS and TCP checks in the Core network namespace.
mod dns;

use super::namespace;
use crate::{
    ById, Result,
    config::{Config, NetworkProbeConfig},
    db::{Db, Doc},
    decode, err,
    host::{Target, Work},
    node::upsert,
    now, policy, require,
};
use psios_api::{canonical, sha256};
use rustix::time::{ClockId, clock_gettime};
use serde_json::{Value, json};
use std::{collections::HashSet, io, net::SocketAddr, time::Duration};
use tokio::{
    net::TcpStream,
    time::{Instant, timeout_at},
};

pub const TOOL: &str = "node.network_probe";
const KIND: &str = "NetworkProbe";

fn endpoint_ok(address: SocketAddr) -> bool {
    if address.port() == 0 || address.ip().is_unspecified() || address.ip().is_multicast() {
        return false;
    }
    match address {
        SocketAddr::V4(v4) => !v4.ip().is_broadcast(),
        SocketAddr::V6(v6) => {
            v6.ip().to_ipv4_mapped().is_none()
                && (v6.scope_id() != 0) == v6.ip().is_unicast_link_local()
        }
    }
}

pub fn validate(probes: &[NetworkProbeConfig]) -> Result<()> {
    require(
        probes.len() <= 32,
        "InvalidArgument",
        "too many network probes",
    )?;
    let mut names = HashSet::new();
    for probe in probes {
        require(
            !probe.name.is_empty()
                && probe.name.len() <= 64
                && probe
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
                && names.insert(&probe.name),
            "InvalidArgument",
            "network probe names must be unique identifiers",
        )?;
        require(
            endpoint_ok(probe.endpoint) && (50..=5_000).contains(&probe.timeout_ms),
            "InvalidArgument",
            "network probe requires a unicast endpoint and a 50–5000 ms deadline",
        )?;
        if let Some(resolver) = &probe.dns {
            require(
                endpoint_ok(resolver.resolver),
                "InvalidArgument",
                "invalid network probe resolver",
            )?;
            dns::name(&resolver.name)?;
        }
    }
    Ok(())
}

fn resource_id(node: &str, probe: &NetworkProbeConfig) -> String {
    let dns = probe.dns.as_ref().map(|dns| {
        json!({
            "resolver":dns.resolver, "name":dns.name.to_ascii_lowercase()
        })
    });
    // A changed destination or DNS authority must receive a new authorization target.
    let identity = sha256(&canonical(&json!([
        "psios/network-probe/1",
        node,
        probe.name,
        probe.endpoint,
        dns
    ])));
    format!("probe-{}", &identity[7..])
}

fn fingerprint(probe: &NetworkProbeConfig) -> String {
    sha256(&canonical(&json!(probe)))
}

pub fn sync(db: &Db<'_>, node: &str) -> Result<()> {
    let mut active = HashSet::new();
    for probe in &db.cfg.network_probes {
        let id = resource_id(node, probe);
        let spec =
            json!({"name":probe.name, "node_id":node, "configuration_digest":fingerprint(probe)});
        upsert(
            db,
            KIND,
            &id,
            spec,
            json!({"lifecycle":"Registered","name":probe.name,"node_id":node}),
        )?;
        active.insert(id);
    }
    let registered =
        db.c.prepare("SELECT id FROM current WHERE kind=?1 AND body->>'lifecycle'='Registered'")?
            .query_map([KIND], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in registered.iter().filter(|id| !active.contains(*id)) {
        let probe = db.get(id)?;
        let mut state = probe.body.clone();
        state["lifecycle"] = json!("Unregistered");
        db.status(&probe, state)?;
    }
    Ok(())
}

fn definition<'a>(cfg: &'a Config, doc: &Doc) -> Result<&'a NetworkProbeConfig> {
    require(
        doc.body["lifecycle"] == "Registered",
        "PreconditionFailed",
        "network probe is unregistered",
    )?;
    let node = doc.spec["node_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "network probe has no node"))?;
    cfg.network_probes
        .iter()
        .find(|p| {
            doc.spec["name"] == p.name
                && doc.id == resource_id(node, p)
                && doc.spec["configuration_digest"] == fingerprint(p)
        })
        .ok_or_else(|| err("PreconditionFailed", "network probe configuration changed"))
}

pub fn get(db: &Db<'_>, actor: &str, request: &Value) -> Result<Value> {
    let query: ById = decode(request)?;
    policy::can_target(db, actor, TOOL, &query.id)?;
    let doc = db.of(&query.id, KIND)?;
    let configuration = if doc.body["lifecycle"] == "Registered" {
        Some(definition(db.cfg, &doc)?)
    } else {
        None
    };
    Ok(json!({"reference":doc.reference(), "state":doc.body, "configuration":configuration}))
}

pub fn target(db: &Db<'_>, target: &Target) -> Result<Doc> {
    let doc = db.of(&target.id, KIND)?;
    doc.cas(&target.expected_resource_version)?;
    definition(db.cfg, &doc)?;
    Ok(doc)
}

fn boot_time() -> Duration {
    let now = clock_gettime(ClockId::Boottime);
    Duration::new(now.tv_sec as u64, now.tv_nsec as u32)
}

fn transport_error(error: &io::Error) -> Value {
    use io::ErrorKind::*;
    let status = match error.kind() {
        ConnectionRefused => "Refused",
        NetworkUnreachable | HostUnreachable => "Unreachable",
        TimedOut => "TimedOut",
        PermissionDenied => "Denied",
        UnexpectedEof | ConnectionReset | ConnectionAborted => "Interrupted",
        _ => "TransportError",
    };
    json!({"status":status, "errno":error.raw_os_error()})
}

async fn dial(endpoint: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect(endpoint).await?;
    // In particular, keep the IPv6 address family and scope of the approved peer.
    let remote = stream.peer_addr()?;
    let same_scope = match (remote, endpoint) {
        (SocketAddr::V6(a), SocketAddr::V6(b)) => a.scope_id() == b.scope_id(),
        _ => true,
    };
    if remote.ip() != endpoint.ip() || remote.port() != endpoint.port() || !same_scope {
        return Err(io::Error::other("TCP peer changed"));
    }
    Ok(stream)
}

async fn observe(probe: &NetworkProbeConfig) -> Value {
    let started = boot_time();
    let budget = Duration::from_millis(probe.timeout_ms);
    let deadline = Instant::now() + budget;
    let dns = match &probe.dns {
        Some(resolver) => {
            let report =
                match timeout_at(deadline, dns::observe(resolver, probe.endpoint.ip())).await {
                    Ok(result) => result,
                    Err(_) => json!({"status":"TimedOut"}),
                };
            Some(json!({"resolver":resolver.resolver,"name":resolver.name,"result":report}))
        }
        None => None,
    };
    let tcp = if dns
        .as_ref()
        .is_some_and(|result| result["result"]["status"] != "Verified")
    {
        json!({"status":"Skipped","reason":"DnsUnverified"})
    } else if boot_time().saturating_sub(started) >= budget {
        json!({"status":"TimedOut"})
    } else {
        match timeout_at(deadline, dial(probe.endpoint)).await {
            Ok(Ok(stream)) if boot_time().saturating_sub(started) < budget => {
                match stream.local_addr() {
                    Ok(local) => json!({"status":"Connected","local_address":local}),
                    Err(error) => transport_error(&error),
                }
            }
            Ok(Err(error)) => transport_error(&error),
            _ => json!({"status":"TimedOut"}),
        }
    };
    let status = if tcp["status"] == "Connected" {
        "Connected"
    } else if tcp["status"] == "Refused"
        || dns
            .as_ref()
            .is_some_and(|v| v["result"]["response_code"].is_number())
    {
        "Limited"
    } else {
        "Offline"
    };
    json!({"status":status,"endpoint":probe.endpoint,"dns":dns,"tcp":tcp,
           "elapsed_ms":boot_time().saturating_sub(started).as_millis() as u64})
}

pub fn perform(work: &Work) -> Result<Value> {
    let probe = definition(&work.cfg, &work.target)?;
    let namespace = namespace()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let observation = runtime.block_on(observe(probe));
    Ok(json!({
        "target_ref":work.target.reference(), "boot_id":namespace.boot_id,
        "network_namespace_inode":namespace.network_namespace_inode, "observed_at":now(),
        "observation":observation
    }))
}
