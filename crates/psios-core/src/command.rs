use crate::*;
use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
fn drain(r: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut r = r;
    let mut kept = Vec::new();
    let mut buf = [0; 8192];
    let mut truncated = false;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let take = n.min((192 * 1024usize).saturating_sub(kept.len()));
        kept.extend_from_slice(&buf[..take]);
        truncated |= take < n
    }
    Ok((kept, truncated))
}
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

pub fn capture(
    mut cmd: Command,
    effect: bool,
    adapter: &str,
    timeout_seconds: u64,
) -> Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|_| {
        err(
            "DependencyUnavailable",
            format!("{adapter} client unavailable"),
        )
    })?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || drain(stdout));
    let errors = thread::spawn(move || drain(stderr));
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let failure = || {
        err(
            if effect {
                "OutcomeUnknown"
            } else {
                "DependencyUnavailable"
            },
            format!("{adapter} did not confirm the request"),
        )
    };
    let (stdout, a) = out.join().map_err(|_| failure())?.map_err(|_| failure())?;
    let (stderr, b) = errors
        .join()
        .map_err(|_| failure())?
        .map_err(|_| failure())?;
    if !code.is_some_and(|s| s.success()) {
        return Err(failure());
    }
    Ok(Output {
        stdout,
        stderr,
        truncated: a || b,
    })
}
