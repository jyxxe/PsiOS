"""Compare authorized mount topology with the real Linux namespace."""
import base64
import os
from pathlib import Path
import subprocess
import unittest

from support import Failure, Node, key


class MountTests(unittest.TestCase):
    def setUp(self):
        self.node = Node("target/debug")
        self.addCleanup(self.node.close)
        self.authority = self.node.call("Principal.WhoAmI", {})["authority_id"]

    def observe(self):
        n = self.node
        grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "units": 1, "ttl_seconds": 120,
            "actions": ["task.submit", "task.read", "node.mounts"], "tool_targets": [self.authority],
        })
        request = n.prepare("node.mounts", {}, key(), delegation=grant["delegation_ref"]["resource_id"])
        receipt = n.call("Task.Submit", request)
        result = n.wait(receipt["task_id"])
        self.assertEqual(result["lifecycle"], "Succeeded", result)
        reference, = result["result"]["artifact_refs"]
        return n.call("Artifact.Read", {"reference": reference})

    def test_same_namespace_and_distinct_grant(self):
        n = self.node
        n.perform("node.inventory", {})
        with self.assertRaises(Failure) as unauthorized:
            n.prepare("node.mounts", {})
        self.assertEqual(unauthorized.exception.code, "PermissionDenied")
        report = self.observe()
        self.assertEqual(report["mount_namespace_inode"], str(Path("/proc/self/ns/mnt").stat().st_ino))
        self.assertEqual(report["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        by_id = {r["mount_id"]: r for r in report["mounts"]}
        self.assertEqual(len(by_id), len(report["mounts"]))
        self.assertEqual(list(by_id), sorted(by_id))
        raw = Path("/proc/self/mountinfo").read_bytes().splitlines()
        # A process root may have hidden stacked entries. Compare by kernel ID.
        root = next(line.split(b" ") for line in raw if line.split(b" ")[4] == b"/")
        mount = by_id[int(root[0])]
        self.assertEqual(mount["mount_point"], "/")
        self.assertEqual(mount["parent_mount_id"], int(root[1]))
        self.assertEqual(mount["filesystem_type"], root[root.index(b"-") + 1].decode())
        major, minor = map(int, root[2].split(b":"))
        self.assertEqual(mount["device"], {"major": major, "minor": minor})
        self.assertEqual(mount["read_only"], root[5].split(b",")[0] == b"ro")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_MOUNT_TOPOLOGY"), "requires a private mount namespace")
    def test_private_tmpfs_and_read_only_bind_with_linux_path_bytes(self):
        original = self.node.base / "tmpfs"
        alias = self.node.base / "space and \\ and \ttab"
        raw_alias = os.fsencode(self.node.base) + b"/unrepresentable-\xff"
        original.mkdir()
        alias.mkdir()
        os.mkdir(raw_alias)
        subprocess.run(["mount", "-t", "tmpfs", "-o", "nodev,nosuid,noexec,size=4m", "psi-fixture-private", original], check=True)
        self.addCleanup(subprocess.run, ["umount", original], check=True)
        subprocess.run(["mount", "--make-shared", original], check=True)
        for dest in (alias, raw_alias):
            subprocess.run(["mount", "--bind", original, dest], check=True)
            self.addCleanup(subprocess.run, ["umount", dest], check=True)
            subprocess.run(["mount", "--make-private", dest], check=True)
        subprocess.run(["mount", "-o", "remount,bind,ro,nosuid,nodev,noexec", alias], check=True)
        mount_rows = self.observe()["mounts"]
        source = next(m for m in mount_rows if m["mount_point"] == str(original))
        bind = next(m for m in mount_rows if m["mount_point"] == str(alias))
        raw = next(m for m in mount_rows if m["mount_point"] == {"base64": base64.b64encode(raw_alias).decode()})
        self.assertEqual((source["filesystem_type"], bind["filesystem_type"]), ("tmpfs", "tmpfs"))
        self.assertEqual(source["device"], bind["device"])
        self.assertEqual(bind["device"], raw["device"])
        self.assertEqual(len({source["mount_id"], bind["mount_id"], raw["mount_id"]}), 3)
        self.assertFalse(source["read_only"])
        self.assertTrue(bind["read_only"])
        self.assertFalse(bind["superblock_read_only"])
        self.assertTrue({"nodev", "nosuid", "noexec"}.issubset(bind["flags"]))
        self.assertGreater(source["propagation"]["shared_group"], 0)
        self.assertIsNone(bind["propagation"]["shared_group"])
        self.assertNotIn("psi-fixture-private", str(mount_rows))
