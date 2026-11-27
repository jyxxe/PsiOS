"""Verify scoped volume accounting using the real Linux file metadata."""
import os
import signal
import subprocess
import unittest

from support import Failure, Node, key, target


class VolumeUsageTests(unittest.TestCase):
    def setUp(self):
        self.node = Node()
        self.addCleanup(self.node.close)
        self.volume = self.node.perform("volume.create", {"name": "measured-volume"})
        self.root = self.node.state / "volumes" / self.volume["resource_id"] / "data"

    def input(self, **extra):
        return dict(target=target(self.volume), **extra)

    def test_sparse_and_allocated_bytes_are_measured_independently(self):
        n = self.node
        directory = self.root / "nested"
        directory.mkdir()
        sparse = directory / "mostly-hole"
        dense = directory / "allocated"
        with sparse.open("wb") as f:
            f.truncate(64 * 1024 * 1024)
            f.write(b"a")
            os.fsync(f.fileno())
        with dense.open("wb") as f:
            f.write(b"b" * (128 * 1024))
            os.fsync(f.fileno())
        observed = [p.stat() for p in (sparse, dense)]
        self.assertLess(observed[0].st_blocks * 512, observed[0].st_size)
        self.assertGreater(observed[1].st_blocks, observed[0].st_blocks)

        other = n.perform("volume.create", {"name": "outside-scope"})
        grant = n.call("Grant.Issue", dict(principal_id="local-owner", actions=["task.submit", "task.read", "volume.usage"],
                       tool_targets=[self.volume["resource_id"]], ttl_seconds=60, units=2))
        delegation = grant["delegation_ref"]["resource_id"]
        outside_scope = n.prepare("volume.usage", {"target": target(other)}, key(), delegation=delegation)
        with self.assertRaises(Failure) as denied:
            n.call("Task.Submit", outside_scope)
        self.assertEqual(denied.exception.code, "PermissionDenied")
        unspent = n.call("Budget.Get", {"id": grant["budget_ref"]["resource_id"]})
        self.assertEqual((unspent["reserved"], unspent["spent"]), (0, 0))
        request = n.prepare("volume.usage", self.input(), key(), delegation=delegation)
        receipt = n.call("Task.Submit", request)
        task = n.wait(receipt["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        ref, = task["result"]["artifact_refs"]
        report = n.call("Artifact.Read", {"reference": ref})
        allocated_files = sum(s.st_blocks for s in observed) * 512
        allocated_dirs = (self.root.stat().st_blocks + directory.stat().st_blocks) * 512
        self.assertEqual(report["volume_ref"], self.volume)
        self.assertEqual(report["data_incarnation"], n.get(self.volume["resource_id"])["value"]["data_incarnation"])
        self.assertEqual((report["files"], report["directories"]), (2, 2))
        self.assertEqual(report["logical_file_bytes"], str(sum(s.st_size for s in observed)))
        self.assertEqual(report["allocated_file_bytes"], str(allocated_files))
        self.assertEqual(report["allocated_directory_bytes"], str(allocated_dirs))
        self.assertEqual(report["allocated_total_bytes"], str(allocated_files + allocated_dirs))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertTrue(n.call("Task.Submit", request)["deduplicated"])
        self.assertEqual(n.call("Artifact.Read", {"reference": ref}), report)
        subtree = n.perform("volume.usage", self.input(path="nested"))
        self.assertEqual((subtree["path"], subtree["files"], subtree["directories"]), ("nested", 2, 1))
        self.assertEqual(subtree["allocated_directory_bytes"], str(directory.stat().st_blocks * 512))

    def test_counts_distinct_raw_names_and_keeps_the_result_after_restart(self):
        n = self.node
        directory = self.root / "measured"
        directory.mkdir()
        raw_dir = os.fsencode(directory) + b"/nested-\xff"
        os.mkdir(raw_dir)
        files = [raw_dir + b"/record-\xfe", raw_dir + b"/record-\xff",
                 raw_dir + "/record-�".encode()]
        for size, path in enumerate(files, 1):
            with open(path, "wb") as stream:
                stream.truncate(size * 65536)
                stream.write(bytes([size]))
        result = n.wait(n.submit("volume.usage", self.input(path="measured")))
        self.assertEqual(result["lifecycle"], "Succeeded", result)
        ref, = result["result"]["artifact_refs"]
        report = n.call("Artifact.Read", {"reference": ref})
        self.assertEqual((report["files"], report["directories"]), (3, 2))
        self.assertEqual(report["logical_file_bytes"], str(sum(os.stat(p).st_size for p in files)))
        blocks = sum(os.stat(p).st_blocks for p in files) * 512
        dirs = (os.stat(raw_dir).st_blocks + directory.stat().st_blocks) * 512
        self.assertEqual(report["allocated_total_bytes"], str(blocks + dirs))
        n.stop(signal.SIGKILL)
        n.start()
        self.assertEqual(n.call("Artifact.Read", {"reference": ref}), report)
        os.link(files[0], raw_dir + b"/link-\x80")
        rejected = n.wait(n.submit("volume.usage", self.input(path="measured")))
        self.assertEqual(rejected["conditions"][0]["type"], "UnsupportedCapability", rejected)
        self.assertEqual(rejected["result"]["artifact_refs"], [])

    def test_rejects_links_and_special_files_without_holding_the_volume(self):
        n = self.node
        outside = n.base / "other-directory"
        outside.mkdir()
        (outside / "external").write_bytes(b"outside")
        link = self.root / "link"
        link.symlink_to(outside)
        for inputs in [self.input(), self.input(path="link")]:
            failed = n.wait(n.submit("volume.usage", inputs))
            self.assertEqual(failed["lifecycle"], "Failed", failed)
            self.assertEqual(failed["result"]["artifact_refs"], [], failed)
        link.unlink()
        external_link = self.root / "hard-link"
        os.link(outside / "external", external_link)
        failed = n.wait(n.submit("volume.usage", self.input()))
        self.assertEqual(failed["conditions"][0]["type"], "UnsupportedCapability", failed)
        external_link.unlink()
        fifo = self.root / "fifo"
        os.mkfifo(fifo)
        failed = n.wait(n.submit("volume.usage", self.input()))
        self.assertEqual(failed["conditions"][0]["type"], "UnsupportedCapability", failed)
        fifo.unlink()
        with self.assertRaises(Failure) as invalid:
            n.prepare("volume.usage", self.input(path="../other-directory"))
        self.assertEqual(invalid.exception.code, "InvalidArgument")
        self.assertEqual(n.perform("volume.usage", self.input())["files"], 0)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_BIND"), "requires a private mount namespace")
    def test_mount_boundary_rejects_even_a_bind_from_the_same_filesystem(self):
        n = self.node
        other = n.base / "outside"
        other.mkdir()
        (other / "outside-data").write_bytes(b"not in the volume")
        mountpoint = self.root / "local-path"
        mountpoint.mkdir()
        self.assertEqual(other.stat().st_dev, mountpoint.stat().st_dev)
        subprocess.run(["mount", "--bind", other, mountpoint], check=True)
        try:
            for inputs in (self.input(), self.input(path="local-path")):
                failed = n.wait(n.submit("volume.usage", inputs))
                self.assertEqual(failed["lifecycle"], "Failed", failed)
                self.assertEqual(failed["result"]["artifact_refs"], [], failed)
        finally:
            subprocess.run(["umount", mountpoint], check=True)
        self.assertEqual(n.perform("volume.usage", self.input())["files"], 0)

    def test_entry_limit_fails_whole_scan_and_allows_a_smaller_subtree(self):
        n = self.node
        limited = self.root / "limited"
        limited.mkdir()
        for number in range(4095):
            (limited / str(number)).touch()
        scan = n.wait(n.submit("volume.usage", self.input()))
        self.assertEqual(scan["conditions"][0]["type"], "QuotaExceeded", scan)
        narrow = n.perform("volume.usage", self.input(path="limited"))
        self.assertEqual((narrow["files"], narrow["directories"]), (4095, 1))
