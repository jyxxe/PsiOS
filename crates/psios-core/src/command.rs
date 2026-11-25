use crate::*;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use std::{
    io::{ErrorKind, Read, Write},
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

#[cfg(test)]
fn capture_bytes(
    cmd: Command,
    max_bytes: usize,
    timeout: Duration,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    capture_to(cmd, &mut output, max_bytes, timeout, check_authority)?;
    Ok(output)
}

/// Stream a nonempty binary response into a caller-owned sink that returns promptly.
/// The caller must discard partial output on error and finalize the sink on success.
pub(crate) fn capture_to(
    mut cmd: Command,
    output: &mut impl Write,
    max_bytes: usize,
    timeout: Duration,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<usize> {
    check_authority()?;
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| capture_unavailable())?;
    let mut child = CaptureChild(child);
    let stdout = child.0.stdout.take().unwrap();
    read_capture(
        &mut child.0,
        stdout,
        output,
        max_bytes,
        timeout,
        check_authority,
    )
}

struct CaptureChild(Child);

impl Drop for CaptureChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
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
    output: &mut impl Write,
    max_bytes: usize,
    timeout: Duration,
    check_authority: &mut impl FnMut() -> Result<()>,
) -> Result<usize> {
    let flags = fcntl_getfl(&stdout).map_err(|_| capture_unavailable())?;
    fcntl_setfl(&stdout, flags | OFlags::NONBLOCK).map_err(|_| capture_unavailable())?;
    let started = Instant::now();
    let deadline = started
        .checked_add(timeout)
        .ok_or_else(capture_unavailable)?;
    let check_interval = Duration::from_millis(100);
    let mut recheck = started + check_interval;
    let mut written = 0;
    let mut buffer = [0; 8192];
    let mut pending = 0..0;
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
        if !pending.is_empty() {
            match output.write(&buffer[pending.clone()]) {
                Ok(count) => {
                    require(
                        (1..=pending.len()).contains(&count),
                        "StorageUnavailable",
                        "capture sink did not accept its bytes",
                    )?;
                    pending.start += count;
                    written += count;
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => (),
                Err(e) => return Err(e.into()),
            }
            continue;
        }
        if !eof {
            match stdout.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(count) => {
                    require(
                        count <= max_bytes - written,
                        "QuotaExceeded",
                        "image archive exceeds capture limit",
                    )?;
                    pending = 0..count;
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
                if written == 0 {
                    return Err(capture_unavailable());
                }
                check_authority()?;
                return Ok(written);
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
    fn binary_capture_handles_short_and_interrupted_sink_writes() {
        #[derive(Default)]
        struct ShortWrites {
            bytes: Vec<u8>,
            interrupted: bool,
        }

        impl Write for ShortWrites {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(ErrorKind::Interrupted.into());
                }
                self.bytes.push(bytes[0]);
                Ok(1)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'a\\000b'"]);
        let mut sink = ShortWrites::default();
        assert_eq!(
            capture_to(
                command,
                &mut sink,
                3,
                Duration::from_secs(2),
                &mut || Ok(())
            )
            .unwrap(),
            3
        );
        assert_eq!(sink.bytes, b"a\0b");
    }

    #[test]
    fn binary_capture_reaps_the_owned_child_after_a_sink_error() {
        use rustix::process::{Pid, WaitOptions, waitpid};

        struct Reject;
        impl Write for Reject {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(ErrorKind::WriteZero.into())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("child");
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf '%s' $$ > \"$1\"; printf 'a'; exec sleep 5",
            "fixture",
        ]);
        command.arg(&pid_file);
        let failure = capture_to(command, &mut Reject, 3, Duration::from_secs(2), &mut || {
            Ok(())
        })
        .expect_err("an unavailable sink must stop the command");
        assert_eq!(failure.0.code, "StorageUnavailable");
        let pid = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_eq!(
            waitpid(Some(Pid::from_raw(pid).unwrap()), WaitOptions::NOHANG).unwrap_err(),
            Errno::CHILD
        );
    }

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
