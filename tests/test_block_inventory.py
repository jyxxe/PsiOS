"""Observe real Linux block devices through the scope-controlled node tool."""
import os
from pathlib import Path
import subprocess
import unittest

from support import Failure, Node
from test_block_layout import attached, scratch_gpt
from test_pressure import private_bind


SYSFS = Path("/sys/class/block")


class BlockInventoryTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)

    def inventory(self, **query):
        return self.node.perform("node.devices", query)

    def rejected_cursor(self, cursor, code):
        rejected = self.node.wait(self.node.submit("node.devices", {"cursor": cursor}))
        self.assertEqual((rejected["lifecycle"], rejected["conditions"][0]["type"]), ("Failed", code), rejected)
        self.assertEqual(rejected["result"]["artifact_refs"], [])

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

    def test_pages_match_real_inventory_and_refuse_other_authority_scopes(self):
        n = self.node
        full = self.inventory()
        page = self.inventory(limit=3)
        digest = page["snapshot_digest"]
        original_cursor = page["next_cursor"]
        rows = []
        while True:
            self.assertEqual((page["boot_id"], page["snapshot_digest"]), (full["boot_id"], digest))
            self.assertLessEqual(len(page["devices"]), 3)
            rows.extend(page["devices"])
            cursor = page["next_cursor"]
            if cursor is None:
                break
            self.assertEqual(cursor, {"snapshot_digest": digest, "offset": len(rows)})
            self.assertLess(len(rows), len(full["devices"]))
            page = self.inventory(limit=3, cursor=cursor)
        self.assertEqual(rows, full["devices"])

        if original_cursor is not None:
            actions = ["task.submit", "task.read", "node.devices"]
            actor, grant = n.agent(actions)
            token = actor["token"]
            node_id = n.call("Node.Get", {})["reference"]["resource_id"]
            another = n.call("Grant.Issue", {"principal_id": actor["principal_id"], "actions": actions,
                "tool_targets": [node_id], "units": 3, "ttl_seconds": 90})

            def read(query, delegation):
                prepared = n.prepare("node.devices", query, token=token, delegation=delegation["delegation_ref"]["resource_id"])
                return n.wait(n.call("Task.Submit", prepared, token)["task_id"])

            denied = read({"cursor": original_cursor}, grant)
            self.assertEqual(denied["conditions"][0]["type"], "ResyncRequired")
            first = read({"limit": 1}, grant)
            artifact, = first["result"]["artifact_refs"]
            cursor = n.call("Artifact.Read", {"reference": artifact}, token)["next_cursor"]
            self.assertEqual(read({"cursor": cursor}, grant)["lifecycle"], "Succeeded")
            denied = read({"cursor": cursor}, another)
            self.assertEqual(denied["conditions"][0]["type"], "ResyncRequired")
            self.assertEqual(denied["result"]["artifact_refs"], [])
            self.rejected_cursor({**original_cursor, "snapshot_digest": "sha256:" + "0" * 64}, "ResyncRequired")

        for query in ({"limit": 0}, {"limit": 33}, {"cursor": {"snapshot_digest": digest, "offset": 0}},
                      {"cursor": {"snapshot_digest": digest, "offset": 1024}}, {"cursor": {"offset": 1}}):
            with self.subTest(query=query), self.assertRaises(Failure) as rejected:
                n.prepare("node.devices", query)
            self.assertEqual(rejected.exception.code, "InvalidArgument")

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
            initial_cursor = self.inventory(limit=1)["next_cursor"]
            self.assertIsNotNone(initial_cursor)
            with first.open("r+b") as image:
                image.truncate(18 * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
            resized = get(name)
            self.assertEqual(resized["disk_sequence"], original["disk_sequence"])
            self.assertNotEqual(resized["sectors_512"], original["sectors_512"])
            self.rejected_cursor(initial_cursor, "ResyncRequired")
            resized_cursor = self.inventory(limit=1)["next_cursor"]

        with attached(second, name):
            reused = get(name)
            self.assertEqual(reused["major_minor"], original["major_minor"])
            self.assertEqual(reused["sectors_512"], original["sectors_512"])
            self.assertNotEqual(reused["disk_sequence"], original["disk_sequence"])
            self.rejected_cursor(resized_cursor, "ResyncRequired")

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
