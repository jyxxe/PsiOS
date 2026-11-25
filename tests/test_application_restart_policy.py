"""Check real Core authorization for the OCI instance inspection and restart tools."""
import os
import unittest

from support import Failure, Node


class RestartPolicyTests(unittest.TestCase):
    def test_tool_registration_and_explicit_scope(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        restart = "application.restart"
        inspect = "application.inspect_instance"
        restart_capability = node.get(f"tool-{restart}")
        inspect_capability = node.get(f"tool-{inspect}")
        self.assertEqual(restart_capability["value"]["actions"], [restart])
        self.assertTrue(restart_capability["resource"]["spec"]["requires_approval"])
        self.assertEqual(inspect_capability["value"]["actions"], [inspect])
        self.assertEqual(inspect_capability["resource"]["spec"]["effect_class"], "ReadOnly")
        issue = {"principal_id": "local-owner", "actions": ["task.submit", restart, inspect],
                 "ttl_seconds": 60, "units": 1}
        with self.assertRaises(Failure) as unscoped:
            node.call("Grant.Issue", issue)
        self.assertEqual(unscoped.exception.code, "InvalidArgument")

        args = {"id": "profile-local", "expected_resource_version": "r1"}
        valid = {**args, "expected_boot_id": "21918b5f-3cdd-4cbd-8948-53319cbe48f6",
                 "expected_started_at": "2026-11-25T04:00:00.123456789Z"}
        with self.assertRaises(Failure) as missing:
            node.prepare(restart, valid)
        self.assertEqual(missing.exception.code, "PermissionDenied")
        accepted = node.call("Grant.Issue", {**issue, "tool_targets": ["profile-local"]})
        delegation = accepted["delegation_ref"]["resource_id"]
        with self.assertRaises(Failure) as malformed:
            node.prepare(restart, dict(valid, expected_started_at="today"), delegation=delegation)
        self.assertEqual(malformed.exception.code, "InvalidArgument")
        request = node.prepare(restart, valid, delegation=delegation)
        with self.assertRaises(Failure) as foreign_kind:
            node.call("Task.Submit", request)
        self.assertEqual(foreign_kind.exception.code, "InvalidArgument")


if __name__ == "__main__":
    unittest.main()
