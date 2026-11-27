"""Check Linux service notifications against the real daemon and service manager."""
from contextlib import closing, contextmanager
import os
import signal
import socket
import sqlite3
import struct
import subprocess
from time import monotonic, sleep
import unittest

from support import Node, key


class ServiceNotificationTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.node.stop()

    @contextmanager
    def listener(self, abstract=False):
        if abstract:
            name = f"psios-notify-{key()}"
            address, environment = "\0" + name, "@" + name
        else:
            environment = str(self.node.base / "notify.sock")
            address = environment
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_PASSCRED, 1)
            listener.settimeout(8)
            listener.bind(address)
            yield listener, dict(os.environ, NOTIFY_SOCKET=environment)

    def notification(self, listener, process, expected):
        credentials = struct.Struct("iii")
        message, ancillary, flags, _ = listener.recvmsg(256, socket.CMSG_SPACE(credentials.size))
        self.assertEqual(flags & (socket.MSG_TRUNC | socket.MSG_CTRUNC), 0)
        self.assertIn(expected, message.splitlines(), message)
        self.assertEqual(len(ancillary), 1, ancillary)
        level, kind, raw = ancillary[0]
        self.assertEqual((level, kind), (socket.SOL_SOCKET, socket.SCM_CREDENTIALS))
        self.assertEqual(credentials.unpack(raw), (process.pid, os.getuid(), os.getgid()))

    def test_readiness_and_shutdown_are_emitted_by_the_main_process(self):
        for abstract in (False, True):
            with self.subTest(abstract=abstract), self.listener(abstract) as (listener, env):
                with subprocess.Popen([*self.node.daemon, "serve"], env=env) as process:
                    try:
                        self.notification(listener, process, b"READY=1")
                        self.assertEqual(self.node.call("Principal.WhoAmI", {})["principal_id"], "local-owner")
                        process.send_signal(signal.SIGTERM)
                        self.notification(listener, process, b"STOPPING=1")
                        self.assertEqual(process.wait(10), 0)
                        self.assertFalse(self.node.socket.exists())
                    finally:
                        if process.poll() is None:
                            process.kill()

    def test_missing_witness_never_emits_readiness_and_missing_listener_refuses_startup(self):
        head = self.node.witness / "head"
        saved = head.read_bytes()
        missing = head.with_name("withheld-head")
        head.rename(missing)
        try:
            with self.listener() as (listener, env):
                failed = subprocess.run([*self.node.daemon, "serve"], env=env,
                                        capture_output=True, text=True, timeout=10)
                self.assertNotEqual(failed.returncode, 0, failed)
                self.assertIn("RecoveryRequired", failed.stderr)
                listener.setblocking(False)
                with self.assertRaises(BlockingIOError):
                    listener.recv(256)
                self.assertFalse(head.exists())
        finally:
            missing.rename(head)
        self.assertEqual(head.read_bytes(), saved)
        endpoint = str(self.node.base / "missing-manager.sock")
        failed = subprocess.run([*self.node.daemon, "serve"], env=dict(os.environ, NOTIFY_SOCKET=endpoint),
                                capture_output=True, text=True, timeout=10)
        self.assertNotEqual(failed.returncode, 0, failed)
        self.assertIn("service manager notification failed", failed.stderr)
        self.assertFalse(self.node.socket.exists())

    def test_watchdog_uses_only_a_supervising_process_address(self):
        with self.listener() as (listener, env):
            env = dict(env, WATCHDOG_USEC="200000", WATCHDOG_PID=str(os.getpid()))
            with subprocess.Popen([*self.node.daemon, "serve"], env=env) as process:
                try:
                    self.notification(listener, process, b"READY=1")
                    listener.settimeout(0.35)
                    with self.assertRaises(TimeoutError):
                        listener.recv(256)
                    process.send_signal(signal.SIGTERM)
                    self.notification(listener, process, b"STOPPING=1")
                    self.assertEqual(process.wait(10), 0)
                finally:
                    if process.poll() is None:
                        process.kill()

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SYSTEMD_NOTIFY") == "1", "requires a real systemd user manager")
    def test_systemd_start_waits_for_a_usable_authority_and_fails_on_missing_witness(self):
        def launch(unit):
            return subprocess.run(
                ["systemd-run", "--user", "--quiet", "--collect", f"--unit={unit}",
                 "--property=Type=notify", "--property=NotifyAccess=main",
                 "--property=TimeoutStartSec=8s", "--property=TimeoutStopSec=12s",
                 "--property=CPUQuota=75%", "--property=MemoryMax=256M", "--property=TasksMax=64",
                 "--", *self.node.daemon, "serve"],
                capture_output=True, text=True, timeout=16,
            )

        def stop(unit):
            return subprocess.run(["systemctl", "--user", "stop", unit], capture_output=True, timeout=15)

        good = f"psios-ready-{key()}.service"
        bad = f"psios-unavailable-{key()}.service"
        self.addCleanup(stop, good)
        self.addCleanup(stop, bad)
        head = self.node.witness / "head"
        saved = head.with_name("withheld-head")
        head.rename(saved)
        try:
            refused = launch(bad)
            self.assertNotEqual(refused.returncode, 0, refused)
            self.assertFalse(self.node.socket.exists())
        finally:
            saved.rename(head)
        started = launch(good)
        self.assertEqual(started.returncode, 0, started.stderr)
        status = subprocess.check_output(
            ["systemctl", "--user", "show", good, "--property=ActiveState", "--property=StatusText"],
            text=True, timeout=8,
        )
        self.assertIn("ActiveState=active", status.splitlines())
        self.assertIn("StatusText=Core authority is serving", status.splitlines())
        self.assertEqual(self.node.call("Principal.WhoAmI", {})["principal_id"], "local-owner")
        self.assertEqual(stop(good).returncode, 0)
        self.assertFalse(self.node.socket.exists())

    @unittest.skipUnless(os.getenv("PSIOS_TEST_SYSTEMD_NOTIFY") == "1", "requires a real systemd user manager")
    def test_systemd_watchdog_detects_a_blocked_authority_and_reopens_existing_state(self):
        node = self.node
        unit = f"psios-watchdog-{key()}.service"
        systemctl = ["systemctl", "--user"]

        def stop():
            for verb in ("stop", "reset-failed"):
                subprocess.run([*systemctl, verb, unit], capture_output=True, timeout=15)

        def launch():
            return subprocess.run(
                ["systemd-run", "--user", "--quiet", f"--unit={unit}",
                 "--property=Type=notify", "--property=NotifyAccess=main",
                 "--property=WatchdogSec=1800ms", "--property=WatchdogSignal=SIGKILL",
                 "--property=TimeoutStartSec=8s", "--property=TimeoutStopSec=12s",
                 "--property=CPUQuota=75%", "--property=MemoryMax=256M", "--property=TasksMax=64",
                 "--", *node.daemon, "serve"], capture_output=True, text=True, timeout=16,
            )

        def property_(field):
            return subprocess.check_output(
                [*systemctl, "show", f"--property={field}", "--value", unit], text=True, timeout=8,
            ).strip()

        self.addCleanup(stop)
        started = launch()
        self.assertEqual(started.returncode, 0, started.stderr)
        before = node.call("Node.Get", {})["reference"]
        current = node.call("Node.WorkAdmission.Get", {})
        wait_until = monotonic() + 7
        for _ in range(2):
            last = int(property_("WatchdogTimestampMonotonic"))
            while int(property_("WatchdogTimestampMonotonic")) <= last:
                self.assertLess(monotonic(), wait_until)
                self.assertEqual(property_("ActiveState"), "active")
                sleep(0.07)
        self.assertEqual(node.call("Node.Get", {})["reference"], before)

        with closing(sqlite3.connect(node.state / "state.db", timeout=5)) as external:
            external.execute("BEGIN IMMEDIATE")
            try:
                wait_until = monotonic() + 8
                while property_("Result") != "watchdog":
                    self.assertLess(monotonic(), wait_until, property_("Result"))
                    sleep(0.1)
            finally:
                external.rollback()
        self.assertEqual(property_("ActiveState"), "failed")
        stop()
        resumed = launch()
        self.assertEqual(resumed.returncode, 0, resumed.stderr)
        self.assertEqual(node.call("Node.Get", {})["reference"], before)
        self.assertEqual(node.call("Node.WorkAdmission.Get", {}), current)
