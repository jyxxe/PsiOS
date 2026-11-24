"""Verify stored recovery sets without allocating a volume or granting access to its contents."""
from contextlib import ExitStack
import hashlib
import json
import os
import signal
import time
import unittest
from support import Failure, Node, target


class BackupTests(unittest.TestCase):
    def setUp(self):
        self.n = Node("target/debug")
        self.addCleanup(self.n.close)
        volume = self.n.perform("volume.create", {"name": "records"})
        volume = self.n.perform("volume.mkdir", dict(target=target(volume), path="nested"))["volume_ref"]
        self.volume = self.n.perform("volume.append", dict(target=target(volume), path="nested/binary",
                                    offset=0, data_base64="AAEC/w=="))["volume_ref"]
        self.backup = self.n.perform("backup.create", target(self.volume))
        self.grant = self.issue("local-owner", self.backup["resource_id"])
        self.folder = self.n.state / "backups" / self.backup["resource_id"]

    def issue(self, actor, scope):
        grant = self.n.call("Grant.Issue", dict(principal_id=actor, tool_targets=[scope],
                            actions=["task.submit", "task.read", "backup.verify"], units=50, ttl_seconds=300))
        return grant["delegation_ref"]["resource_id"]

    def submit_check(self, delegation, token=None):
        submit = self.n.prepare("backup.verify", target(self.backup), delegation=delegation, token=token)
        return self.n.call("Task.Submit", submit, token)["task_id"]

    def check(self):
        return self.n.wait(self.submit_check(self.grant))

    def test_verification_obeys_scope_and_needs_no_pool_allocation(self):
        n = self.n
        foreign = n.call("Agent.Register", {"name": "auditor"})
        for delegation, token in [("owner-delegation", n.owner),
                                  (self.issue(foreign["principal_id"], self.backup["resource_id"]), foreign["token"]),
                                  (self.issue("local-owner", self.volume["resource_id"]), n.owner)]:
            with self.assertRaises(Failure) as failure:
                self.submit_check(delegation, token)
            self.assertEqual(failure.exception.code, "PermissionDenied")
        pool = n.call("StoragePool.Get", {"id": "local"})["pool_ref"]
        n.call("StoragePool.Configure", dict(**target(pool), allocation_enabled=False))
        volumes = set((n.state / "volumes").iterdir())
        digest = n.get(self.backup["resource_id"])["value"]["manifest_digest"]
        job = self.check()
        self.assertEqual(job["lifecycle"], "Succeeded", job)
        report = n.call("Artifact.Read", {"reference": job["result"]["artifact_refs"][0]})
        self.assertEqual((report["backup_ref"], report["entries"], report["bytes"], report["manifest_digest"]),
                         (self.backup, 2, "4", digest))
        self.assertEqual(n.get(self.backup["resource_id"])["reference"], self.backup)
        self.assertEqual(set((n.state / "volumes").iterdir()), volumes)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION") == "1", "requires a bounded filesystem")
    def test_restore_preflights_shared_filesystem_without_publishing_partial_volume(self):
        n = self.n
        volumes = n.state / "volumes"
        original = volumes / self.volume["resource_id"] / "data" / "record"
        with original.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, 6 * 1024 * 1024)
        for index in range(18):
            (original.parent / f"empty-{index}").touch()
        saved = n.perform("backup.create", target(self.volume))
        cfg = json.loads(n.config.read_text())
        n.stop()
        n.config.write_text(json.dumps(dict(cfg, authority_reserve={"state_bytes": 4 * 1024 * 1024})))
        n.start()
        snapshot = set(volumes.iterdir())
        filler = n.base / "allocation"
        stats = os.statvfs(n.state)
        with filler.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, stats.f_bavail * stats.f_frsize - 6 * 1024 * 1024)
        self.assertFalse(n.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        refused = n.wait(n.submit("backup.restore", target(saved)), autoapprove=True)
        self.assertEqual((refused["lifecycle"], refused["conditions"][0]["type"]), ("Failed", "QuotaExceeded"), refused)
        self.assertIn("destination blocks", refused["conditions"][0]["message"])
        self.assertEqual(set(volumes.iterdir()), snapshot)
        self.assertEqual(n.get(saved["resource_id"])["reference"], saved)
        filler.unlink()

        inodes = n.base / "inodes"
        inodes.mkdir()
        for index in range(os.statvfs(inodes).f_favail - 10):
            (inodes / str(index)).touch()
        refused = n.wait(n.submit("backup.restore", target(saved)), autoapprove=True)
        self.assertEqual((refused["lifecycle"], refused["conditions"][0]["type"]), ("Failed", "QuotaExceeded"), refused)
        self.assertIn("destination inodes", refused["conditions"][0]["message"])
        self.assertEqual(set(volumes.iterdir()), snapshot)
        for entry in inodes.iterdir():
            entry.unlink()
        restored = n.perform("backup.restore", target(saved))
        restored_path = volumes / restored["resource_id"] / "data" / "record"
        with original.open("rb") as source, restored_path.open("rb") as output:
            self.assertEqual(hashlib.file_digest(output, "sha256").digest(), hashlib.file_digest(source, "sha256").digest())

    def test_manifest_and_blob_faults_release_the_backup_and_preserve_source(self):
        blob = next(self.folder.glob("blob-*"))
        manifest = self.folder / "manifest.json"
        initial = blob.read_bytes()
        original = manifest.read_bytes()
        for path, corrupted in [(blob, b"xxxx"), (blob, b""), (manifest, original + b" ")]:
            saved = path.read_bytes()
            path.write_bytes(corrupted)
            job = self.check()
            self.assertEqual((job["lifecycle"], job["conditions"][0]["type"]), ("Failed", "PreconditionFailed"), job)
            path.write_bytes(saved)
        blob.unlink()
        os.mkfifo(blob)
        self.assertEqual(self.check()["lifecycle"], "Failed")
        blob.unlink()
        blob.write_bytes(initial)
        manifest.unlink()
        manifest.symlink_to(blob)
        self.assertEqual(self.check()["lifecycle"], "Failed")
        manifest.unlink()
        with manifest.open("wb") as f:
            f.truncate(73 * 1024 * 1024)
        self.assertEqual(self.check()["conditions"][0]["type"], "QuotaExceeded")
        manifest.write_bytes(original)
        self.n.stop()
        self.n.start()
        self.assertEqual(self.check()["lifecycle"], "Succeeded")
        restored = self.n.perform("backup.restore", target(self.backup))
        data = self.n.perform("volume.read_chunk", dict(target=target(restored), path="nested/binary", offset=0, limit=4))
        self.assertEqual(data["data_base64"], "AAEC/w==")


    def test_changed_offline_source_aborts_publication_and_releases_hold(self):
        n = self.n
        volume = n.perform("volume.create", {"name": "offline-records"})
        root = n.state / "volumes" / volume["resource_id"] / "data"
        early = root / "00-record"
        early.write_bytes(b"old")
        stamp = early.stat()
        padding = root / "99-padding"
        with padding.open("wb") as f:
            f.truncate(32 * 1024 * 1024)
        backups = n.state / "backups"
        initial = set(backups.iterdir())
        task = n.submit("backup.create", target(volume))
        # The second blob appears only after the first was copied and fsynced.
        deadline = time.monotonic() + 15
        while not any(backups.glob(".tmp*/blob-1")):
            if time.monotonic() >= deadline:
                self.fail("did not observe the second blob in staging")
            time.sleep(0.001)
        n.process.send_signal(signal.SIGSTOP)
        try:
            _, status = os.waitpid(n.process.pid, os.WUNTRACED)
            self.assertTrue(os.WIFSTOPPED(status))
            self.assertTrue(any(backups.glob(".tmp*/blob-1")))
            early.write_bytes(b"new")
            os.utime(early, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        finally:
            n.process.send_signal(signal.SIGCONT)
        failed = n.wait(task)
        self.assertEqual(failed["lifecycle"], "Failed", failed["lifecycle"])
        self.assertEqual(failed["conditions"][0]["type"], "Conflict", failed)
        self.assertEqual(set(backups.iterdir()), initial)
        padding.unlink()
        saved = n.perform("backup.create", target(volume))
        restored = n.perform("backup.restore", target(saved))
        result = n.perform("volume.read", dict(**target(restored), name="00-record"))
        self.assertEqual(result["content"], "new")

    def test_restore_file_directory_and_root_modification_times(self):
        n = self.n
        volume = n.perform("volume.create", {"name": "timeline"})
        root = n.state / "volumes" / volume["resource_id"] / "data"
        (root / "project").mkdir()
        (root / "project/log").write_bytes(b"original")
        stamps = {"project/log": -333_333_333, "project": 1_600_000_000_123_000_456, ".": 1_600_000_000_000_000_001}
        for path, ns in stamps.items():
            os.utime(root / path, ns=(ns, ns))
            self.assertEqual((root / path).stat().st_mtime_ns, ns)
        backup = n.perform("backup.create", target(volume))
        (root / "project/log").write_bytes(b"later")
        n.stop(signal.SIGKILL)
        n.start()
        query = n.prepare("backup.verify", target(backup), delegation=self.issue("local-owner", backup["resource_id"]))
        self.assertEqual(n.wait(n.call("Task.Submit", query)["task_id"])["lifecycle"], "Succeeded")
        restored = n.perform("backup.restore", target(backup))
        output = n.state / "volumes" / restored["resource_id"] / "data"
        self.assertEqual((output / "project/log").read_bytes(), b"original")
        for path, ns in stamps.items():
            self.assertEqual((output / path).stat().st_mtime_ns, ns, path)

    def test_restore_deep_paths_created_by_a_linux_workload(self):
        n = self.n
        volume = n.perform("volume.create", {"name": "deep"})
        parts = [f"level-{i:02d}-" + "a" * 200 for i in range(22)]

        def descend(stack, volume_ref, create):
            root = n.state / "volumes" / volume_ref["resource_id"] / "data"
            fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            stack.callback(os.close, fd)
            for name in parts:
                if create:
                    os.mkdir(name, 0o700, dir_fd=fd)
                fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
                stack.callback(os.close, fd)
            return fd

        with ExitStack() as stack:
            leaf = descend(stack, volume, True)
            fd = os.open("payload", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600, dir_fd=leaf)
            with os.fdopen(fd, "wb") as f:
                f.write(b"preserved")
            os.fchmod(leaf, 0o500)
        try:
            backup = n.perform("backup.create", target(volume))
        finally:
            with ExitStack() as stack:
                os.fchmod(descend(stack, volume, False), 0o700)
        restored = n.perform("backup.restore", target(backup))
        with ExitStack() as stack:
            leaf = descend(stack, restored, False)
            stack.callback(os.fchmod, leaf, 0o700)
            self.assertEqual(os.fstat(leaf).st_mode & 0o777, 0o500)
            with os.fdopen(os.open("payload", os.O_RDONLY, dir_fd=leaf), "rb") as f:
                self.assertEqual(f.read(), b"preserved")
