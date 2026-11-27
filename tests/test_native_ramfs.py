"""Verify the native initramfs member with the same GNU cpio used to construct disks."""
import errno
import gzip
from importlib.util import module_from_spec, spec_from_file_location
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

INSTALLER = Path(__file__).resolve().parents[1] / "images/native/create.py"
spec = spec_from_file_location("native_ramfs_installer", INSTALLER)
installer = module_from_spec(spec)
spec.loader.exec_module(installer)


@unittest.skipUnless(shutil.which("cpio"), "requires GNU cpio")
class NativeRamfs(unittest.TestCase):
    def test_member_preserves_pinned_prefix_permissions_and_exact_newc_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            binary = work / "program"
            binary.write_bytes(b"native payload\n" * 16384)
            binary.chmod(0o755)
            (work / "source").write_bytes(b"verified source\n")
            names = ["program", "source"]
            raw = subprocess.check_output(["cpio", "--quiet", "-o", "-H", "newc", "--reproducible", "--owner=0:0"],
                                          cwd=work, input=b"program\nsource\n")
            self.assertTrue(raw.startswith(b"070701"))
            for prefix in (b"base", b"base1", b"base12", b"base123"):
                with self.subTest(alignment=len(prefix) % 4):
                    initrd = work / "initrd"
                    initrd.write_bytes(prefix)
                    installer.append_deployment_payload(work, names)
                    data = initrd.read_bytes()
                    aligned = prefix + bytes(-len(prefix) % 4)
                    self.assertEqual(data[:len(aligned)], aligned)
                    member = data[len(aligned):]
                    self.assertEqual(member[:8], b"\x1f\x8b\x08\x00" + bytes(4))
                    self.assertEqual(gzip.decompress(member), raw)
                    self.assertLess(len(member), len(raw))

    def test_cpio_read_error_reaches_the_builder(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            (work / "initrd").write_bytes(b"pinned")
            with self.assertRaises(subprocess.CalledProcessError):
                installer.append_deployment_payload(work, ["missing"])

    def test_full_output_device_fails_the_append(self):
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            (work / "initrd").symlink_to("/dev/full")
            (work / "source").write_bytes(b"verified source")
            with self.assertRaises(OSError) as failure:
                installer.append_deployment_payload(work, ["source"])
            self.assertEqual(failure.exception.errno, errno.ENOSPC)
