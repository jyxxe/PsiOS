"""List only Projects visible under current membership and read grants."""
import json
import os
import subprocess
import unittest

from support import Failure, Node, key, target


class ProjectDirectoryTests(unittest.TestCase):
    def test_pages_skip_projects_without_both_current_authorities(self):
        node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(node.close)
        viewer = node.call("Agent.Register", {"name": "directory-viewer"})
        principal = viewer["principal_id"]
        token = viewer["token"]

        def grant(subject, actions, ids):
            return node.call("Grant.Issue", {
                "principal_id": subject, "actions": actions, "tool_targets": ids,
                "units": 1, "ttl_seconds": 120,
            })

        def listing(params):
            return node.call("Project.List", params, token)

        with self.assertRaises(Failure) as ungranted:
            listing({})
        self.assertEqual(ungranted.exception.code, "PermissionDenied")

        projects = sorted((node.call("Project.Create", {"key": key(), "title": title})["reference"]
                           for title in ("First", "Second", "Third", "Fourth")),
                          key=lambda reference: reference["resource_id"])
        ids = [ref["resource_id"] for ref in projects]
        grant("local-owner", ["project.read", "project.manage"], ids)
        grant(principal, ["project.read"], [ids[0], ids[1]])
        last_grant = grant(principal, ["project.read"], [ids[3]])

        def set_role(index, role):
            updated = node.call("Project.SetMember", {
                **target(projects[index]), "principal_id": principal, "role": role,
            })
            projects[index] = updated["reference"]

        for index in (0, 2, 3):
            set_role(index, "Viewer")
        first = listing({"limit": 1})
        self.assertEqual([record["reference"]["resource_id"] for record in first["items"]], [ids[0]])
        self.assertEqual(first["next_after_id"], ids[0])
        last = listing({"after_id": first["next_after_id"], "limit": 1})
        self.assertEqual([record["reference"]["resource_id"] for record in last["items"]], [ids[3]])
        self.assertIsNone(last["next_after_id"])
        with self.assertRaises(Failure) as bad_cursor:
            listing({"after_id": "unrelated-task"})
        self.assertEqual(bad_cursor.exception.code, "InvalidArgument")

        token_file = node.base / "viewer.token"
        token_file.write_text(token)
        token_file.chmod(0o600)
        cli = [str(node.binaries / "psiosctl"), "--socket", str(node.socket),
               "--token-file", str(token_file), "project", "list", "--limit", "1"]
        self.assertEqual(json.loads(subprocess.check_output(cli, timeout=10)), first)

        node.call("Grant.Revoke", target(last_grant["grant_ref"]))
        self.assertEqual(listing({"after_id": ids[0]}), {"items": [], "next_after_id": None})
        set_role(0, None)
        self.assertEqual(listing({}), {"items": [], "next_after_id": None})
