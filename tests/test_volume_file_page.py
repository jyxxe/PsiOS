"""Round-trip pagination over names created by the real Linux filesystem."""

import os
import signal
import subprocess
import unittest

from support import Failure, Node, target


class VolumeFilePages(unittest.TestCase):
    def test_entries_do_not_report_sizes_of_links_and_special_files(self):
        node = Node()
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "file-page-confined-links"})
        root = node.state / "volumes" / volume["resource_id"] / "data"
        outside = node.base / "outside"
        outside.write_bytes(b"outside the volume")
        os.link(outside, root / "hard-link")
        (root / "symbolic-link").symlink_to(outside)
        os.mkfifo(root / "pipe")
        (root / "private-file").write_bytes(b"abc")
        page = node.perform("volume.files", {"target": target(volume)})
        self.assertEqual(page["entries"], [
            {"name": name, "kind": kind, "bytes": size}
            for name, kind, size in (("hard-link", "unsupported", "0"), ("pipe", "unsupported", "0"),
                                     ("private-file", "file", "3"), ("symbolic-link", "unsupported", "0"))
        ])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_VOLUME_BIND"), "requires a private mount namespace")
    def test_mount_boundary_prevents_listing_foreign_entry_metadata(self):
        node = Node()
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "file-page-confined-mount"})
        root = node.state / "volumes" / volume["resource_id"] / "data"
        outside = node.base / "outside"
        outside.write_bytes(b"outside the volume")
        mountpoint = root / "bound-file"
        mountpoint.touch()
        self.assertEqual(outside.stat().st_dev, mountpoint.stat().st_dev)
        subprocess.run(["mount", "--bind", outside, mountpoint], check=True)
        try:
            self.assertEqual(mountpoint.stat().st_size, outside.stat().st_size)
            failed = node.wait(node.submit("volume.files", {"target": target(volume)}))
            self.assertEqual(failed["lifecycle"], "Failed", failed)
            self.assertEqual(failed["conditions"][0]["type"], "StorageUnavailable", failed)
            self.assertEqual(failed["result"]["artifact_refs"], [], failed)
        finally:
            subprocess.run(["umount", mountpoint], check=True)
        page = node.perform("volume.files", {"target": target(volume)})
        self.assertEqual(page["entries"], [{"name": "bound-file", "kind": "file", "bytes": "0"}])

    def test_every_returned_cursor_advances_the_same_directory(self):
        node = Node()
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "file-page-boundaries"})
        directory = node.state / "volumes" / volume["resource_id"] / "data" / "subdirectory"
        directory.mkdir()

        control_name = "a\n\x1bstart"
        long_name = "b" + "界" * 84 + "xy"
        self.assertEqual(len(long_name.encode()), 255)
        names = (control_name, long_name, "c-end")
        for name in names:
            (directory / name).write_text("content")

        params = {"target": target(volume), "path": "subdirectory", "limit": 1}
        for index, name in enumerate(names):
            page = node.perform("volume.files", params)
            self.assertEqual(page["entries"], [{"name": name, "kind": "file", "bytes": "7"}])
            self.assertEqual(page["volume_version"], volume["version"])
            self.assertEqual(page["next_after"], name if index < len(names) - 1 else None)
            params["after"] = page["next_after"]
            if index == 0:
                node.stop(signal.SIGKILL)
                node.start()

    def test_pinned_membership_survives_restart_and_rejects_insertions_and_replacements(self):
        node = Node()
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "directory-membership"})
        root = node.state / "volumes" / volume["resource_id"] / "data"
        directory = root / "reports"
        directory.mkdir()
        for name in ("b", "d"):
            (directory / name).touch()
        query = {"target": target(volume), "path": "reports", "limit": 1}
        first = node.perform("volume.files", query)
        revision = first["directory_revision"]
        self.assertRegex(revision, r"^sha256:[0-9a-f]{64}$")
        next_page = dict(query, after=first["next_after"], expected_directory_revision=revision)
        node.stop(signal.SIGKILL)
        node.start()
        (directory / "d").write_bytes(b"now six")
        second = node.perform("volume.files", next_page)
        self.assertEqual(second["directory_revision"], revision)
        self.assertEqual(second["entries"], [{"name": "d", "kind": "file", "bytes": "7"}])

        (directory / "a").touch()
        stale = node.wait(node.submit("volume.files", next_page))
        self.assertEqual(stale["conditions"][0]["reason"], "Conflict", stale)
        self.assertEqual(stale["result"]["artifact_refs"], [])
        fresh = node.perform("volume.files", query)
        self.assertEqual((fresh["volume_version"], fresh["next_after"]), (first["volume_version"], "a"))
        self.assertNotEqual(fresh["directory_revision"], revision)
        # Even an identically populated replacement cannot reuse the original directory's revision.
        directory.rename(root / "previous")
        directory.mkdir()
        for name in ("b", "d"):
            (directory / name).touch()
        replaced = node.wait(node.submit("volume.files", next_page))
        self.assertEqual(replaced["conditions"][0]["reason"], "Conflict", replaced)
        self.assertEqual(replaced["result"]["artifact_refs"], [])
        self.assertEqual(node.perform("volume.files", dict(query, after="b"))["entries"][0]["name"], "d")
        for invalid in ("bad", "sha256:" + "A" * 64):
            with self.assertRaises(Failure) as rejected:
                node.prepare("volume.files", dict(query, expected_directory_revision=invalid))
            self.assertEqual(rejected.exception.code, "InvalidArgument")

    def test_cursor_shape_does_not_expand_filesystem_paths(self):
        node = Node()
        self.addCleanup(node.close)
        volume = node.perform("volume.create", {"name": "file-page-path-boundaries"})
        for after in ("", ".", "..", "a/b", "a\x00b"):
            with self.subTest(after=after), self.assertRaises(Failure) as rejected:
                node.prepare("volume.files", {"target": target(volume), "after": after})
            self.assertEqual(rejected.exception.code, "InvalidArgument")
        with self.assertRaises(Failure) as rejected:
            node.prepare("volume.files", {"target": target(volume), "path": "a\nb"})
        self.assertEqual(rejected.exception.code, "InvalidArgument")
