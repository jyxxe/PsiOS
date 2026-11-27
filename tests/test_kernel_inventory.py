"""Check the live kernel sources used by inventory and status."""
from contextlib import ExitStack
import os
from pathlib import Path
import time
import unittest

from support import Failure, Node
from test_pressure import private_bind


class KernelInventoryTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def status(self):
        return self.node.call("Tool.Call", {
            "tool": "system.status", "input": {}, "delegation_id": "owner-delegation",
        })["result"]

    def assert_live_status(self):
        earliest = time.clock_gettime(time.CLOCK_BOOTTIME)
        status = self.status()
        latest = time.clock_gettime(time.CLOCK_BOOTTIME)
        self.assertEqual(status["kernel_release"], os.uname().release, status)
        uptime = float(status["uptime_seconds"])
        self.assertGreaterEqual(uptime + 0.011, earliest, status)
        self.assertLessEqual(uptime, latest, status)

    def test_node_and_status_observe_real_linux_data(self):
        self.assert_live_status()
        data = self.node.perform("node.inventory", {})
        self.assertEqual(data["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertEqual(data["kernel_release"], os.uname().release)
        self.assertLessEqual(data["available_cpus"], len(os.sched_getaffinity(0)))
        self.assertEqual(len(data["load_average"]), 3)
        self.assertTrue(all(float(value) >= 0 for value in data["load_average"]))
        memtotal = next(
            int(line.split()[1]) * 1024
            for line in Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal:")
        )
        self.assertEqual(data["memory_bytes"]["MemTotal"], memtotal)
        self.assertEqual(data["kernel_observation"], {"memory": "Observed", "load_average": "Observed"})

    @unittest.skipUnless(os.getenv("PSIOS_TEST_KERNEL_INVENTORY_MOUNTS") == "1", "requires a private mount namespace")
    def test_independent_inventory_survives_foreign_linux_mounts(self):
        namespace = Path("/proc/self/ns/mnt").readlink()
        self.assertNotEqual(namespace, Path(f"/proc/{os.getppid()}/ns/mnt").readlink())
        for line in Path("/proc/self/mountinfo").read_text().splitlines():
            fields = line.split(" - ", 1)[0].split()
            if fields[4] in ("/", "/proc", "/proc/sys"):
                self.assertFalse(any(field.startswith("shared:") for field in fields[6:]), line)
        node = self.node
        fake_memory = node.base / "meminfo"
        fake_load = node.base / "loadavg"
        fake_uptime = node.base / "uptime"
        fake_release = node.base / "release"
        fake_memory.write_text("MemTotal: 1 kB\n")
        fake_load.write_text("55.00 55.00 55.00 1/2 3\n")
        fake_uptime.write_text("0.00 0.00\n")
        fake_release.write_text("foreign-release\n")
        with ExitStack() as mounted:
            mounted.enter_context(private_bind(fake_memory, "/proc/meminfo"))
            mounted.enter_context(private_bind(fake_uptime, "/proc/uptime"))
            mounted.enter_context(private_bind(fake_release, "/proc/sys/kernel/osrelease"))
            self.assert_live_status()
            limited = node.perform("node.inventory", {})
            self.assertEqual(limited["memory_bytes"], {})
            self.assertEqual(limited["kernel_observation"], {"memory": "Unsupported", "load_average": "Observed"})
            self.assertTrue(limited["load_average"])
            mounted.enter_context(private_bind(fake_load, "/proc/loadavg"))
            limited = node.perform("node.inventory", {})
            self.assertEqual(limited["load_average"], [])
            self.assertEqual(limited["kernel_observation"],
                             {"memory": "Unsupported", "load_average": "Unsupported"})
            self.assertEqual(limited["kernel_release"], os.uname().release)
            with private_bind(fake_release, "/proc/version"):
                with self.assertRaises(Failure) as refused:
                    self.status()
                self.assertEqual(refused.exception.code, "StorageUnavailable")
        self.assert_live_status()
        resumed = node.perform("node.inventory", {})
        self.assertEqual(resumed["kernel_observation"], {"memory": "Observed", "load_average": "Observed"})
