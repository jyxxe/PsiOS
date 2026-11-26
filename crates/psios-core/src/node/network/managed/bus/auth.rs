//! Read the SASL server GUID from the root-owned Unix transport before busctl use.
use super::{SOCKET, valid_guid};
use crate::{Error, Result, err, require};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
    net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType, connect, socket_with, sockopt},
};
use std::{
    io::{ErrorKind, Read, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

const REQUEST: &[u8] = b"\0AUTH EXTERNAL 30\r\n";
const REPLY_LEN: usize = 37;

fn unavailable() -> Error {
    err(
        "DependencyUnavailable",
        "local system D-Bus authentication failed",
    )
}

fn ready(stream: &UnixStream, interest: PollFlags, deadline: Instant) -> Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(unavailable());
        }
        let mut fds = [PollFd::new(stream, interest)];
        let timeout = Timespec::try_from(remaining).expect("local D-Bus timeout fits timespec");
        match poll(&mut fds, Some(&timeout)) {
            Ok(1) if fds[0].revents().contains(interest) => return Ok(()),
            Err(Errno::INTR) => continue,
            _ => return Err(unavailable()),
        }
    }
}

fn retryable(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock)
}

fn write_auth(stream: &mut UnixStream, deadline: Instant) -> Result<()> {
    let mut unsent = REQUEST;
    while !unsent.is_empty() {
        ready(stream, PollFlags::OUT, deadline)?;
        match stream.write(unsent) {
            Ok(sent) if sent > 0 => unsent = &unsent[sent..],
            Err(error) if retryable(&error) => (),
            _ => return Err(unavailable()),
        }
    }
    Ok(())
}

fn parse_reply(bytes: &[u8]) -> Result<String> {
    let guid = bytes
        .strip_prefix(b"OK ")
        .and_then(|value| value.strip_suffix(b"\r\n"))
        .and_then(|value| std::str::from_utf8(value).ok())
        .filter(|value| valid_guid(value))
        .ok_or_else(unavailable)?;
    Ok(guid.to_owned())
}

fn read_auth(stream: &mut UnixStream, deadline: Instant) -> Result<String> {
    let mut buffer = [0; REPLY_LEN + 1];
    let mut size = 0;
    while size < buffer.len() {
        ready(stream, PollFlags::IN, deadline)?;
        match stream.read(&mut buffer[size..]) {
            Ok(received) if received > 0 => size += received,
            Err(error) if retryable(&error) => continue,
            _ => return Err(unavailable()),
        }
        if buffer[..size].windows(2).any(|part| part == b"\r\n") {
            return parse_reply(&buffer[..size]);
        }
    }
    Err(unavailable())
}

pub(super) fn server_guid() -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let socket = socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        None,
    )
    .map_err(|_| unavailable())?;
    let address = SocketAddrUnix::new(SOCKET).map_err(|_| unavailable())?;
    let connected = connect(&socket, &address);
    let mut stream = UnixStream::from(socket);
    match connected {
        Ok(()) => (),
        Err(Errno::INPROGRESS) => {
            ready(&stream, PollFlags::OUT, deadline)?;
            sockopt::socket_error(&stream)
                .map_err(|_| unavailable())?
                .map_err(|_| unavailable())?;
        }
        Err(_) => return Err(unavailable()),
    }
    let peer = sockopt::socket_peercred(&stream).map_err(|_| unavailable())?;
    require(
        peer.uid.is_root(),
        "DependencyUnavailable",
        "local system D-Bus peer is not root",
    )?;
    write_auth(&mut stream, deadline)?;
    // Closing before BEGIN leaves the authenticated connection outside the message bus.
    read_auth(&mut stream, deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_one_canonical_sasl_guid_reply() {
        assert_eq!(
            parse_reply(b"OK 31692a8ceea25ab03f9896906b079aae\r\n").unwrap(),
            "31692a8ceea25ab03f9896906b079aae"
        );
        for invalid in [
            b"OK 00000000000000000000000000000000\r\n".as_slice(),
            b"OK 31692a8ceea25ab03f9896906b079aaE\r\n",
            b"OK 31692a8ceea25ab03f9896906b079aae\n",
            b"OK 31692a8ceea25ab03f9896906b079aae\r\nX",
            b"DATA 31692a8ceea25ab03f9896906b079aae\r\n",
            b"REJECTED EXTERNAL\r\n",
        ] {
            assert!(parse_reply(invalid).is_err());
        }
    }

    #[test]
    fn partial_sasl_reply_expires_while_peer_remains_connected() {
        let (mut stream, mut peer) = UnixStream::pair().unwrap();
        stream.set_nonblocking(true).unwrap();
        peer.write_all(b"OK 31692a8c").unwrap();
        let deadline = Instant::now() + Duration::from_millis(25);
        assert_eq!(
            read_auth(&mut stream, deadline).unwrap_err().0.code,
            "DependencyUnavailable"
        );
    }
}
