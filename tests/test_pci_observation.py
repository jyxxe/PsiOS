"""Verify scoped PCI observations against Linux sysfs and durable Task results."""
import json
import os
from pathlib import Path
import subprocess
import unittest

from support import Failure, Node, target


TOOL = "node.pci"
DEVICES = Path("/sys/bus/pci/devices")
ATTRIBUTES = {
    "vendor_id": "vendor", "device_id": "device", "subsystem_vendor_id": "subsystem_vendor",
    "subsystem_device_id": "subsystem_device", "class_code": "class", "revision": "revision",
}


def linux_devices():
    paths = sorted(DEVICES.iterdir())
    addresses = {path.name for path in paths}
    return [dict(address=entry.name,
                 parent_address=parent if (parent := entry.resolve().parent.name) in addresses else None,
                 driver=Path(os.readlink(entry / "driver")).name if (entry / "driver").is_symlink() else None,
                 **{key: (entry / attribute).read_text().strip() for key, attribute in ATTRIBUTES.items()})
            for entry in paths]


class PciObservation(unittest.TestCase):
    def denied(self, code, callback):
        with self.assertRaises(Failure) as failure:
            callback()
        self.assertEqual(failure.exception.code, code)

    def scoped(self, node, principal, scope):
        return node.call("Grant.Issue", dict(principal_id=principal, tool_targets=[scope],
                         actions=[TOOL, "task.submit", "task.read"], units=3, ttl_seconds=180))

    @unittest.skipUnless(DEVICES.is_dir(), "requires Linux PCI sysfs")
    def test_scoped_observation_matches_real_devices_and_preserves_an_exact_result(self):
        node = Node()
        self.addCleanup(node.close)
        resource = node.get("tool-" + TOOL)["resource"]["spec"]
        self.assertEqual(resource["effect_class"], "ReadOnly")
        self.assertIs(resource["requires_approval"], False)
        local = node.call("Node.Get", {})["reference"]["resource_id"]
        self.denied("PermissionDenied", lambda: node.prepare(TOOL, {}))
        actor, old_grant = node.agent(["task.submit", "task.read", "node.inventory"])
        token = actor["token"]
        self.denied("PermissionDenied", lambda: node.prepare(TOOL, {}, token=token,
                    delegation=old_grant["delegation_ref"]["resource_id"]))
        wrong = self.scoped(node, actor["principal_id"], "profile-local")
        self.denied("PermissionDenied", lambda: node.call("Task.Submit", node.prepare(
                    TOOL, {}, token=token, delegation=wrong["delegation_ref"]["resource_id"]), token))
        grant = self.scoped(node, actor["principal_id"], local)
        delegation = grant["delegation_ref"]["resource_id"]
        self.denied("InvalidArgument", lambda: node.call("Task.Submit", node.prepare(
                    TOOL, {"path": "/sys"}, token=token, delegation=delegation), token))
        expected = linux_devices()
        self.assertLessEqual(len(expected), 256, "fixture exceeds supported PCI inventory size")
        request = node.prepare(TOOL, {}, token=token, delegation=delegation)
        task = node.wait(node.call("Task.Submit", request, token)["task_id"])
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        reference, = task["result"]["artifact_refs"]
        report = node.call("Artifact.Read", {"reference": reference}, token)
        self.assertEqual(report["devices"], expected)
        self.assertEqual(report["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertEqual(report["mount_namespace_inode"], str(Path("/proc/self/ns/mnt").stat().st_ino))
        node.stop()
        node.start()
        self.assertEqual(node.call("Artifact.Read", {"reference": reference}, token), report)
        node.call("Grant.Revoke", target(grant["grant_ref"]))
        self.denied("PermissionDenied", lambda: node.prepare(TOOL, {}, token=token, delegation=delegation))

    @unittest.skipUnless(os.getenv("PSIOS_TEST_PCI_MASKED"), "requires a private non-sysfs mount over /sys")
    def test_refuses_a_non_sysfs_mount(self):
        self.assertFalse(DEVICES.exists())
        node = Node()
        self.addCleanup(node.close)
        local = node.call("Node.Get", {})["reference"]["resource_id"]
        grant = self.scoped(node, "local-owner", local)
        task = node.wait(node.call("Task.Submit", node.prepare(
            TOOL, {}, delegation=grant["delegation_ref"]["resource_id"]))["task_id"])
        self.assertEqual(task["lifecycle"], "Failed", task)
        self.assertEqual(task["conditions"][0]["type"], "UnsupportedCapability", task)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_PCI_PREVIOUS_BIN_DIR"), "requires exact format 149 executables")
    def test_installing_the_pci_catalog_preserves_format_149_authority(self):
        node = Node(Path(os.environ["PSIOS_TEST_PCI_PREVIOUS_BIN_DIR"]))
        self.addCleanup(node.close)
        saved = node.submit("system.status", {})
        previous = node.wait(saved)
        self.assertEqual(previous["lifecycle"], "Succeeded", previous)
        reference, = previous["result"]["artifact_refs"]
        prior_result = node.call("Artifact.Read", {"reference": reference})
        self.denied("NotFound", lambda: node.get("tool-" + TOOL))
        node.stop()

        def verify():
            return json.loads(subprocess.check_output([*node.daemon, "verify", "--scratch-dir", node.base], timeout=30))

        self.assertEqual(verify()["database_format"], 149)
        node.binaries = Path(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug")).resolve()
        node.daemon[0] = str(node.binaries / "psiosd")
        node.start()
        self.assertEqual(node.call("Task.Get", {"id": saved}), previous)
        self.assertEqual(node.call("Artifact.Read", {"reference": reference}), prior_result)
        self.assertEqual(node.get("tool-" + TOOL)["resource"]["spec"]["effect_class"], "ReadOnly")
        self.denied("PermissionDenied", lambda: node.prepare(TOOL, {}))
        node.stop()
        status = verify()
        self.assertEqual((status["database_format"], status["witness"]), (150, "Confirmed"))
