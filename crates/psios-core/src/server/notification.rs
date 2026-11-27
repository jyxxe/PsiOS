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
            [b'@', name @ ..] if !name.is_empty() => SocketAddr::from_abstract_name(name),
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

    pub(super) fn watchdog_interval(&self) -> Result<Option<Duration>> {
        let Some(usec) = environment_number("WATCHDOG_USEC")? else {
            return Ok(None);
        };
        if environment_number("WATCHDOG_PID")?
            .is_some_and(|pid| pid != u64::from(std::process::id()))
        {
            return Ok(None);
        }
        require(
            self.0.is_some(),
            "InvalidArgument",
            "service watchdog requires a notification socket",
        )?;
        Ok(Some(Duration::from_micros((usec / 2).max(1))))
    }

    pub(super) fn heartbeat(&self) -> Result<()> {
        self.send(b"WATCHDOG=1")
    }

    pub(super) fn stopping(&self) -> Result<()> {
        self.send(b"STOPPING=1\nSTATUS=Core authority is shutting down")
    }
}

fn environment_number(key: &str) -> Result<Option<u64>> {
    env::var_os(key)
        .map(|raw| {
            raw.to_str()
                .and_then(|text| text.parse().ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| err("InvalidArgument", format!("invalid service watchdog {key}")))
        })
        .transpose()
}
