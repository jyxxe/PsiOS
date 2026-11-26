//! Verify one link and its applied IPv4 address/connected route through RTNETLINK.
use super::super::{
    kernel::{self, Observer},
    routes,
};
use super::profile::Ipv4;
use crate::{Result, err, require};
use netlink_packet_route::{
    AddressFamily, RouteNetlinkMessage,
    link::{InfoKind, LinkAttribute, LinkFlags, LinkInfo, LinkLayerType, LinkMessage},
    route::{RouteAddress, RouteAttribute, RouteFlags, RouteMessage, RouteScope, RouteType},
};
use serde::{Deserialize, Serialize};
use std::{
    fmt::Write,
    net::{IpAddr, Ipv4Addr},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    pub interface: String,
    pub ifindex: u32,
    pub mac: String,
}

pub(super) fn validate_interface(name: &str) -> Result<()> {
    require(
        !name.is_empty()
            && name.len() <= 15
            && name != "lo"
            && name.as_bytes()[0].is_ascii_alphanumeric()
            && name
                .bytes()
                .all(|value| value.is_ascii_alphanumeric() || b"._-".contains(&value)),
        "InvalidArgument",
        "invalid NetworkManager Ethernet interface name",
    )
}

pub(super) fn read(name: &str) -> Result<Link> {
    validate_interface(name)?;
    let mut selected = None;
    Observer::open()?.dump(RouteNetlinkMessage::GetLink(LinkMessage::default()), |message| {
        let RouteNetlinkMessage::NewLink(link) = message else {
            return Err(err("DependencyUnavailable", "unexpected kernel link response"));
        };
        if !link.attributes.iter().any(|attr| matches!(attr, LinkAttribute::IfName(value) if value == name)) {
            return Ok(());
        }
        require(selected.is_none() && link.header.index > 0 && link.header.link_layer_type == LinkLayerType::Ether
            && !link.header.flags.intersects(LinkFlags::Loopback | LinkFlags::Pointopoint | LinkFlags::Controller | LinkFlags::Port),
            "PreconditionFailed", "managed link is not independent Ethernet")?;
        let mut mac = None;
        for attr in &link.attributes {
            match attr {
                LinkAttribute::Address(bytes) => {
                    require(mac.is_none() && bytes.len() == 6 && bytes[0] & 1 == 0 && bytes.iter().any(|byte| *byte != 0),
                        "PreconditionFailed", "invalid managed Ethernet hardware address")?;
                    let mut encoded = String::with_capacity(17);
                    for (index, byte) in bytes.iter().enumerate() {
                        if index != 0 { encoded.push(':'); }
                        write!(encoded, "{byte:02x}").expect("encode hardware address");
                    }
                    mac = Some(encoded);
                }
                LinkAttribute::Controller(index) if *index != 0 => {
                    return Err(err("PreconditionFailed", "managed link belongs to a kernel controller"));
                }
                LinkAttribute::LinkInfo(info) => {
                    require(info.iter().all(|item| !matches!(item, LinkInfo::Kind(kind) if *kind != InfoKind::Veth)),
                        "UnsupportedCapability", "unsupported managed Ethernet link kind")?;
                }
                _ => ()
            }
        }
        selected = Some(Link { interface: name.to_owned(), ifindex: link.header.index,
            mac: mac.ok_or_else(|| err("DependencyUnavailable", "kernel did not report the managed hardware address"))? });
        Ok(())
    })?;
    selected.ok_or_else(|| err("PreconditionFailed", "managed Ethernet interface is absent"))
}

fn destination(route: &RouteMessage) -> Option<Ipv4Addr> {
    route.attributes.iter().find_map(|attr| match attr {
        RouteAttribute::Destination(RouteAddress::Inet(ip)) => Some(*ip),
        _ => None,
    })
}

