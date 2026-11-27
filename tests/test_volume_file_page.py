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
