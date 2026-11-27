"""Observe real Linux block devices through the scope-controlled node tool."""
import os
from pathlib import Path
import subprocess
import unittest

from support import Node
from test_block_layout import attached, scratch_gpt
from test_pressure import private_bind


SYSFS = Path("/sys/class/block")


class BlockInventoryTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def inventory(self):
        return self.node.perform("node.devices", {})

    def assert_matches_kernel(self, report):
        self.assertEqual(report["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        rows = report["devices"]
        self.assertEqual([row["name"] for row in rows], sorted(path.name for path in SYSFS.iterdir()))
        for row in rows:
            path = SYSFS / row["name"]
            self.assertEqual(row["major_minor"], (path / "dev").read_text().strip())
            self.assertEqual(row["read_only"], (path / "ro").read_text().strip() == "1")
            self.assertEqual(row["sectors_512"], (path / "size").read_text().strip())
            for field, relative in (("disk_sequence", "diskseq"), ("logical_block_size", "queue/logical_block_size")):
                attribute = path / relative
                value = attribute.read_text().strip() if attribute.is_file() else None
                self.assertEqual(row.get(field), value)

    def test_verified_inventory_matches_real_block_devices(self):
        self.assert_matches_kernel(self.inventory())

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_LOOP") == "1", "requires read-only loop fixtures")
    def test_resize_and_device_number_reuse_are_visible_in_inventory(self):
        first = self.node.base / "first-inventory.raw"
        second = self.node.base / "second-inventory.raw"
        scratch_gpt(first)
        scratch_gpt(second)

        def get(name):
            report = self.inventory()
            row, = [row for row in report["devices"] if row["name"] == name]
            self.assertTrue(row["read_only"], row)
            return row

        with attached(first) as (name, _):
            original = get(name)
            self.assertGreater(int(original["disk_sequence"]), 0)
            with first.open("r+b") as image:
                image.truncate(18 * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
            resized = get(name)
            self.assertEqual(resized["disk_sequence"], original["disk_sequence"])
            self.assertNotEqual(resized["sectors_512"], original["sectors_512"])

        with attached(second, name):
            reused = get(name)
            self.assertEqual(reused["major_minor"], original["major_minor"])
            self.assertEqual(reused["sectors_512"], original["sectors_512"])
            self.assertNotEqual(reused["disk_sequence"], original["disk_sequence"])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_INVENTORY_MOUNTS") == "1", "requires a private mount namespace")
    def test_refuse_foreign_sysfs_index_and_attribute(self):
        namespace = Path("/proc/self/ns/mnt").readlink()
        self.assertNotEqual(namespace, Path(f"/proc/{os.getppid()}/ns/mnt").readlink())
        for line in Path("/proc/self/mountinfo").read_text().splitlines():
            fields = line.split(" - ", 1)[0].split()
            if fields[4] in ("/", "/sys", "/sys/dev"):
                self.assertFalse(any(field.startswith("shared:") for field in fields[6:]), line)
        before = self.inventory()
        alternate = self.node.base / "kernel-block-index"
        alternate.mkdir()
        with private_bind(alternate, "/sys/dev/block"):
            failed = self.node.wait(self.node.submit("node.devices", {}))
            self.assertEqual((failed["lifecycle"], failed["conditions"][0]["type"]),
                             ("Failed", "DependencyUnavailable"), failed)
            self.assertEqual(failed["result"]["artifact_refs"], [])
        device = next(iter(SYSFS.iterdir()), None)
        if device is not None:
            fake = self.node.base / "disallowed-size-source"
            fake.write_text("1\n")
            with private_bind(fake, device / "size"):
                refused = self.node.wait(self.node.submit("node.devices", {}))
                self.assertEqual((refused["lifecycle"], refused["conditions"][0]["type"]),
                                 ("Failed", "StorageUnavailable"), refused)
                self.assertEqual(refused["result"]["artifact_refs"], [])
        after = self.inventory()
        self.assertEqual(after["boot_id"], before["boot_id"])
        self.assert_matches_kernel(after)