fn verify_routes(link: &Link, ipv4: Ipv4) -> Result<()> {
    let mut observer = Observer::open()?;
    let mut connected = 0;
    for family in [AddressFamily::Inet, AddressFamily::Inet6] {
        let mut request = RouteMessage::default();
        request.header.address_family = family;
        observer.dump(RouteNetlinkMessage::GetRoute(request), |message| {
            let RouteNetlinkMessage::NewRoute(route) = message else {
                return Err(err("DependencyUnavailable", "unexpected kernel route response"));
            };
            if route.header.flags.contains(RouteFlags::Cloned) {
                return Ok(());
            }
            require(!route.attributes.iter().any(|arg| matches!(arg, RouteAttribute::NhId(_))),
                "UnsupportedCapability", "indirect kernel nexthops prevent exact managed route verification")?;
            let on_link = route.attributes.iter().any(|arg| match arg {
                RouteAttribute::Oif(index) => *index == link.ifindex,
                RouteAttribute::MultiPath(hops) => hops.iter().any(|hop| hop.interface_index == link.ifindex),
                _ => false,
            });
            if !on_link { return Ok(()); }
            require(family == AddressFamily::Inet && !route.attributes.iter().any(|arg| matches!(arg,
                RouteAttribute::Gateway(_) | RouteAttribute::Via(_) | RouteAttribute::MultiPath(_))),
                "PreconditionFailed", "managed link has an unexpected gateway, multipath or IPv6 route")?;
            if routes::table(&route) == 255 {
                let broadcast = Ipv4Addr::from(u32::from(ipv4.network()) | (u32::MAX >> ipv4.prefix));
                require(route.header.destination_prefix_length == 32 && match route.header.kind {
                    RouteType::Local => destination(&route) == Some(ipv4.address),
                    RouteType::Broadcast => destination(&route).is_some_and(|ip| ip == ipv4.network() || ip == broadcast),
                    _ => false,
                }, "PreconditionFailed", "managed link has an unrelated local route")?;
                return Ok(());
            }
            require(routes::table(&route) == 254 && route.header.kind == RouteType::Unicast
                && route.header.scope == RouteScope::Link && route.header.source_prefix_length == 0
                && route.header.destination_prefix_length == ipv4.prefix && destination(&route) == Some(ipv4.network())
                && route.attributes.iter().any(|attr| matches!(attr, RouteAttribute::PrefSource(RouteAddress::Inet(ip)) if *ip == ipv4.address)),
                "PreconditionFailed", "managed link has an unexpected connected route")?;
            connected += 1;
            Ok(())
        })?;
    }
    require(
        connected == 1,
        "PreconditionFailed",
        "managed link lacks its exact connected IPv4 route",
    )
}

pub(super) fn verifies(link: &Link, ipv4: Ipv4, choices: [Ipv4; 2]) -> Result<()> {
    let interfaces = kernel::observe_interfaces()?;
    require(
        interfaces.iter().all(|other| {
            other.ifindex == link.ifindex
                || other.addr_info.iter().all(|address| {
                    choices
                        .iter()
                        .all(|choice| address.local != IpAddr::V4(choice.address))
                })
        }),
        "PreconditionFailed",
        "an approved profile address is already in use on another interface",
    )?;
    let observed = interfaces
        .iter()
        .find(|item| item.ifindex == link.ifindex && item.ifname == link.interface)
        .ok_or_else(|| {
            err(
                "PreconditionFailed",
                "managed kernel interface identity changed",
            )
        })?;
    require(
        observed.flags.iter().any(|flag| flag == "UP")
            && observed.flags.iter().any(|flag| flag == "LOWER_UP")
            && observed.addr_info.len() == 1
            && observed.addr_info.iter().any(|address| {
                address.family == "inet"
                    && address.scope == "global"
                    && address.local == IpAddr::V4(ipv4.address)
                    && address.prefixlen == ipv4.prefix
                    && address.peer.is_none()
            }),
        "PreconditionFailed",
        "managed link lacks its exact isolated IPv4 address",
    )?;
    verify_routes(link, ipv4)?;
    require(
        read(&link.interface)? == *link,
        "PreconditionFailed",
        "managed kernel interface identity changed",
    )
}
