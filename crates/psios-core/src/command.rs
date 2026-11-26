use crate::*;
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    fd::AsFd,
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
};
use std::{
    io::{self, ErrorKind, Read, Write},
    process::{Child, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn nonblocking(fd: impl AsFd) -> io::Result<()> {
    let fd = fd.as_fd();
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)
}

struct CapturePipe<R> {
    reader: Option<R>,
    bytes: Vec<u8>,
    truncated: bool,
}

impl<R: Read + AsFd> CapturePipe<R> {
    fn new(reader: R) -> io::Result<Self> {
        nonblocking(&reader)?;
        Ok(Self {
            reader: Some(reader),
            bytes: Vec::new(),
            truncated: false,
        })
    }

    fn poll_fd(&self) -> Option<PollFd<'_>> {
        self.reader
            .as_ref()
            .map(|reader| PollFd::new(reader, PollFlags::IN))
    }

    fn read(&mut self, buffer: &mut [u8], limit: usize, keep_end: bool) -> io::Result<bool> {
        let Some(reader) = &mut self.reader else {
            return Ok(false);
        };
        let count = match reader.read(buffer) {
            Ok(0) => {
                self.reader = None;
                return Ok(true);
            }
            Ok(count) => count,
            Err(e) if matches!(e.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };
        let remaining = limit.saturating_sub(self.bytes.len());
        self.truncated |= count > remaining;
        if keep_end {
            let take = count.min(limit);
            let discard = self.bytes.len().saturating_sub(limit - take);
            self.bytes.drain(..discard);
            self.bytes.extend_from_slice(&buffer[count - take..count]);
        } else {
            self.bytes
                .extend_from_slice(&buffer[..count.min(remaining)]);
        }
        Ok(true)
    }
}

pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

pub fn capture(cmd: Command, effect: bool, adapter: &str, timeout_seconds: u64) -> Result<Output> {
    capture_window(
        cmd,
        effect,
        adapter,
        Duration::from_secs(timeout_seconds),
        192 * 1024,
        false,
    )
}

pub(crate) fn capture_tail(
    cmd: Command,
    adapter: &str,
    timeout_seconds: u64,
    limit: usize,
) -> Result<Output> {
    capture_window(
        cmd,
        false,
        adapter,
        Duration::from_secs(timeout_seconds),
        limit,
        true,
    )
}

fn capture_window(
    mut cmd: Command,
    effect: bool,
    adapter: &str,
    timeout: Duration,
    limit: usize,
    keep_end: bool,
) -> Result<Output> {
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
    let deadline = Instant::now().checked_add(timeout).ok_or_else(failure)?;
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| {
            err(
                "DependencyUnavailable",
                format!("{adapter} client unavailable"),
            )
        })?;
    let mut child = CaptureChild(child);
    read_window(&mut child.0, deadline, limit, keep_end).map_err(|_| failure())
}

