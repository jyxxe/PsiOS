//! Use Linux block uevents to wake the existing bounded sysfs reconciliation.
use rustix::{
    event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll},
    fd::OwnedFd,
    io::{self, Errno},
    net::{
        self, AddressFamily, RecvFlags, SocketFlags, SocketType,
        netlink::{KOBJECT_UEVENT, SocketAddrNetlink},
    },
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

pub(super) const MIN_INTERVAL: Duration = Duration::from_millis(250);
const MAX_DATAGRAMS: usize = 32;
const DATAGRAM_BYTES: usize = 4096;

pub(super) struct BlockWake {
    socket: OwnedFd,
    shutdown: Arc<OwnedFd>,
}

impl BlockWake {
    pub(super) fn open() -> io::Result<Self> {
        let socket = net::socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            Some(KOBJECT_UEVENT),
        )?;
        net::bind(&socket, &SocketAddrNetlink::new(0, 1))?;
        let shutdown = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?);
        Ok(Self { socket, shutdown })
    }

    pub(super) fn shutdown_fd(&self) -> Arc<OwnedFd> {
        self.shutdown.clone()
    }

    pub(super) fn notify_shutdown(shutdown: &OwnedFd) {
        while matches!(io::write(shutdown, &1u64.to_ne_bytes()), Err(Errno::INTR)) {}
    }

    pub(super) fn wait(&self, interval: Duration) -> io::Result<()> {
        let start = Instant::now();
        let earliest = start + MIN_INTERVAL;
        let deadline = start + interval;
        let mut datagrams = 0;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            let receive = now >= earliest && datagrams < MAX_DATAGRAMS;
            let until = if now < earliest {
                deadline.min(earliest)
            } else {
                deadline
            };
            let duration = Timespec::try_from(until - now).expect("device interval is bounded");
            let mut fds = [
                PollFd::new(&*self.shutdown, PollFlags::IN),
                PollFd::new(&self.socket, PollFlags::IN),
            ];
            let count = if receive { 2 } else { 1 };
            match poll(&mut fds[..count], Some(&duration)) {
                Err(Errno::INTR) => continue,
                Ok(_) => {}
                Err(error) => return Err(error),
            }
            if fds[0].revents().contains(PollFlags::IN) {
                return Ok(());
            }
            if fds
                .iter()
                .any(|fd| fd.revents().intersects(PollFlags::HUP | PollFlags::NVAL))
            {
                return Err(Errno::IO);
            }
            if receive && fds[1].revents().intersects(PollFlags::IN | PollFlags::ERR) {
                datagrams += 1;
                if self.receive_hint()? {
                    return Ok(());
                }
            }
        }
    }

    fn receive_hint(&self) -> io::Result<bool> {
        let mut message = [0; DATAGRAM_BYTES];
        match net::recvfrom(&self.socket, &mut message[..], RecvFlags::TRUNC) {
            Ok((_, count, sender)) => {
                let kernel = sender.and_then(|addr| SocketAddrNetlink::try_from(addr).ok())
                    == Some(SocketAddrNetlink::new(0, 1));
                Ok(kernel && message.get(..count).is_some_and(block_event))
            }
            // Overflow may have dropped a relevant wakeup. The resulting
            // scan still confirms every change through the normal sysfs path.
            Err(Errno::NOBUFS) => Ok(true),
            Err(Errno::AGAIN | Errno::INTR) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn block_event(message: &[u8]) -> bool {
    let Some(message) = message.strip_suffix(b"\0") else {
        return false;
    };
    let mut fields = message.split(|value| *value == 0);
    let Some(header) = fields.next() else {
        return false;
    };
    let mut header = header.splitn(2, |b| *b == b'@');
    let (Some(action), Some(path)) = (header.next(), header.next()) else {
        return false;
    };
    path.starts_with(b"/devices/")
        && matches!(action, b"add" | b"remove" | b"change" | b"move")
        && fields.any(|field| field == b"SUBSYSTEM=block")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_bounded_block_hints() {
        assert!(block_event(
            b"add@/devices/controller/block/disk\0SUBSYSTEM=block\0SEQNUM=1\0"
        ));
        assert!(block_event(
            b"remove@/devices/controller/block/disk\0SUBSYSTEM=block\0"
        ));
        assert!(!block_event(b"add@/devices/controller\0SUBSYSTEM=usb\0"));
        assert!(!block_event(b"add@/devices/controller\0SUBSYSTEM=block"));
        assert!(!block_event(
            b"add@/not-devices/controller\0SUBSYSTEM=block\0"
        ));
    }
}
