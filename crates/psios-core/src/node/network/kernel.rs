//! RTNETLINK inventory for minimal hosts without iproute2 JSON support.
use super::{Address, Interface};
use crate::{Result, err, require};
use futures_util::StreamExt;
use rtnetlink::{
    Handle,
    packet_core::{NLM_F_DUMP, NLM_F_DUMP_INTR, NLM_F_REQUEST, NetlinkMessage, NetlinkPayload},
    packet_route::{
        AddressFamily, RouteNetlinkMessage,
        address::{AddressAttribute, AddressMessage, AddressScope},
        link::{LinkAttribute, LinkFlags, LinkLayerType, LinkMessage},
    },
};
use std::{collections::BTreeMap, time::Duration};

async fn dump(
    handle: &mut Handle,
    query: RouteNetlinkMessage,
    mut observe: impl FnMut(RouteNetlinkMessage) -> Result<()>,
) -> Result<()> {
    let mut request = NetlinkMessage::from(query);
    request.header.flags = NLM_F_DUMP | NLM_F_REQUEST;
    let mut replies = handle
        .request(request)
        .map_err(|_| err("DependencyUnavailable", "cannot request network inventory"))?;
    let mut bytes = 0;
    while let Some(reply) = replies.next().await {
        bytes += reply.buffer_len();
        require(
            bytes <= 1024 * 1024,
            "QuotaExceeded",
            "network observation exceeds one MiB",
        )?;
        require(
            reply.header.flags & NLM_F_DUMP_INTR == 0,
            "Conflict",
            "network dump interrupted by change",
        )?;
        match reply.payload {
            NetlinkPayload::InnerMessage(message) => observe(message)?,
            NetlinkPayload::Done(done) if done.code == 0 => return Ok(()),
            _ => {
                return Err(err(
                    "DependencyUnavailable",
                    "kernel could not complete network observation",
                ));
            }
        }
    }
    Err(err(
        "DependencyUnavailable",
        "incomplete network observation",
    ))
}

fn link(message: LinkMessage) -> Result<Interface> {
    let mut name = None;
    let mut mtu = None;
    let mut state = None;
    for attr in message.attributes {
        match attr {
            LinkAttribute::IfName(value) => name = Some(value),
            LinkAttribute::Mtu(value) => mtu = Some(value),
            LinkAttribute::OperState(value) => state = Some(value.to_string()),
            _ => {}
        }
    }
    let flags = message.header.flags;
    let mut names = Vec::new();
    if flags.contains(LinkFlags::Up) && !flags.contains(LinkFlags::Running) {
        names.push("NO-CARRIER".into());
    }
    for (flag, label) in [
        (LinkFlags::Loopback, "LOOPBACK"),
        (LinkFlags::Broadcast, "BROADCAST"),
        (LinkFlags::Pointopoint, "POINTOPOINT"),
        (LinkFlags::Multicast, "MULTICAST"),
        (LinkFlags::Noarp, "NOARP"),
        (LinkFlags::Allmulti, "ALLMULTI"),
        (LinkFlags::Promisc, "PROMISC"),
        (LinkFlags::Controller, "MASTER"),
        (LinkFlags::Port, "SLAVE"),
        (LinkFlags::Debug, "DEBUG"),
        (LinkFlags::Dynamic, "DYNAMIC"),
        (LinkFlags::Automedia, "AUTOMEDIA"),
        (LinkFlags::Portsel, "PORTSEL"),
        (LinkFlags::Notrailers, "NOTRAILERS"),
        (LinkFlags::Up, "UP"),
        (LinkFlags::LowerUp, "LOWER_UP"),
        (LinkFlags::Dormant, "DORMANT"),
        (LinkFlags::Echo, "ECHO"),
    ] {
        if flags.contains(flag) {
            names.push(label.to_owned());
        }
    }
    // The packet reader maps unrecognized ARPHRD codes to Void.
    require(
        message.header.link_layer_type != LinkLayerType::Void,
        "UnsupportedCapability",
        "unsupported kernel link type",
    )?;
    let missing = || err("DependencyUnavailable", "incomplete kernel link record");
    Ok(Interface {
        ifindex: message.header.index,
        ifname: name.ok_or_else(missing)?,
        mtu: mtu.ok_or_else(missing)?,
        operstate: state.ok_or_else(missing)?,
        flags: names,
        link_type: message
            .header
            .link_layer_type
            .to_string()
            .to_ascii_lowercase(),
        addr_info: Vec::new(),
    })
}

fn address(message: AddressMessage) -> Result<Option<Address>> {
    let family = match message.header.family {
        AddressFamily::Inet => "inet",
        AddressFamily::Inet6 => "inet6",
        _ => return Ok(None),
    };
    let mut local = None;
    let mut address = None;
    for attr in message.attributes {
        match attr {
            AddressAttribute::Local(value) => local = Some(value),
            AddressAttribute::Address(value) => address = Some(value),
            _ => {}
        }
    }
    let local = local
        .or(address)
        .ok_or_else(|| err("DependencyUnavailable", "missing kernel interface address"))?;
    let scope = match message.header.scope {
        AddressScope::Universe => "global".to_owned(),
        other => other.to_string(),
    };
    Ok(Some(Address {
        family: family.into(),
        local,
        prefixlen: message.header.prefix_len,
        scope,
        peer: address.filter(|value| *value != local),
    }))
}

async fn inventory() -> Result<Vec<Interface>> {
    let (mut connection, mut handle, _) = rtnetlink::new_connection()?;
    connection.set_forward_done(true);
    tokio::spawn(connection);
    let mut links = BTreeMap::new();
    dump(
        &mut handle,
        RouteNetlinkMessage::GetLink(LinkMessage::default()),
        |message| {
            let RouteNetlinkMessage::NewLink(record) = message else {
                return Err(err(
                    "DependencyUnavailable",
                    "unexpected kernel link response",
                ));
            };
            let item = link(record)?;
            require(
                links.insert(item.ifindex, item).is_none(),
                "Conflict",
                "duplicate interface index",
            )
        },
    )
    .await?;
    dump(
        &mut handle,
        RouteNetlinkMessage::GetAddress(AddressMessage::default()),
        |message| {
            let RouteNetlinkMessage::NewAddress(record) = message else {
                return Err(err(
                    "DependencyUnavailable",
                    "unexpected kernel address response",
                ));
            };
            let index = record.header.index;
            if let Some(item) = address(record)? {
                links
                    .get_mut(&index)
                    .ok_or_else(|| err("Conflict", "link changed during address observation"))?
                    .addr_info
                    .push(item);
            }
            Ok(())
        },
    )
    .await?;
    Ok(links.into_values().collect())
}

pub(super) fn observe() -> Result<Vec<Interface>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), inventory())
            .await
            .map_err(|_| {
                err(
                    "DependencyUnavailable",
                    "kernel network inventory timed out",
                )
            })?
    })
}
