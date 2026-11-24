use super::{
    Family,
    kernel::{Observer, RouteReply},
    namespace, routes, user_namespace_inode,
};
use crate::{Result, now, require};
use netlink_packet_route::route::{RouteAttribute, RouteFlags, RouteMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::IpAddr;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Transport {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LookupQuery {
    destination: IpAddr,
    source: Option<IpAddr>,
    fwmark: Option<u32>,
    output_ifindex: Option<u32>,
    uid: Option<u32>,
    transport: Option<Transport>,
    source_port: Option<u16>,
    destination_port: Option<u16>,
}

impl LookupQuery {
    pub fn validate(&self) -> Result<()> {
        require(
            self.source
                .is_none_or(|source| source.is_ipv4() == self.destination.is_ipv4()),
            "InvalidArgument",
            "route lookup addresses require the same family",
        )?;
        require(
            self.output_ifindex
                .is_none_or(|index| (1..=i32::MAX as u32).contains(&index))
                && self.uid != Some(u32::MAX),
            "InvalidArgument",
            "route lookup interface index or UID is outside Linux range",
        )?;
        require(
            self.transport.is_some()
                || (self.source_port.is_none() && self.destination_port.is_none()),
            "InvalidArgument",
            "route lookup ports require a transport protocol",
        )
    }

    fn request(&self, family: Family, uid: u32) -> RouteMessage {
        let bits = match family {
            Family::Inet => 32,
            Family::Inet6 => 128,
        };
        let mut request = RouteMessage::default();
        request.header.address_family = family.kernel();
        request.header.destination_prefix_length = bits;
        request.header.flags = RouteFlags::LookupTable;
        request
            .attributes
            .push(RouteAttribute::Destination(self.destination.into()));
        request.attributes.push(RouteAttribute::Uid(uid));
        if let Some(source) = self.source {
            request.header.source_prefix_length = bits;
            request
                .attributes
                .push(RouteAttribute::Source(source.into()));
        }
        request
            .attributes
            .extend(self.fwmark.map(RouteAttribute::Mark));
        request
            .attributes
            .extend(self.output_ifindex.map(RouteAttribute::Oif));
        request.attributes.extend(self.transport.map(|protocol| {
            RouteAttribute::IpProto(match protocol {
                Transport::Tcp => 6,
                Transport::Udp => 17,
            })
        }));
        request
            .attributes
            .extend(self.source_port.map(RouteAttribute::Sport));
        request
            .attributes
            .extend(self.destination_port.map(RouteAttribute::Dport));
        request
    }
}

pub fn observe_lookup(query: &LookupQuery) -> Result<Value> {
    let namespace = namespace()?;
    let user_namespace_inode = user_namespace_inode()?;
    let uid = query
        .uid
        .unwrap_or_else(|| rustix::process::getuid().as_raw());
    let family = Family::from(query.destination);
    let reply = Observer::open()?.lookup(query.request(family, uid))?;
    let (outcome, route, errno) = match reply {
        RouteReply::Resolved(message) => {
            let table = routes::table(&message);
            (
                "Resolved",
                Some(routes::route(family, table, message)?),
                None,
            )
        }
        RouteReply::Rejected(code) => ("KernelRejected", None, Some(code)),
    };
    Ok(json!({
        "query": query, "lookup_uid": uid, "outcome": outcome, "route": route, "kernel_errno": errno,
        "boot_id": namespace.boot_id, "network_namespace_inode": namespace.network_namespace_inode,
        "user_namespace_inode": user_namespace_inode, "observed_at": now(),
    }))
}
