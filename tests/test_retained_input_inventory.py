"""Inventory legacy retained inputs in a stopped, witnessed SQLite authority."""

from datetime import datetime, timedelta, timezone
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import unittest

from support import Node, key, target


BINARIES = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
LEGACY_BINARIES = os.getenv("PSIOS_LEGACY_INPUT_BIN_DIR")
MIB = 1024 * 1024


def encoded_bytes(value):
    return len(json.dumps(value, sort_keys=True, separators=(",", ":")).encode())


def stamp(path):
    info = path.stat()
    with path.open("rb") as file:
        digest = hashlib.file_digest(file, "sha256").hexdigest()
    return info.st_dev, info.st_ino, info.st_mode, info.st_size, info.st_mtime_ns, digest


def fake_ref(digit="0"):
    return {"resource_id": "input-" + digit * 64, "version": "r1", "digest": "sha256:" + digit * 64}


class RetainedInputInventoryTests(unittest.TestCase):
    def start(self, binaries):
        self.node = Node(binaries)
        self.addCleanup(self.node.close)
        self.scratch = self.node.base / "inventory-scratch"
        self.scratch.mkdir(mode=0o700)
        self.references = self.node.base / "references.json"
        self.references.touch(mode=0o600)
        return self.node

    def inventory(self, refs, *, max_copy=32, max_scan=32, scratch=None):
        self.references.write_text(json.dumps(refs))
        result = subprocess.run([
            str(Path(BINARIES, "psiosd").absolute()), "--config", self.node.config, "inspect-retained-inputs",
            "--references-file", self.references, "--scratch-dir", scratch or self.scratch,
            "--max-copy-mib", str(max_copy), "--max-scan-mib", str(max_scan),
        ], capture_output=True, timeout=40)
        self.assertEqual(list(self.scratch.iterdir()), [], result.stderr.decode(errors="replace"))
        return result

    def refused(self, code, result):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertFalse(result.stdout, result)
        self.assertIn(code.encode(), result.stderr, result.stderr.decode(errors="replace"))

    def fingerprint(self):
        n = self.node
        return {path: stamp(path) for path in (*n.state.glob("state.db*"), *n.witness.iterdir()) if path.is_file()}

    def test_live_authority_and_invalid_reference_sets_are_refused(self):
        n = self.start(BINARIES)
        candidate = fake_ref()
        self.refused("Conflict", self.inventory([candidate]))
        n.stop()
        frozen = self.fingerprint()
        excessive = [{**candidate, "resource_id": f"input-{i:064x}"} for i in range(17)]
        for refs in ([], [candidate, candidate], excessive, [{**candidate, "version": "r2"}],
                     [{**candidate, "resource_id": "input-prepared-" + "0" * 64}]):
            with self.subTest(refs=refs):
                self.refused("InvalidArgument", self.inventory(refs))
        self.refused("PreconditionFailed", self.inventory([candidate]))
        self.refused("InvalidArgument", self.inventory([candidate], scratch=n.state))
        self.assertEqual(self.fingerprint(), frozen)

    @unittest.skipUnless(LEGACY_BINARIES, "set PSIOS_LEGACY_INPUT_BIN_DIR to actual format 95 binaries")
    def test_inventory_distinguishes_historical_causal_and_literal_refs_without_leaking_bodies(self):
        n = self.start(LEGACY_BINARIES)
        principal = n.call("Principal.WhoAmI", {})
        volume = n.perform("volume.create", {"name": "legacy-inventory-fixture"})
        common = {**target(volume), "name": "stored-input"}
        private = {name: f"body-marker-{name}-{key()}" for name in ("orphan", "schedule", "signal", "literal")}
        drafts = {name: n.prepare("volume.write", {**common, "content": content})["input_ref"]
                  for name, content in private.items()}
        # The literal is neither a ResourceRef nor a structured relationship.
        # A conservative scanner still reports the row containing its ID.
        literal_payload = f"free-text link: [{drafts['literal']['resource_id']}]"
        n.prepare("volume.write", {**common, "content": literal_payload})

        accepted_body = {"limit": 1}
        accepted_ref = n.prepare("node.processes", accepted_body)
        task_id = n.call("Task.Submit", accepted_ref)["task_id"]
        task = n.wait(task_id)
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"), task)
        artifact, = task["result"]["artifact_refs"]
        archive = n.call("Task.Archive", target(n.get(task_id)["reference"]))
        self.assertEqual(n.get(archive["resource_id"])["value"]["task_ref"]["resource_id"], task_id)
        task_history = n.call("Task.History", {"id": task_id})["items"]
        self.assertGreater(len(task_history), 1)
        requirements = [{"task_id": task_id, "digest": artifact["digest"]}]
        manifest = n.call("Task.Prepare", {"tool": "system.status", "key": key(), "input": {},
                                          "delegation_id": "owner-delegation", "requires": requirements})

        grant = n.call("Grant.Issue", {
            "principal_id": principal["principal_id"], "units": 1, "ttl_seconds": 600,
            "actions": ["schedule.manage", "task.submit", "task.read", "resource.read", "event.read", "system.status"],
            "tool_targets": ["profile-local"],
        })
        expiry = (datetime.now(timezone.utc) + timedelta(seconds=300)).isoformat().replace("+00:00", "Z")
        schedule = n.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {}, "delegation_id": grant["delegation_ref"]["resource_id"],
            "event": {"source_ref": drafts["schedule"], "condition": {"kind": "SpecChanged"},
                      "expires_at": expiry, "max_triggers": 1},
        })
        self.assertEqual(schedule["state"]["source_ref"], drafts["schedule"])
        mailbox = n.call("Mailbox.Bind", {"name": "inventory-signal", "sender_principal_id": principal["principal_id"]})
        signal_ref = n.call("Signal.Send", {"mailbox_id": mailbox["resource_id"], "key": key(),
                                           "topic": "ObservationReady", "resource_ref": drafts["signal"]})
        self.assertEqual(n.get(signal_ref["resource_id"])["value"]["resource_ref"], drafts["signal"])

        # More than 1 MiB of actual row text makes the scan and copy caps
        # independent of SQLite page geometry or the platform's empty schema.
        padding_marker = "private-scan-padding-" + key()
        for index in range(24):
            prefix = f"{padding_marker}-{index:02d}-"
            content = prefix.ljust(64 * 1024, "x")
            n.prepare("volume.write", {**common, "content": content})

        n.stop(signal.SIGKILL)
        frozen = self.fingerprint()
        witness = json.loads((n.witness / "head").read_bytes())
        refs = [drafts["signal"], accepted_ref["input_ref"], drafts["orphan"], drafts["literal"],
                manifest["input_ref"], drafts["schedule"]]
        for ref in refs:
            self.assertRegex(ref["resource_id"], r"^input-[0-9a-f]{64}$")
            self.assertEqual(ref["version"], "r1")
        result = self.inventory(refs)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        report = json.loads(result.stdout)
        self.assertEqual((report["node_id"], report["database_format"], report["head"], report["witness"]),
                         (principal["authority_id"], 95, witness["committed"], "Confirmed"), report)
        self.assertTrue(report["recovery_incarnation"], report)
        self.assertEqual((report["sqlite_integrity"], report["foreign_keys"]), ("Verified", "Verified"), report)
        self.assertEqual(report["scope"], "StoredSqliteValues")
        self.assertGreater(report["copied_bytes"], MIB)
        self.assertGreater(report["scan"]["bytes"], MIB)
        self.assertGreater(report["scan"]["tables"], 1)
        self.assertGreaterEqual(report["scan"]["rows"], len(refs) + 24)
        self.assertGreaterEqual(report["scan"]["cells"], report["scan"]["rows"])
        self.assertEqual([item["reference"] for item in report["inputs"]], refs)

        bodies = {
            **{ref["resource_id"]: ("Input", {**common, "content": private[name]}) for name, ref in drafts.items()},
            accepted_ref["input_ref"]["resource_id"]: ("Input", accepted_body),
            manifest["input_ref"]["resource_id"]: ("TaskInputManifest", {"tool_input": {}, "requires": requirements}),
        }
        for item in report["inputs"]:
            kind, body = bodies[item["reference"]["resource_id"]]
            self.assertEqual(set(item), {"reference", "kind", "owner_principal_id", "stored_body_bytes", "references"})
            self.assertEqual((item["kind"], item["owner_principal_id"], item["stored_body_bytes"]),
                             (kind, principal["principal_id"], encoded_bytes(body)), item)
        linked_signal, historical, orphan, literal, manifest_row, linked_schedule = report["inputs"]
        self.assertEqual(orphan["references"], [])
        self.assertEqual(manifest_row["references"], [])
        self.assertEqual(linked_signal["references"], [{"table": "versions", "rows": 1}])
        self.assertEqual(literal["references"], [{"table": "versions", "rows": 1}])
        self.assertEqual(linked_schedule["references"], [
            {"table": "pending_event_schedules", "rows": 1}, {"table": "versions", "rows": 1},
        ])
        tables = {match["table"]: match["rows"] for match in historical["references"]}
        self.assertGreaterEqual(tables["versions"], len(task_history) + 2)
        for value in (*private.values(), literal_payload, padding_marker, n.owner.strip()):
            self.assertNotIn(value.encode(), result.stdout + result.stderr)

        self.refused("InvalidArgument", self.inventory([refs[0], refs[0]]))
        self.refused("PreconditionFailed", self.inventory([{**refs[0], "digest": fake_ref()["digest"]}]))
        self.refused("PreconditionFailed", self.inventory([refs[0], fake_ref("1")]))
        copy_failure = self.inventory(refs, max_copy=1)
        self.refused("QuotaExceeded", copy_failure)
        scan_failure = self.inventory(refs, max_scan=1)
        self.refused("QuotaExceeded", scan_failure)
        self.assertIn(b"scan", scan_failure.stderr.lower())
        self.assertEqual(self.fingerprint(), frozen)


if __name__ == "__main__":
    unittest.main()
