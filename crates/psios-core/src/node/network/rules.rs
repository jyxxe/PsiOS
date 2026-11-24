use super::{
    Family, Prefix,
    kernel::Observer,
    namespace,
    snapshot::{self, Cursor},
};
use crate::{Result, err, host::Work, now, require};
use netlink_packet_core::Nla;
use netlink_packet_route::{
    RouteNetlinkMessage,
    rule::{RuleAttribute, RuleMessage},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, net::IpAddr, os::unix::fs::MetadataExt};

const FRA_PAD: u16 = 18;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleQuery {
    family: Option<Family>,
    cursor: Option<Cursor>,
    limit: Option<usize>,
}

impl RuleQuery {
    pub fn validate(&self) -> Result<()> {
        snapshot::validate(self.cursor.as_ref(), self.limit.unwrap_or(32))
    }
}

#[derive(Default, Serialize)]
struct Selectors {
    source: Option<Prefix>,
    destination: Option<Prefix>,
    tos: u8,
    input_interface: Option<String>,
    output_interface: Option<String>,
    fwmark: Option<u32>,
    fwmask: Option<u32>,
    tunnel_id: Option<String>,
    l3mdev: bool,
    uid_range: Option<[u32; 2]>,
    ip_protocol: Option<u8>,
    source_port_range: Option<[u16; 2]>,
    destination_port_range: Option<[u16; 2]>,
}

#[derive(Serialize)]
struct UnsupportedAttribute {
    kernel_type: u16,
    digest: String,
}

#[derive(Serialize)]
struct Rule {
    family: Family,
    order: usize,
    priority: u32,
    action: u8,
    table: u32,
    goto_priority: Option<u32>,
    flags: u32,
    protocol: Option<u8>,
    realms: Option<[u16; 2]>,
    suppress_ifgroup: Option<u32>,
    suppress_prefixlen: Option<u32>,
    selectors: Selectors,
    unsupported_attributes: Vec<UnsupportedAttribute>,
}

fn prefix(family: Family, address: Option<IpAddr>, length: u8) -> Result<Option<Prefix>> {
    match (family, address, length) {
        (_, None, 0) => Ok(None),
        (Family::Inet, Some(address @ IpAddr::V4(_)), 0..=32)
        | (Family::Inet6, Some(address @ IpAddr::V6(_)), 0..=128) => {
            Ok(Some(Prefix { address, length }))
        }
        _ => Err(err("DependencyUnavailable", "invalid kernel rule prefix")),
    }
}

fn rule(family: Family, order: usize, message: RuleMessage) -> Result<Rule> {
    let header = message.header;
    require(
        header.family == family.kernel(),
        "DependencyUnavailable",
        "unexpected kernel rule address family",
    )?;
    let mut item = Rule {
        family,
        order,
        priority: 0,
        action: header.action.into(),
        table: header.table.into(),
        goto_priority: None,
        flags: header.flags.bits(),
        protocol: None,
        realms: None,
        suppress_ifgroup: None,
        suppress_prefixlen: None,
        selectors: Selectors {
            tos: header.tos,
            ..Selectors::default()
        },
        unsupported_attributes: Vec::new(),
    };
    let (mut source, mut destination) = (None, None);
    let selectors = &mut item.selectors;
    for attribute in message.attributes {
        match attribute {
            RuleAttribute::Destination(v) => destination = Some(v),
            RuleAttribute::Source(v) => source = Some(v),
            RuleAttribute::Iifname(v) => selectors.input_interface = Some(v),
            RuleAttribute::Oifname(v) => selectors.output_interface = Some(v),
            RuleAttribute::Goto(v) => item.goto_priority = Some(v),
            RuleAttribute::Priority(v) => item.priority = v,
            RuleAttribute::FwMark(v) => selectors.fwmark = Some(v),
            RuleAttribute::FwMask(v) => selectors.fwmask = Some(v),
            RuleAttribute::Realm(v) => item.realms = Some([v.source, v.destination]),
            RuleAttribute::TunId(v) => selectors.tunnel_id = Some(v.to_string()),
            RuleAttribute::SuppressIfGroup(v) => {
                item.suppress_ifgroup = (v != u32::MAX).then_some(v)
            }
            RuleAttribute::SuppressPrefixLen(v) => {
                item.suppress_prefixlen = (v != u32::MAX).then_some(v)
            }
            RuleAttribute::Table(v) => item.table = v,
            RuleAttribute::L3MDev(v) => selectors.l3mdev = v,
            RuleAttribute::UidRange(v) => selectors.uid_range = Some([v.start, v.end]),
            RuleAttribute::Protocol(v) => item.protocol = Some(v.into()),
            RuleAttribute::IpProtocol(v) => selectors.ip_protocol = Some(v.into()),
            RuleAttribute::SourcePortRange(v) => {
                selectors.source_port_range = Some([v.start, v.end])
            }
            RuleAttribute::DestinationPortRange(v) => {
                selectors.destination_port_range = Some([v.start, v.end])
            }
            // The kernel may align an adjacent 64-bit attribute with an empty pad.
            other if other.kind() == FRA_PAD && other.value_len() == 0 => {}
            other => {
                let mut value = vec![0; other.value_len()];
                other.emit_value(&mut value);
                item.unsupported_attributes.push(UnsupportedAttribute {
                    kernel_type: other.kind(),
                    digest: psios_api::sha256(&value),
                });
            }
        }
    }
    // Linux may omit a zero mark even when its mask is set.
    if selectors.fwmask.is_some() {
        selectors.fwmark.get_or_insert(0);
    }
    selectors.source = prefix(family, source, header.src_len)?;
    selectors.destination = prefix(family, destination, header.dst_len)?;
    Ok(item)
}

fn collect(q: &RuleQuery) -> Result<Vec<Rule>> {
    let mut observer = Observer::open()?;
    let mut rules = Vec::new();
    for family in Family::ALL {
        if q.family.is_some_and(|wanted| wanted != family) {
            continue;
        }
        let mut request = RuleMessage::default();
        request.header.family = family.kernel();
        let first = rules.len();
        observer.dump(RouteNetlinkMessage::GetRule(request), |message| {
            let RouteNetlinkMessage::NewRule(record) = message else {
                return Err(err(
                    "DependencyUnavailable",
                    "unexpected kernel rule response",
                ));
            };
            // A priority can occur more than once. Kernel traversal order is significant.
            rules.push(rule(family, rules.len() - first, record)?);
            Ok(())
        })?;
    }
    Ok(rules)
}

pub fn observe_rules(q: &RuleQuery, w: &Work) -> Result<Value> {
    let namespace = namespace()?;
    let user_namespace_inode = fs::metadata("/proc/self/ns/user")?.ino().to_string();
    let rules = collect(q)?;
    let digest = psios_api::sha256(&serde_json::to_vec(&(
        "psios/node.routing_rules/1",
        &w.target.id,
        &w.task.owner,
        &w.task.spec["delegation_ref"],
        &namespace,
        &user_namespace_inode,
        q.family,
        &rules,
    ))?);
    let (entries, next) =
        snapshot::page(&rules, q.cursor.as_ref(), q.limit.unwrap_or(32), &digest)?;
    Ok(json!({
        "rules": entries, "snapshot_digest": digest, "next_cursor": next,
        "boot_id": namespace.boot_id, "network_namespace_inode": namespace.network_namespace_inode,
        "user_namespace_inode": user_namespace_inode, "observed_at": now(),
    }))
}
