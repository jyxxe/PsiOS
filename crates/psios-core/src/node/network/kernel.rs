//! Authenticated, bounded RTNETLINK dumps in the caller's network namespace.
use super::{Address, Interface};
use crate::{Result, err, require};
use netlink_packet_core::{
    NLM_F_DUMP, NLM_F_DUMP_INTR, NLM_F_REQUEST, NetlinkBuffer, NetlinkMessage, NetlinkPayload,
    Parseable,
};
use netlink_packet_route::{
    AddressFamily, RouteNetlinkMessage,
    address::{AddressAttribute, AddressMessage, AddressScope},
    link::{LinkAttribute, LinkFlags, LinkLayerType, LinkMessage},
};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::OwnedFd,
    io::Errno,
    net::{self, RecvFlags, SendFlags, SocketFlags, SocketType, netlink::SocketAddrNetlink},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const KERNEL: SocketAddrNetlink = SocketAddrNetlink::new(0, 0);

pub(super) struct Observer {
    socket: OwnedFd,
    deadline: Instant,
    sequence: u32,
}

impl Observer {
    pub(super) fn open() -> Result<Self> {
        // The default protocol for AF_NETLINK is NETLINK_ROUTE (zero).
        let socket = net::socket_with(
            net::AddressFamily::NETLINK,
            SocketType::RAW,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            None,
        )
        .map_err(io_error)?;
        net::bind(&socket, &KERNEL).map_err(io_error)?;
        Ok(Self {
            socket,
            deadline: Instant::now() + Duration::from_secs(5),
            sequence: 0,
        })
    }

    fn receive(&self, buf: &mut [u8]) -> Result<usize> {
        loop {
            let timeout = self.deadline.saturating_duration_since(Instant::now());
            let expired = || {
                err(
                    "DependencyUnavailable",
                    "kernel network observation timed out",
                )
            };
            if timeout.is_zero() {
                return Err(expired());
            }
            let timeout = Timespec::try_from(timeout).expect("network timeout fits timespec");
            let mut fds = [PollFd::new(&self.socket, PollFlags::IN)];
            match poll(&mut fds, Some(&timeout)) {
                Ok(0) => return Err(expired()),
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(e) => return Err(io_error(e)),
            }
            // MSG_TRUNC reports the full datagram length, even if the buffer was too small.
            match net::recvfrom(&self.socket, &mut *buf, RecvFlags::TRUNC) {
                Ok((_, len, sender)) => {
                    require(
                        sender.and_then(|addr| SocketAddrNetlink::try_from(addr).ok())
                            == Some(KERNEL),
                        "DependencyUnavailable",
                        "network dump sender is not the kernel",
                    )?;
                    require(
                        len != 0 && len <= buf.len(),
                        "QuotaExceeded",
                        "invalid network datagram size",
                    )?;
                    return Ok(len);
                }
                Err(Errno::AGAIN | Errno::INTR) => continue,
                Err(Errno::NOBUFS) => return Err(err("Conflict", "kernel network dump lost data")),
                Err(e) => return Err(io_error(e)),
            }
        }
    }

    pub(super) fn dump(
        &mut self,
        query: RouteNetlinkMessage,
        mut observe: impl FnMut(RouteNetlinkMessage) -> Result<()>,
    ) -> Result<()> {
        self.sequence += 1;
        let mut request = NetlinkMessage::from(query);
        request.header.sequence_number = self.sequence;
        request.header.flags = NLM_F_DUMP | NLM_F_REQUEST;
        request.finalize();
        let mut encoded = vec![0; request.buffer_len()];
        request.serialize(&mut encoded);
        let size =
            net::sendto(&self.socket, &encoded, SendFlags::empty(), &KERNEL).map_err(io_error)?;
        require(
            size == encoded.len(),
            "DependencyUnavailable",
            "incomplete network dump request",
        )?;
        let mut buf = vec![0; 128 * 1024];
        let mut bytes = 0;
        loop {
            let size = self.receive(&mut buf)?;
            bytes += size;
            require(
                bytes <= 1024 * 1024,
                "QuotaExceeded",
                "network dump exceeds one MiB",
            )?;
            if consume(&buf[..size], self.sequence, &mut observe)? {
                return Ok(());
            }
        }
    }
}

fn consume(
    mut messages: &[u8],
    sequence: u32,
    observe: &mut impl FnMut(RouteNetlinkMessage) -> Result<()>,
) -> Result<bool> {
    while !messages.is_empty() {
        let invalid = |_| err("DependencyUnavailable", "invalid kernel network message");
        let buffer = NetlinkBuffer::new_checked(&messages).map_err(invalid)?;
        let reply = NetlinkMessage::<RouteNetlinkMessage>::parse(&buffer).map_err(invalid)?;
        let len = buffer.length() as usize;
        messages = &messages[len.next_multiple_of(4).min(messages.len())..];
        require(
            reply.header.sequence_number == sequence,
            "DependencyUnavailable",
            "unexpected network dump sequence",
        )?;
        require(
            reply.header.flags & NLM_F_DUMP_INTR == 0,
            "Conflict",
            "network dump interrupted by change",
        )?;
        match reply.payload {
            NetlinkPayload::InnerMessage(message) => observe(message)?,
            NetlinkPayload::Done(done) if done.code == 0 && messages.is_empty() => return Ok(true),
            _ => {
                return Err(err(
                    "DependencyUnavailable",
                    "kernel could not complete network observation",
                ));
            }
        }
    }
    Ok(false)
}

fn io_error(e: Errno) -> crate::Error {
    err(
        "DependencyUnavailable",
        format!("kernel network observation: {e}"),
    )
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

pub(super) fn observe_interfaces() -> Result<Vec<Interface>> {
    let mut observer = Observer::open()?;
    let mut links = BTreeMap::new();
    observer.dump(
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
    )?;
    observer.dump(
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
    )?;
    Ok(links.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use netlink_packet_core::{DoneMessage, NetlinkHeader};

    fn packet(mut reply: NetlinkMessage<RouteNetlinkMessage>, flags: u16) -> Vec<u8> {
        reply.header.sequence_number = 17;
        reply.header.flags = flags;
        reply.finalize();
        let mut bytes = vec![0; reply.buffer_len()];
        reply.serialize(&mut bytes);
        bytes
    }

    #[test]
    fn accepts_only_complete_matching_network_dumps() {
        let mut count = 0;
        let mut observe = |_| {
            count += 1;
            Ok(())
        };
        let record = packet(
            RouteNetlinkMessage::NewLink(LinkMessage::default()).into(),
            0,
        );
        let done = |code, flags| {
            let mut reply = DoneMessage::default();
            reply.code = code;
            packet(
                NetlinkMessage::new(NetlinkHeader::default(), NetlinkPayload::Done(reply)),
                flags,
            )
        };
        assert!(!consume(&record, 17, &mut observe).unwrap());
        assert!(consume(&done(0, 0), 17, &mut observe).unwrap());
        assert_eq!(
            consume(&done(0, NLM_F_DUMP_INTR), 17, &mut observe)
                .unwrap_err()
                .0
                .code,
            "Conflict"
        );
        assert!(consume(&done(-4, 0), 17, &mut observe).is_err());
        assert!(consume(&done(0, 0), 18, &mut observe).is_err());
        assert!(consume(&record[..record.len() - 1], 17, &mut observe).is_err());
        assert!(consume(&[done(0, 0), record].concat(), 17, &mut observe).is_err());
        assert_eq!(count, 1);
    }
}
