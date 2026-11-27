"""Round-trip pagination over names created by the real Linux filesystem."""

import signal
import unittest

from support import Failure, Node, target


class VolumeFilePages(unittest.TestCase):
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
