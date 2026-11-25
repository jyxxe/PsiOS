"""Authority inode admission and managed volume accounting on a real filesystem."""
import base64
import json
import os
import unittest

from support import Failure, Node, target


class StorageReserveTests(unittest.TestCase):
    @unittest.skipUnless(os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION") == "1",
                         "requires the bounded Linux filesystem fixture")
    def test_approved_replacement_reserves_peak_blocks_before_removing_old_contents(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        original = json.loads(node.config.read_text())
        volume = node.perform("volume.create", {"name": "peak-replacement"})
        volume = node.perform("volume.write", {**target(volume), "name": "record",
                                              "content": "original"})["volume_ref"]
        path = node.state / "volumes" / volume["resource_id"] / "data" / "record"
        request = {**target(volume), "name": "record", "content": "x" * (64 * 1024)}
        approval = node.submit("volume.write", request)
        self.assertEqual(node.wait(approval)["lifecycle"], "WaitingApproval")
        node.stop()

        reserve = 4 * 1024 * 1024
        node.config.write_text(json.dumps({**original, "authority_reserve": {"state_bytes": reserve}}))
        node.start()
        stats = os.statvfs(node.state)
        self.assertLessEqual(stats.f_blocks * stats.f_frsize, 32 * 1024 * 1024)
        growth = stats.f_bavail * stats.f_frsize - reserve - stats.f_frsize
        self.assertGreater(growth, 0)
        filler = node.base / "test-allocated-blocks"
        with filler.open("xb") as file:
            os.posix_fallocate(file.fileno(), 0, growth)
        self.assertFalse(node.call("Node.Get", {})["authority_storage"]["state"]["below_reserve"])
        node.approve(approval)
        result = node.wait(approval)
        self.assertEqual((result["lifecycle"], result["outcome_knowledge"], result["conditions"][0]["type"]),
                         ("Failed", "Known", "QuotaExceeded"), result)
        self.assertIn("destination blocks", result["conditions"][0]["message"])
        self.assertEqual(path.read_text(), "original")
        self.assertEqual(sorted(item.name for item in path.parent.iterdir()), ["record"])
        self.assertEqual(node.get(volume["resource_id"])["reference"], volume)

        filler.unlink()
        node.perform("volume.write", request)
        self.assertEqual(path.stat().st_size, 64 * 1024)

    def test_inode_pressure_blocks_admission_and_growth_but_preserves_queued_reads_and_removal(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        if os.statvfs(node.state).f_files == 0:
            self.skipTest("filesystem does not expose an inode capacity")
        original = json.loads(node.config.read_text())
        bounded = os.getenv("PSIOS_TEST_CAPACITY_ALLOCATION") == "1"
        floor = 24 if bounded else os.statvfs(node.state).f_files + 1

        def scheduling(mode):
            current = node.call("Node.Get", {})["reference"]
            node.call("Node.Configure", {"expected_resource_version": current["version"], "scheduling": mode})

        volume = node.perform("volume.create", {"name": "reserve-test"})
        for name, contents in (("record", "before"), ("removable", "old")):
            volume = node.perform("volume.write", {**target(volume), "name": name,
                                                    "content": contents})["volume_ref"]
        directory = node.state / "volumes" / volume["resource_id"] / "data"
        self.assertEqual(node.state.stat().st_dev, node.witness.stat().st_dev)
        self.assertEqual(directory.stat().st_dev, node.witness.stat().st_dev)
        previous = sorted(node.state.joinpath("volumes").iterdir())

        scheduling("Cordoned")
        queued_read = node.submit("volume.read", {**target(volume), "name": "record"})
        queued_create = node.submit("volume.create", {"name": "blocked-volume"})
        changes = [
            ("volume.write", {**target(volume), "name": "record", "content": "x"}),
            ("volume.append", {"target": target(volume), "path": "record", "offset": 6,
                               "data_base64": base64.b64encode(b"x").decode()}),
            ("volume.append", {"target": target(volume), "path": "new", "offset": 0,
                               "data_base64": base64.b64encode(b"new").decode()}),
            ("volume.mkdir", {"target": target(volume), "path": "new-directory"}),
        ]
        approved = []
        for tool, request in changes:
            task = node.submit(tool, request)
            self.assertEqual(node.wait(task)["lifecycle"], "WaitingApproval")
            node.approve(task)
            approved.append(task)
        cleanup = node.submit("volume.remove_entry", {"target": target(volume), "path": "removable"})
        self.assertEqual(node.wait(cleanup)["lifecycle"], "WaitingApproval")
        node.approve(cleanup)
        fresh = node.prepare("volume.create", {"name": "same-request-after-pressure"})

        node.stop()
        state_floor = floor - 8 if bounded else 0
        reserve = {"state_inodes": state_floor, "witness_inodes": floor}
        node.config.write_text(json.dumps({**original, "authority_reserve": reserve}))
        node.start()
        blockers = node.base / "test-allocated-inodes"
        if bounded:
            blockers.mkdir()
            inodes = os.statvfs(blockers)
            self.assertGreater(inodes.f_favail, floor)
            self.assertLessEqual(inodes.f_files, 512)
            for index in range(inodes.f_favail - floor + 3):
                (blockers / str(index)).touch()

        status = node.call("Node.Get", {})["authority_storage"]
        self.assertEqual(status["witness"]["reserve_inodes"], str(floor))
        self.assertEqual(status["witness"]["inode_accounting"], "Observed")
        self.assertTrue(status["witness"]["below_reserve"])
        self.assertFalse(status["state"]["below_reserve"])
        self.assertEqual(status["state"]["reserve_inodes"], str(state_floor))
        if bounded:
            self.assertGreater(int(status["witness"]["available_inodes"]), 16)

        with self.assertRaises(Failure) as refused:
            node.call("Task.Submit", fresh)
        details = refused.exception.args[0]
        self.assertEqual((details["code"], details["retry_condition"]),
                         ("QuotaExceeded", "SameRequestAfterBackoff"))
        self.assertIn("witness filesystem below configured free inode reserve", details["message"])

        scheduling("Enabled")
        read = node.wait(queued_read)
        self.assertEqual(read["lifecycle"], "Succeeded", read)
        self.assertEqual(node.call("Artifact.Read", {"reference": read["result"]["artifact_refs"][0]})["content"],
                         "before")
        for task_id in [queued_create, *approved]:
            task = node.wait(task_id)
            self.assertEqual((task["lifecycle"], task["outcome_knowledge"], task["conditions"][0]["type"]),
                             ("Failed", "Known", "QuotaExceeded"), task)
            self.assertIn("destination inodes", task["conditions"][0]["message"])
        self.assertEqual(node.wait(cleanup)["lifecycle"], "Succeeded")
        self.assertEqual((directory / "record").read_text(), "before")
        self.assertEqual(sorted(path.name for path in directory.iterdir()), ["record"])
        self.assertEqual(sorted(node.state.joinpath("volumes").iterdir()), previous)

        if bounded:
            for entry in blockers.iterdir():
                entry.unlink()
            blockers.rmdir()
        else:
            node.stop()
            node.config.write_text(json.dumps(original))
            node.start()
        self.assertFalse(node.call("Node.Get", {})["authority_storage"]["witness"]["below_reserve"])
        admitted = node.call("Task.Submit", fresh)
        self.assertFalse(admitted["deduplicated"])
        self.assertEqual(node.wait(admitted["task_id"])["lifecycle"], "Succeeded")
        volume = node.get(volume["resource_id"])["reference"]
        node.perform("volume.append", {"target": target(volume), "path": "record", "offset": 6,
                                       "data_base64": base64.b64encode(b"!").decode()})
        self.assertEqual((directory / "record").read_text(), "before!")


if __name__ == "__main__":
    unittest.main()
