"""Check both the immutable original documentation and an amended live specification."""
import hashlib
import importlib.util
from pathlib import Path
import shutil
import tempfile
import unittest
import zipfile


RELEASES = Path(__file__).resolve().parents[1] / "docs/releases"
spec = importlib.util.spec_from_file_location("psios_docs_release", RELEASES / "restore_v1.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class DocumentationReleaseTests(unittest.TestCase):
    def test_original_archive_amended_tree_and_corruption(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for part in RELEASES.glob("*.part*"):
                shutil.copyfile(part, root / part.name)
            archive = release.rebuild(root)
            with zipfile.ZipFile(archive) as original:
                original.extractall(root)
            tree = root / "PsiOS-v1"
            for name in release.RESTORED:
                (tree / name).unlink()
            self.assertEqual(release.restore_from_zip(archive, tree, True, True), 2)
            chapter = tree / "CHANGELOG.md"
            old = release.digest(chapter.read_bytes())
            chapter.write_bytes(chapter.read_bytes() + b"\nRevision in the current checkout\n")
            new = release.digest(chapter.read_bytes())
            manifest = tree / "MANIFEST.sha256"
            manifest.write_text(manifest.read_text().replace(old + "  CHANGELOG.md", new + "  CHANGELOG.md"))
            self.assertEqual(release.restore_from_zip(archive, tree, False, True), 0)
            with self.assertRaisesRegex(RuntimeError, "modified: CHANGELOG.md"):
                release.restore_from_zip(archive, tree, True)
            chapter.write_bytes(b"damaged")
            with self.assertRaisesRegex(RuntimeError, "modified: CHANGELOG.md"):
                release.restore_from_zip(archive, tree, False, True)
            oversized = tree / next(iter(release.RESTORED))
            oversized.write_bytes(b"operator changes")
            with self.assertRaisesRegex(RuntimeError, "Refusing to overwrite"):
                release.restore_from_zip(archive, tree, False, True)
            self.assertEqual(oversized.read_bytes(), b"operator changes")
            self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(), release.SHA256)

    def test_reject_malformed_and_indirect_manifest_entries(self):
        digest = "a" * 64
        for manifest in ["", "invalid", digest + "  ../other", digest + "  /root", digest + "  a/../b",
                         digest + "  a\\b", (digest + "  entry\n") * 2]:
            with self.subTest(manifest=manifest), self.assertRaises(RuntimeError):
                release.manifest_entries(manifest)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            docs = base / "docs"
            docs.mkdir()
            other = base / "other"
            other.write_bytes(b"outside the tree")
            (docs / "chapter").symlink_to(other)
            with self.assertRaisesRegex(RuntimeError, "indirect"):
                release.verify_tree(docs, {"chapter": release.digest(other.read_bytes())})
