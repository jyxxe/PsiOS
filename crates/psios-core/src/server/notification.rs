//! Supervised startup and shutdown notifications sent by the daemon process.
use crate::{Result, err, require};
use std::{
    env, io,
    os::{
        linux::net::SocketAddrExt,
        unix::{
            ffi::OsStrExt,
            net::{SocketAddr, UnixDatagram},
        },
    },
    time::Duration,
};

pub(super) struct Notifier(Option<UnixDatagram>);

fn unavailable(error: io::Error) -> crate::Error {
    err(
        "DependencyUnavailable",
        format!("service manager notification failed: {error}"),
    )
}

impl Notifier {
    pub(super) fn configured() -> Result<Self> {
        let Some(name) = env::var_os("NOTIFY_SOCKET") else {
            return Ok(Self(None));
        };
        let address = match name.as_bytes() {
            [b'/', ..] => SocketAddr::from_pathname(&name),
            [b'@', name @ [_, ..]] => SocketAddr::from_abstract_name(name),
            _ => {
                return Err(err(
                    "InvalidArgument",
                    "invalid service manager notification address",
                ));
            }
        }
        .map_err(unavailable)?;
        let socket = UnixDatagram::unbound().map_err(unavailable)?;
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .map_err(unavailable)?;
        socket.connect_addr(&address).map_err(unavailable)?;
        Ok(Self(Some(socket)))
    }

    fn send(&self, message: &[u8]) -> Result<()> {
        if let Some(socket) = &self.0 {
            require(
                socket.send(message).map_err(unavailable)? == message.len(),
                "DependencyUnavailable",
                "incomplete service manager notification",
            )?;
        }
        Ok(())
    }

    pub(super) fn ready(&self) -> Result<()> {
        self.send(b"READY=1\nSTATUS=Core authority is serving")
    }

    pub(super) fn stopping(&self) -> Result<()> {
        self.send(b"STOPPING=1\nSTATUS=Core authority is shutting down")
    }
}
