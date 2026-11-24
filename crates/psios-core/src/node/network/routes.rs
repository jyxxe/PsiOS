use super::{
    Family, Prefix,
    kernel::Observer,
    namespace,
    snapshot::{self, Cursor},
};
use crate::{Result, err, host::Work, now, require};
use netlink_packet_route::{
    AddressFamily, RouteNetlinkMessage,
    route::{RouteAddress, RouteAttribute, RouteFlags, RouteMessage, RouteNextHop, RouteVia},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteQuery {
    family: Option<Family>,
    table: Option<u32>,
    cursor: Option<Cursor>,
    limit: Option<usize>,
}

impl RouteQuery {
    pub fn validate(&self) -> Result<()> {
        snapshot::validate(self.cursor.as_ref(), self.limit.unwrap_or(32))?;
        require(
            self.table != Some(0),
            "InvalidArgument",
            "route table must be positive",
        )
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct NextHop {
    ifindex: u32,
    weight: u16,
    flags: u8,
    gateway: Option<IpAddr>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct Route {
    family: Family,
    table: u32,
    destination: Prefix,
    source: Option<Prefix>,
    tos: u8,
    protocol: u8,
    scope: u8,
    kind: u8,
    flags: u32,
    metric: Option<u32>,
    preferred_source: Option<IpAddr>,
    output_ifindex: Option<u32>,
    gateway: Option<IpAddr>,
    ipv6_preference: Option<u8>,
    nexthop_id: Option<u32>,
    multipath: Vec<NextHop>,
}

fn address(addr: RouteAddress) -> Result<IpAddr> {
    match addr {
        RouteAddress::Inet(v) => Ok(v.into()),
        RouteAddress::Inet6(v) => Ok(v.into()),
        _ => Err(err(
            "DependencyUnavailable",
            "invalid kernel route address family",
        )),
    }
}

fn via(addr: RouteVia) -> Result<IpAddr> {
    match addr {
        RouteVia::Inet(v) => Ok(v.into()),
        RouteVia::Inet6(v) => Ok(v.into()),
        _ => Err(err(
            "UnsupportedCapability",
            "kernel route uses a non-IP gateway",
        )),
    }
}

fn next_hop(hop: RouteNextHop) -> Result<NextHop> {
    let mut gateway = None;
    for attr in hop.attributes {
        match attr {
            RouteAttribute::Gateway(v) => gateway = Some(address(v)?),
            RouteAttribute::Via(v) => gateway = Some(via(v)?),
            _ => {}
        }
    }
    Ok(NextHop {
        ifindex: hop.interface_index,
        weight: u16::from(hop.hops) + 1,
        flags: hop.flags.bits(),
        gateway,
    })
}

fn route(family: Family, table: u32, record: RouteMessage) -> Result<Route> {
    let header = record.header;
    let (default, max_prefix, expected_family) = match family {
        Family::Inet => (Ipv4Addr::UNSPECIFIED.into(), 32, AddressFamily::Inet),
        Family::Inet6 => (Ipv6Addr::UNSPECIFIED.into(), 128, AddressFamily::Inet6),
    };
    require(
        header.address_family == expected_family
            && header.destination_prefix_length <= max_prefix
            && header.source_prefix_length <= max_prefix,
        "DependencyUnavailable",
        "invalid kernel route family or prefix length",
    )?;
    let mut destination = None;
    let mut item = Route {
        family,
        table,
        destination: Prefix {
            address: default,
            length: header.destination_prefix_length,
        },
        source: None,
        tos: header.tos,
        protocol: header.protocol.into(),
        scope: header.scope.into(),
        kind: header.kind.into(),
        flags: header.flags.bits(),
        metric: None,
        preferred_source: None,
        output_ifindex: None,
        gateway: None,
        ipv6_preference: None,
        nexthop_id: None,
        multipath: Vec::new(),
    };
    for attr in record.attributes {
        match attr {
            RouteAttribute::Destination(v) => destination = Some(address(v)?),
            RouteAttribute::Source(v) => {
                item.source = Some(Prefix {
                    address: address(v)?,
                    length: header.source_prefix_length,
                })
            }
            RouteAttribute::PrefSource(v) => item.preferred_source = Some(address(v)?),
            RouteAttribute::Gateway(v) => item.gateway = Some(address(v)?),
            RouteAttribute::Via(v) => item.gateway = Some(via(v)?),
            RouteAttribute::Oif(v) => item.output_ifindex = Some(v),
            RouteAttribute::Priority(v) => item.metric = Some(v),
            RouteAttribute::Preference(v) => item.ipv6_preference = Some(v.into()),
            RouteAttribute::NhId(v) => item.nexthop_id = Some(v),
            RouteAttribute::MultiPath(hops) => {
                item.multipath = hops.into_iter().map(next_hop).collect::<Result<_>>()?;
            }
            _ => {}
        }
    }
    require(
        (destination.is_some() || header.destination_prefix_length == 0)
            && (item.source.is_some() || header.source_prefix_length == 0),
        "DependencyUnavailable",
        "kernel route is missing a prefix address",
    )?;
    item.destination.address = destination.unwrap_or(default);
    Ok(item)
}

fn collect(q: &RouteQuery) -> Result<Vec<Route>> {
    let mut observer = Observer::open()?;
    let mut routes = Vec::new();
    for family in Family::ALL {
        if q.family.is_some_and(|wanted| wanted != family) {
            continue;
        }
        let mut request = RouteMessage::default();
        request.header.address_family = family.kernel();
        observer.dump(RouteNetlinkMessage::GetRoute(request), |message| {
            let RouteNetlinkMessage::NewRoute(record) = message else {
                return Err(err(
                    "DependencyUnavailable",
                    "unexpected kernel route response",
                ));
            };
            if record.header.flags.contains(RouteFlags::Cloned) {
                return Ok(());
            }
            let table = record
                .attributes
                .iter()
                .find_map(|attr| match attr {
                    RouteAttribute::Table(v) => Some(*v),
                    _ => None,
                })
                .unwrap_or_else(|| record.header.table.into());
            if q.table.is_none_or(|wanted| wanted == table) {
                routes.push(route(family, table, record)?);
            }
            Ok(())
        })?;
    }
    routes.sort_unstable();
    Ok(routes)
}

pub fn observe_routes(q: &RouteQuery, w: &Work) -> Result<Value> {
    let namespace = namespace()?;
    let routes = collect(q)?;
    let digest = psios_api::sha256(&serde_json::to_vec(&(
        "psios/node.routes/1",
        &w.target.id,
        &w.task.owner,
        &w.task.spec["delegation_ref"],
        &namespace,
        q.family,
        q.table,
        &routes,
    ))?);
    let (entries, next) =
        snapshot::page(&routes, q.cursor.as_ref(), q.limit.unwrap_or(32), &digest)?;
    Ok(json!({
        "routes": entries, "snapshot_digest": digest, "next_cursor": next,
        "boot_id": namespace.boot_id, "network_namespace_inode": namespace.network_namespace_inode,
        "observed_at": now(),
    }))
}
