"""Exercise rescue UART limits and errors with real child processes."""
from functools import partial
import importlib.util
import os
from pathlib import Path
import resource
import signal
import subprocess
import sys
import tempfile
import unittest


RESCUE = Path(__file__).resolve().parents[1] / "images" / "rescue"
spec = importlib.util.spec_from_file_location("psios_rescue_inspect", RESCUE / "inspect.py")
rescue = importlib.util.module_from_spec(spec)
sys.path.insert(0, str(RESCUE))
try:
    spec.loader.exec_module(rescue)
finally:
    sys.path.remove(str(RESCUE))


class RescueReportCaptureTests(unittest.TestCase):
    def setUp(self):
        work = tempfile.TemporaryDirectory(prefix="psios-test-rescue-report-")
        self.addCleanup(work.cleanup)
        self.work = Path(work.name)

    def capture(self, script, *args, **options):
        return rescue.capture_report([sys.executable, "-I", "-c", script, *args], cwd=self.work, **options)

    def assert_child_reaped(self):
        pid = int((self.work / "pid").read_text())
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_exact_byte_limit_waits_for_clean_eof_without_truncating(self):
        result = self.capture("import os; os.write(1, b'x' * 4096)", limit=4096, timeout=5)
        self.assertEqual((result.returncode, result.stdout), (0, b"x" * 4096))

    def test_oversized_live_child_is_reaped_without_echoing_report(self):
        script = """
import os
from pathlib import Path
import time
Path("pid").write_text(str(os.getpid()))
os.write(1, b"private-guest-report" * 300)
time.sleep(30)
"""
        with self.assertRaisesRegex(ValueError, r"^rescue QEMU report exceeded 4096 bytes$"):
            self.capture(script, limit=4096, timeout=5)
        self.assert_child_reaped()

    def test_deadline_still_applies_after_stdout_closes(self):
        script = """
import os
from pathlib import Path
import time
Path("pid").write_text(str(os.getpid()))
os.close(1)
time.sleep(30)
"""
        with self.assertRaises(subprocess.TimeoutExpired) as failure:
            self.capture(script, timeout=2)
        self.assertEqual(failure.exception.timeout, 2)
        self.assertIsNone(failure.exception.output)
        self.assert_child_reaped()

    def test_file_size_limit_signal_and_inherited_descriptor_are_preserved(self):
        script = """
import os
import resource
import signal
import sys
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
os.write(int(sys.argv[1]), b"x")
os.write(int(sys.argv[1]), b"x")
"""
        private = self.work / "limited"
        with private.open("xb") as output:
            fd = output.fileno()
            limit_file = partial(resource.setrlimit, resource.RLIMIT_FSIZE, (1, 1))
            result = self.capture(script, str(fd), pass_fds=(fd,), preexec_fn=limit_file, timeout=5)
        self.assertEqual(result.returncode, -signal.SIGXFSZ)
        self.assertEqual((result.stdout, private.read_bytes()), (b"", b"x"))

    def test_only_marker_counts_and_boot_modes_are_reported_on_failure(self):
        secret = "private-guest-report"
        marker = "\r\nPSIOS-RESCUE/1\r\n"
        for count in (0, 1, 2):
            serial = secret + (marker + secret) * count
            result = self.capture("import sys; sys.stdout.write(sys.argv[1])", serial, timeout=5)
            self.assertEqual(result.returncode, 0)
            if count == 1:
                self.assertEqual(rescue.unique_report(result.stdout, "uefi", "tcg"),
                                 serial.replace("\r\n", "\n").encode())
                continue
            with self.assertRaises(ValueError) as failure:
                rescue.unique_report(result.stdout, "uefi", "tcg")
            self.assertEqual(str(failure.exception), "rescue inspection failed: no unique guest report "
                             f"(firmware=uefi, accelerator=tcg, markers={count}, stdout_bytes={len(serial)})")


if __name__ == "__main__":
    unittest.main()
