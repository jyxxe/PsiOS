import json
import os
from pathlib import Path
import unittest

from support import Failure, Node, target


TOOL = "node.oci_runtime"


def submit(node, delegation):
    prepared = node.prepare(TOOL, {}, delegation=delegation)
    return node.call("Task.Submit", prepared)["task_id"]


def grant_observation(node, scope):
    grant = node.call("Grant.Issue", {"principal_id": "local-owner", "tool_targets": [scope],
        "actions": [TOOL, "task.submit", "task.read"], "ttl_seconds": 120, "units": 4})
    return grant, grant["delegation_ref"]["resource_id"]


class OciRuntimeObservation(unittest.TestCase):
    def test_reports_an_absent_engine_without_leaking_the_endpoint(self):
        node = Node()
        self.addCleanup(node.close)
        baseline = node.call("Node.Get", {})
        node_id = baseline["reference"]["resource_id"]
        settings = json.loads(node.config.read_text())
        not_present = node.base / "private-engine-endpoint.sock"
        node.stop()
        node.config.write_text(json.dumps(dict(settings, docker_socket=str(not_present))))
        node.start()

        with self.assertRaises(Failure) as denied:
            submit(node, "owner-delegation")
        self.assertEqual(denied.exception.code, "PermissionDenied")
        _, wrong = grant_observation(node, "profile-local")
        with self.assertRaises(Failure) as denied:
            submit(node, wrong)
        self.assertEqual(denied.exception.code, "PermissionDenied")

        grant, delegation = grant_observation(node, node_id)
        task = node.wait(submit(node, delegation))
        self.assertEqual((task["lifecycle"], task["outcome_knowledge"]), ("Succeeded", "Known"))
        evidence, = task["result"]["artifact_refs"]
        report = node.call("Artifact.Read", {"reference": evidence})
        self.assertEqual(set(report), {"status", "reason", "driver", "boot_id", "observed_at"})
        wanted = ("Unsupported", "NonRootCoreRequired") if os.geteuid() == 0 else (
            "Unavailable", "EngineInformationUnavailable")
        self.assertEqual((report["status"], report["reason"]), wanted)
        self.assertEqual(report["driver"], "Docker")
        self.assertEqual(report["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertNotIn(str(not_present), json.dumps(report))
        node.call("Grant.Revoke", target(grant["grant_ref"]))
        with self.assertRaises(Failure) as revoked:
            submit(node, delegation)
        self.assertEqual(revoked.exception.code, "PermissionDenied")
        self.assertEqual(node.call("Node.Get", {})["reference"], baseline["reference"])
