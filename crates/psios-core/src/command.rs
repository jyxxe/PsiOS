use crate::*;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use std::{
    io::{ErrorKind, Read},
    process::{Child, ChildStdout, Command, Stdio},
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

pub(crate) fn capture_bytes(
    mut cmd: Command,
    max_bytes: usize,
    timeout: Duration,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    check_authority()?;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| capture_unavailable())?;
    let stdout = child.stdout.take().unwrap();
    let output = read_capture(&mut child, stdout, max_bytes, timeout, check_authority);
    if output.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    output
}

fn capture_unavailable() -> Error {
    err(
        "DependencyUnavailable",
        "image exporter did not confirm the request",
    )
}

fn read_capture(
    child: &mut Child,
    mut stdout: ChildStdout,
    max_bytes: usize,
    timeout: Duration,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let flags = fcntl_getfl(&stdout).map_err(|_| capture_unavailable())?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK).map_err(|_| capture_unavailable())?;
    let deadline = Instant::now() + timeout;
    let check_interval = Duration::from_millis(100);
    let mut recheck = Instant::now() + check_interval;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut eof = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(capture_unavailable());
        }
        if now >= recheck {
            check_authority()?;
            recheck = now + check_interval;
        }
        if !eof {
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    require(
                        count <= max_bytes - bytes.len(),
                        "QuotaExceeded",
                        "image archive exceeds capture limit",
                    )?;
                    bytes.extend_from_slice(&buffer[..count]);
                    continue;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => (),
                Err(_) => return Err(capture_unavailable()),
            }
        }
        if let Some(status) = child.try_wait().map_err(|_| capture_unavailable())? {
            if !status.success() {
                return Err(capture_unavailable());
            }
            if eof {
                if bytes.is_empty() {
                    return Err(capture_unavailable());
                }
                check_authority()?;
                return Ok(bytes);
            }
        }
        let interval = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(15));
        if eof {
            thread::sleep(interval);
        } else {
            let elapsed = Timespec::try_from(interval).expect("capture interval fits timespec");
            match poll(&mut [PollFd::new(&stdout, PollFlags::IN)], Some(&elapsed)) {
                Ok(_) | Err(Errno::INTR) => (),
                Err(_) => return Err(capture_unavailable()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_capture_stops_at_limit_and_rechecks_authority() {
        let mut exact = Command::new("/bin/sh");
        exact.args(["-c", "printf 'a\\000b'"]);
        let bytes = capture_bytes(exact, 3, Duration::from_secs(2), &mut || Ok(())).unwrap();
        assert_eq!(bytes, b"a\0b");

        let mut excess = Command::new("/bin/sh");
        excess.args(["-c", "printf '1234'; exec sleep 5"]);
        let failure = capture_bytes(excess, 3, Duration::from_secs(2), &mut || Ok(()))
            .expect_err("excess output must stop the child");
        assert_eq!(failure.0.code, "QuotaExceeded");

        let mut waiting = Command::new("/bin/sleep");
        waiting.arg("5");
        let mut checks = 0;
        let failure = capture_bytes(waiting, 3, Duration::from_secs(2), &mut || {
            checks += 1;
            require(checks == 1, "PermissionDenied", "fixture revoked capture")
        })
        .expect_err("revocation must stop the child");
        assert_eq!(failure.0.code, "PermissionDenied");
        assert_eq!(checks, 2);
    }

    #[test]
    fn binary_capture_times_out_with_inherited_stdout() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("sleeper");
        let mut background = Command::new("/bin/sh");
        background.args(["-c", "sleep 5 & printf '%s' \"$!\" > \"$1\"", "fixture"]);
        background.arg(&pid_file);
        let start = Instant::now();
        let result = capture_bytes(background, 3, Duration::from_millis(200), &mut || Ok(()));
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::TERM,
        );
        assert_eq!(
            result.expect_err("open pipe must time out").0.code,
            "DependencyUnavailable"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