fn read_window(
    child: &mut Child,
    deadline: Instant,
    limit: usize,
    keep_end: bool,
) -> io::Result<Output> {
    let mut stdout = CapturePipe::new(child.stdout.take().unwrap())?;
    let mut stderr = CapturePipe::new(child.stderr.take().unwrap())?;
    let mut buffer = [0; 8192];
    loop {
        if Instant::now() >= deadline {
            return Err(ErrorKind::TimedOut.into());
        }
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other("adapter child exited unsuccessfully"));
            }
            // Other processes may still hold a pipe writer after the child exits.
            if stdout.reader.is_none() && stderr.reader.is_none() {
                return Ok(Output {
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                    truncated: stdout.truncated || stderr.truncated,
                });
            }
        }
        let read_stdout = stdout.read(&mut buffer, limit, keep_end)?;
        let read_stderr = stderr.read(&mut buffer, limit, keep_end)?;
        if read_stdout || read_stderr {
            continue;
        }
        let interval = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(15));
        let elapsed = Timespec::try_from(interval).expect("capture interval fits timespec");
        let mut fds: Vec<_> = [stdout.poll_fd(), stderr.poll_fd()]
            .into_iter()
            .flatten()
            .collect();
        match poll(&mut fds, Some(&elapsed)) {
            Ok(_) | Err(Errno::INTR) => (),
            Err(e) => return Err(e.into()),
        }
    }
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
    nonblocking(&stdout).map_err(|_| capture_unavailable())?;
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
    fn window_capture_preserves_both_streams_and_byte_limits() {
        let short = |limit, keep_end| {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "printf abcdef; printf 123456 >&2"]);
            capture_window(
                cmd,
                false,
                "pipe fixture",
                Duration::from_secs(2),
                limit,
                keep_end,
            )
            .unwrap()
        };
        let exact = short(6, false);
        assert_eq!(exact.stdout, b"abcdef");
        assert_eq!(exact.stderr, b"123456");
        assert!(!exact.truncated);
        let prefix = short(3, false);
        assert_eq!(prefix.stdout, b"abc");
        assert_eq!(prefix.stderr, b"123");
        assert!(prefix.truncated);

        let mut large = Command::new("/bin/sh");
        large.args([
            "-c",
            "head -c 131072 /dev/zero; printf out; head -c 131072 /dev/zero >&2; printf err >&2",
        ]);
        let tail = capture_window(
            large,
            false,
            "pipe fixture",
            Duration::from_secs(2),
            3,
            true,
        )
        .unwrap();
        assert_eq!(tail.stdout, b"out");
        assert_eq!(tail.stderr, b"err");
        assert!(tail.truncated);
    }

    #[test]
    fn window_capture_preserves_read_and_effect_failure_codes() {
        for (effect, code) in [(false, "DependencyUnavailable"), (true, "OutcomeUnknown")] {
            let mut cmd = Command::new("/bin/sh");
            cmd.args(["-c", "printf rejected >&2; exit 7"]);
            let error = capture(cmd, effect, "pipe fixture", 2)
                .err()
                .expect("an unsuccessful command must fail");
            assert_eq!(error.0.code, code);
        }
    }

    fn held_pipe_deadline(script: &str, effect: bool, expected: &str) {
        use rustix::process::{Pid, WaitOptions, waitpid};

        let temp = tempfile::tempdir().unwrap();
        let child = temp.path().join("child");
        let released = temp.path().join("released");
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script, "fixture"])
            .arg(&child)
            .arg(&released);
        let start = Instant::now();
        let result = capture_window(
            cmd,
            effect,
            "pipe fixture",
            Duration::from_millis(300),
            64,
            false,
        );
        let elapsed = start.elapsed();
        // The test's descendant releases its endpoint on its own. Wait for
        // its completion signal even if the command returned the wrong result.
        let cleanup_deadline = Instant::now() + Duration::from_secs(5);
        while !released.is_file() && Instant::now() < cleanup_deadline {
            thread::sleep(Duration::from_millis(15));
        }
        assert!(
            released.is_file(),
            "fixture did not finish releasing its pipe"
        );
        let pid = std::fs::read_to_string(child).unwrap().parse().unwrap();
        assert_eq!(
            waitpid(Some(Pid::from_raw(pid).unwrap()), WaitOptions::NOHANG).unwrap_err(),
            Errno::CHILD,
            "adapter child was not reaped",
        );
        assert_eq!(
            result.err().expect("open pipe must time out").0.code,
            expected
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "waited for the endpoint: {elapsed:?}"
        );
    }

    #[test]
    fn window_capture_times_out_when_stdout_outlives_a_successful_child() {
        held_pipe_deadline(
            "( sleep 3; : > \"$2\" ) 2>/dev/null & printf '%s' $$ > \"$1\"; printf partial; exit 0",
            false,
            "DependencyUnavailable",
        );
    }

    #[test]
    fn window_capture_reaps_timed_out_child_while_stderr_is_inherited() {
        held_pipe_deadline(
            "( sleep 3; : > \"$2\" ) >/dev/null & printf '%s' $$ > \"$1\"; exec sleep 8",
            true,
            "OutcomeUnknown",
        );
    }

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
