"""Verify detached single-copy volume clones through the real Unix Core API."""

from contextlib import closing
import base64
import errno
import hashlib
import json
import os
import signal
import sqlite3
import stat
import subprocess
import time
import unittest

from support import Failure, Node, target


CLONE = "volume.clone"
MIB = 1024 * 1024


class VolumeCloneTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.volumes = self.n.state / "volumes"
        self.backups = self.n.state / "backups"

    def data(self, ref):
        return self.volumes / ref["resource_id"] / "data"

    def grant(self, targets, principal="local-owner"):
        return self.n.call("Grant.Issue", {
            "principal_id": principal, "actions": ["task.submit", "task.read", CLONE],
            "tool_targets": targets, "units": 12, "ttl_seconds": 300,
        })["delegation_ref"]["resource_id"]

    def submit(self, source, name, delegation, token=None):
        request = self.n.prepare(CLONE, {"target": target(source), "name": name},
                                 delegation=delegation, token=token)
        acceptance = self.n.call("Task.Submit", request, token)
        return request, acceptance["task_id"]

    def result(self, task_id):
        task = self.n.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        result, = task["result"]["artifact_refs"]
        return self.n.call("Artifact.Read", {"reference": result})

    def failure(self, task_id, code, knowledge=None):
        task = self.n.wait(task_id, autoapprove=True)
        self.assertEqual((task["lifecycle"], task["conditions"][0]["type"]), ("Failed", code), task)
        self.assertFalse(task["result"]["artifact_refs"], task)
        if knowledge is not None:
            self.assertEqual(task["outcome_knowledge"], knowledge, task)
        return task

    def append(self, source, path, contents, offset=0):
        updated = self.n.perform("volume.append", {
            "target": target(source), "path": path, "offset": offset,
            "data_base64": base64.b64encode(contents).decode("ascii"),
        })
        return updated["volume_ref"]

    def verify(self):
        self.n.stop()
        result = json.loads(subprocess.check_output([*self.n.daemon, "verify"], timeout=25))
        self.assertEqual(result["sqlite_integrity"], "Verified", result)
        self.assertFalse(result["migration_required"], result)

    def assert_detached(self, source, clone, name):
        self.assertNotEqual(source["resource_id"], clone["resource_id"])
        original = self.n.get(source["resource_id"])
        created = self.n.get(clone["resource_id"])
        self.assertEqual(created["reference"], clone)
        spec = created["resource"]["spec"]
        self.assertEqual((spec["name"], spec["cloned_from"], spec["storage_restore_phase"]),
                         (name, source, "AwaitingActivation"), spec)
        self.assertTrue(original["value"]["data_incarnation"] and created["value"]["data_incarnation"])
        self.assertNotEqual(original["value"]["data_incarnation"], created["value"]["data_incarnation"])
        return spec["restore_proof"]

    def test_sparse_layout_survives_capture_restore_clone_and_detached_intake(self):
        n = self.n
        source = n.perform("volume.create", {"name": "sparse"})
        tree = self.data(source)
        length = 4 * MIB + 157
        mixed = tree / "mixed"
        with mixed.open("xb") as f:
            f.seek(MIB + 17)
            f.write(b"payload")
            f.seek(2 * MIB)
            f.write(bytes(64 * 1024))  # allocated zero data between two holes
            f.truncate(length)
            f.flush()
            os.fsync(f.fileno())
        with (tree / "holes").open("xb") as f:
            f.truncate(length)
        (tree / "empty").touch()

        def layout(path):
            with path.open("rb") as f:
                return (os.lseek(f.fileno(), 0, os.SEEK_DATA),
                        os.lseek(f.fileno(), 2 * MIB, os.SEEK_DATA),
                        os.lseek(f.fileno(), 2 * MIB, os.SEEK_HOLE))

        try:
            source_layout = layout(mixed)
        except OSError as error:
            if error.errno not in (errno.EINVAL, errno.ENOTSUP, errno.ENXIO):
                raise
            self.skipTest("source filesystem does not report sparse extents")
        if not (source_layout[0] > 0 and source_layout[1] == 2 * MIB
                and source_layout[2] < 3 * MIB):
            self.skipTest("source filesystem does not distinguish the fixture's data and holes")

        saved = n.perform("backup.create", target(source))
        archive = self.backups / saved["resource_id"]
        manifest = json.loads((archive / "manifest.json").read_bytes())
        blobs = {entry["path"]: f"blob-{index}" for index, entry in enumerate(manifest["entries"])}
        for entry in manifest["entries"]:
            digest = hashlib.sha256((tree / entry["path"]).read_bytes()).hexdigest()
            self.assertEqual(entry["digest"], f"sha256:{digest}")

        def assert_layout(files):
            for name, path in files.items():
                with self.subTest(path=path):
                    self.assertEqual(path.read_bytes(), (tree / name).read_bytes())
                    self.assertLessEqual(path.stat().st_blocks * 512, 256 * 1024)
                    if name == "mixed":
                        self.assertEqual(layout(path), source_layout)
                    if name == "holes":
                        with path.open("rb") as f, self.assertRaises(OSError) as hole:
                            os.lseek(f.fileno(), 0, os.SEEK_DATA)
                        self.assertEqual(hole.exception.errno, errno.ENXIO)

        assert_layout({name: archive / blob for name, blob in blobs.items()})
        restored = n.perform("backup.restore", target(saved))
        grant = self.grant([source["resource_id"], "local"])
        _, pending = self.submit(source, "cloned-sparse", grant)
        self.assertEqual(n.wait(pending)["lifecycle"], "WaitingApproval")
        n.approve(pending)
        clone = self.result(pending)
        for volume in (restored, clone):
            assert_layout({name: self.data(volume) / name for name in blobs})

        receiver = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(receiver.close)
        receiver.stop()
        digest = n.get(saved["resource_id"])["value"]["manifest_digest"]
        subprocess.run([*receiver.daemon, "import-backup", "--directory", archive,
                        "--manifest-digest", digest], check=True, capture_output=True, timeout=30)
        imported = receiver.state / "backups" / saved["resource_id"]
        assert_layout({name: imported / blob for name, blob in blobs.items()})

        # Older backups can store exactly the same digest in fully allocated blobs.
        blob = archive / blobs["mixed"]
        blob.write_bytes(mixed.read_bytes())
        self.assertGreaterEqual(blob.stat().st_blocks * 512, length)
        legacy = n.perform("backup.restore", target(saved))
        self.assertEqual((self.data(legacy) / "mixed").read_bytes(), mixed.read_bytes())

    def test_exact_scoped_approval_stale_source_and_independent_activation(self):
        n = self.n
        source = n.perform("volume.create", {"name": "clone-source"})
        source = n.perform("volume.mkdir", {"target": target(source), "path": "records"})["volume_ref"]
        payload = b"\x00\x01\x80\xff\noriginal"
        source = self.append(source, "records/binary", payload)
        original_root = self.data(source)
        (original_root / "records/empty").mkdir()
        modes = {".": 0o750, "records": 0o700, "records/empty": 0o710, "records/binary": 0o640}
        base_time = 1_700_000_193_123_456_789
        for index, (path, mode) in enumerate(reversed(list(modes.items()))):
            (original_root / path).chmod(mode)
            os.utime(original_root / path, ns=(base_time + index, base_time + index))

        for unauthorized in ("owner-delegation", self.grant([source["resource_id"]]), self.grant(["local"])):
            with self.assertRaises(Failure) as denied:
                self.submit(source, "scope-refused", unauthorized)
            self.assertEqual(denied.exception.code, "PermissionDenied", denied.exception)
        foreign = n.call("Agent.Register", {"name": "clone-outsider"})
        with self.assertRaises(Failure) as denied:
            outside = self.grant([source["resource_id"], "local"], foreign["principal_id"])
            self.submit(source, "other-principal", outside, foreign["token"])
        self.assertEqual(denied.exception.code, "PermissionDenied", denied.exception)

        delegation = self.grant([source["resource_id"], "local"])
        initial_volumes = set(self.volumes.iterdir())
        initial_backups = set(self.backups.iterdir())
        _, stale = self.submit(source, "stale-source", delegation)
        self.assertEqual(n.wait(stale)["lifecycle"], "WaitingApproval")
        n.perform("volume.write", {**target(source), "name": "latest", "content": "published before the next approval"})
        fresh_source = n.get(source["resource_id"])["reference"]
        self.assertNotEqual(fresh_source, source)
        self.failure(stale, "Conflict", "NotStarted")
        self.assertEqual(set(self.volumes.iterdir()), initial_volumes)
        self.assertEqual(set(self.backups.iterdir()), initial_backups)
        os.utime(original_root, ns=(base_time + 3, base_time + 3))
        source_times = {path: (original_root / path).stat().st_mtime_ns for path in modes}

        request, pending = self.submit(fresh_source, "independent-copy", delegation)
        paused = n.wait(pending)
        self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("WaitingApproval", "NotStarted"), paused)
        approval = n.get(paused["conditions"][0]["message"])["value"]
        plan = n.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
        self.assertIn(fresh_source, [condition["resource_ref"] for condition in plan["preconditions"]], plan)
        self.assertEqual(plan["allowed_effects"][0]["action"], CLONE, plan)
        self.assertEqual(set(self.volumes.iterdir()), initial_volumes)
        n.approve(pending)
        clone = self.result(pending)
        proof = self.assert_detached(fresh_source, clone, "independent-copy")
        self.assertEqual(proof["root_mode_unix_v1"], modes["."])
        self.assertTrue(proof["includes_mtime"], proof)
        target_root = self.data(clone)
        target_file = target_root / "records/binary"
        self.assertEqual(set(self.volumes.iterdir()), initial_volumes | {target_root.parent})
        self.assertEqual(set(self.backups.iterdir()), initial_backups)
        self.assertEqual(target_file.read_bytes(), payload)
        self.assertEqual((target_root / "latest").read_bytes(), b"published before the next approval")
        for path, mode in modes.items():
            with self.subTest(path=path):
                original = (original_root / path).stat()
                copied = (target_root / path).stat()
                self.assertEqual(stat.S_IMODE(copied.st_mode), mode)
                self.assertEqual(copied.st_mtime_ns, source_times[path])
                self.assertNotEqual((original.st_dev, original.st_ino), (copied.st_dev, copied.st_ino))
        self.assertEqual(list((target_root / "records/empty").iterdir()), [])

        blocked = n.submit("volume.write", {**target(clone), "name": "too-early", "content": "refused"})
        self.failure(blocked, "PreconditionFailed")
        self.assertFalse((target_root / "too-early").exists())
        n.stop(signal.SIGKILL)
        target_file.write_bytes(b"x" * len(payload))
        stamp = source_times["records/binary"]
        os.utime(target_file, ns=(stamp, stamp))
        n.start()
        self.failure(n.submit("volume.activate", target(clone)), "PreconditionFailed")
        self.assertEqual(n.get(clone["resource_id"])["reference"], clone)
        target_file.write_bytes(payload)
        os.utime(target_file, ns=(stamp, stamp))

        later_source = self.append(fresh_source, "records/binary", b"later", offset=len(payload))
        self.assertNotEqual(later_source, fresh_source)
        replay = n.call("Task.Submit", request)
        self.assertTrue(replay["deduplicated"], replay)
        self.assertEqual(replay["task_id"], pending)
        self.assertEqual(self.result(pending), clone)
        self.assertEqual(target_file.read_bytes(), payload)
        activated = n.perform("volume.activate", target(clone))
        read = n.perform("volume.read_chunk", {"target": target(activated), "path": "records/binary",
                         "offset": 0, "limit": len(payload)})
        self.assertEqual(base64.b64decode(read["data_base64"]), payload)
        self.assertEqual((original_root / "records/binary").read_bytes(), payload + b"later")
        self.assertEqual(n.get(clone["resource_id"])["resource"]["spec"]["cloned_from"], fresh_source)
        self.verify()

    def test_unresolved_exclusive_source_hold_survives_restart_and_blocks_clone(self):
        n = self.n
        source = n.perform("volume.create", {"name": "held-clone-source"})
        root = self.data(source)
        (root / "remove").write_bytes(b"in custody")
        (root / "keep").write_bytes(b"survives both operations")
        delegation = self.grant([source["resource_id"], "local"])
        with closing(sqlite3.connect(n.state / "state.db", timeout=8)) as db, db:
            db.execute(f"""CREATE TRIGGER lose_source_update BEFORE INSERT ON versions
                WHEN NEW.kind='Volume' AND NEW.id='{source['resource_id']}'
                BEGIN SELECT RAISE(ABORT, 'fixture withheld source retirement receipt'); END""")
        try:
            original_task = n.submit("volume.remove_entry", {"target": target(source), "path": "remove"})
            paused = n.wait(original_task, autoapprove=True)
            self.assertEqual((paused["lifecycle"], paused["outcome_knowledge"]), ("Paused", "Unknown"), paused)
            original_op, = paused["operation_refs"]
            self.assertFalse((root / "remove").exists())
            n.stop(signal.SIGKILL)
        finally:
            with closing(sqlite3.connect(n.state / "state.db", timeout=8)) as db, db:
                db.execute("DROP TRIGGER lose_source_update")
        n.start()
        snapshot = set(self.volumes.iterdir())
        with closing(sqlite3.connect(n.state / "state.db")) as db:
            hold = db.execute("SELECT holder FROM holds WHERE volume=?", (source["resource_id"],)).fetchone()
        self.assertEqual(hold, (original_op["resource_id"],))
        _, refused = self.submit(source, "while-held", delegation)
        self.failure(refused, "Conflict", "NotStarted")
        self.assertEqual(set(self.volumes.iterdir()), snapshot)
        self.assertEqual((root / "keep").read_bytes(), b"survives both operations")
        self.assertEqual(n.wait(original_task)["operation_refs"], [original_op])
        done = n.call("Operation.Reconcile", target(original_op))
        self.assertEqual(done["outcome_knowledge"], "Known", done)
        updated = n.get(source["resource_id"])["reference"]
        _, allowed = self.submit(updated, "after-release", delegation)
        n.approve(allowed)
        clone = self.result(allowed)
        self.assert_detached(updated, clone, "after-release")
        self.assertEqual((self.data(clone) / "keep").read_bytes(), b"survives both operations")
        self.assertFalse((self.data(clone) / "remove").exists())
        self.verify()

    def test_root_only_extended_attributes_block_clone_and_activation(self):
        source = self.n.perform("volume.create", {"name": "clone-root-xattr"})
        before = set(self.volumes.iterdir())
        root = self.data(source)
        attribute = "user.psios-clone-proof"
        try:
            os.setxattr(root, attribute, b"source root attribute")
        except OSError as exc:
            if exc.errno in (errno.ENOTSUP, errno.EOPNOTSUPP):
                self.skipTest("local filesystem does not support user extended attributes")
            raise
        delegation = self.grant([source["resource_id"], "local"])
        try:
            _, rejected = self.submit(source, "unexpected-root-metadata", delegation)
            self.failure(rejected, "UnsupportedCapability")
            self.assertEqual(os.getxattr(root, attribute), b"source root attribute")
            self.assertEqual(set(self.volumes.iterdir()), before)
        finally:
            os.removexattr(root, attribute)

        _, pending = self.submit(source, "root-metadata-verified", delegation)
        self.n.approve(pending)
        clone = self.result(pending)
        output = self.data(clone)
        stamp = output.stat()
        os.setxattr(output, attribute, b"added after publication")
        self.assertEqual(output.stat().st_mtime_ns, stamp.st_mtime_ns)
        self.failure(self.n.submit("volume.activate", target(clone)), "PreconditionFailed")
        self.assertEqual(self.n.get(clone["resource_id"])["reference"], clone)
        self.assertEqual(os.getxattr(output, attribute), b"added after publication")
        os.removexattr(output, attribute)
        self.n.perform("volume.activate", target(clone))
        self.verify()

    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION") == "1", "requires a bounded filesystem")
    def test_bounded_pool_fits_single_copy_when_backup_and_restore_need_two(self):
        n = self.n
        source = n.perform("volume.create", {"name": "space-source"})
        path = self.data(source) / "payload"
        payload_bytes = 6 * MIB
        reserve_bytes = 4 * MIB
        with path.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, payload_bytes)
            file.write(b"an actual source on the bounded backing filesystem")
        n.stop()
        config = json.loads(n.config.read_text())
        config["authority_reserve"] = {"state_bytes": reserve_bytes}
        n.config.write_text(json.dumps(config))
        n.start()

        stats = os.statvfs(n.state)
        self.assertLessEqual(stats.f_blocks * stats.f_frsize, 64 * MIB)
        filler = n.base / "clone-space-filler"
        free = stats.f_bavail * stats.f_frsize
        headroom = 13 * MIB
        self.assertGreater(free, headroom)
        with filler.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, free - headroom)
        initial_volumes = set(self.volumes.iterdir())

        backup = n.perform("backup.create", target(source))
        self.failure(n.submit("backup.restore", target(backup)), "QuotaExceeded")
        self.assertEqual(set(self.volumes.iterdir()), initial_volumes)
        deletion = n.wait(n.submit("backup.delete", target(backup)), autoapprove=True)
        self.assertEqual((deletion["lifecycle"], deletion["outcome_knowledge"]), ("Succeeded", "Known"), deletion)
        op, = deletion["operation_refs"]
        retired = self.backups / f".retired-{op['resource_id']}"
        retirement_id = f"storage-retirement-{op['resource_id']}"
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            retirement = n.get(retirement_id)["value"]
            if retirement["lifecycle"] == "Reclaimed":
                break
            time.sleep(0.04)
        else:
            self.fail(f"backup retirement did not settle: {retirement}")
        self.assertFalse(retired.exists())
        self.assertFalse((self.backups / backup["resource_id"]).exists())

        stats = os.statvfs(n.state)
        available = stats.f_bavail * stats.f_frsize
        self.assertGreaterEqual(available, reserve_bytes + payload_bytes)
        self.assertLess(available, reserve_bytes + 2 * payload_bytes)

        remaining_backups = set(self.backups.iterdir())
        _, pending = self.submit(source, "one-copy", self.grant([source["resource_id"], "local"]))
        n.approve(pending)
        clone = self.result(pending)
        self.assert_detached(source, clone, "one-copy")
        self.assertEqual(set(self.backups.iterdir()), remaining_backups)
        self.assertEqual(set(self.volumes.iterdir()), initial_volumes | {self.data(clone).parent})
        with path.open("rb") as original, (self.data(clone) / "payload").open("rb") as copy:
            self.assertEqual(hashlib.file_digest(copy, "sha256").digest(),
                             hashlib.file_digest(original, "sha256").digest())
        filler.unlink()
        self.verify()
