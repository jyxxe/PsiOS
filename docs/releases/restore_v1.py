#!/usr/bin/env python3
"""Rebuild the exact PsiOS v1 documentation ZIP and restore oversized archive files.

Usage from a checkout:
    python3 docs/releases/restore_v1.py
    python3 docs/releases/restore_v1.py --write-zip --verify-all

This tool runs offline, does not execute source archives and never overwrites a
non-matching existing source file.
"""
from __future__ import annotations

import argparse
import hashlib
import os
import tempfile
import zipfile
from pathlib import Path

ARCHIVE = "PsiOS-v1.0.0-docs-2026-11-22.zip"
SHA256 = "b6f2c5510063c1304b11bd623cd159e1155f706dd1c6b8398c1753b1763127c3"
PART_SHA256 = [
    "b78eb179b2bc68ba1323446f34c3ecd919d25dad21de43fa0f87feac1ca973e8",
    "8a9876bc1cea61ec943d7cea2dc8efb902dc0861305a59c035370ebc95970722",
    "0c49015765c430c0267a9ea9795e6402b1d78070e057192cdc2fa4d8c947f9ed",
    "f13f1910983a34824fa152f2f83e4f5d2d1c95537a688ab73e678c049adca88b",
    "d71f45e79c9c1c8b2800a23efd282b3d5e50b32fdfe99d414a44faaa2d54bb4c",
]
RESTORED = {
    "registries/legacy-requirements.json": "96c296e9b275c074319cbff730ebaedebd8770ecedf3881acfbe1ed34813013c",
    "sources/archive/xelab-package-history.zip": "e6d1b43ed5644e8f7f8e7eeaf72495e7471887918b5afb012970ad32c910b599",
}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def rebuild(directory: Path) -> Path:
    temp_fd, temp_name = tempfile.mkstemp(prefix="psios-v1-", suffix=".zip", dir=directory)
    temp_path = Path(temp_name)
    os.close(temp_fd)
    full_hash = hashlib.sha256()
    try:
        with temp_path.open("wb") as dest:
            for i, wanted in enumerate(PART_SHA256, 1):
                chunk = (directory / f"{ARCHIVE}.part{i:02d}").read_bytes()
                if digest(chunk) != wanted:
                    raise RuntimeError(f"Part {i} SHA-256 mismatch")
                dest.write(chunk)
                full_hash.update(chunk)
        if full_hash.hexdigest() != SHA256:
            raise RuntimeError("Reassembled ZIP SHA-256 mismatch")
        with zipfile.ZipFile(temp_path) as z:
            if z.testzip() is not None:
                raise RuntimeError("ZIP CRC check failed")
        return temp_path
    except BaseException:
        temp_path.unlink(missing_ok=True)
        raise


def restore_from_zip(archive: Path, docs_root: Path, verify_all: bool) -> int:
    restored = 0
    with zipfile.ZipFile(archive) as z:
        for rel, expected_sha in RESTORED.items():
            payload = z.read("PsiOS-v1/" + rel)
            if digest(payload) != expected_sha:
                raise RuntimeError(f"Archived content mismatch: {rel}")
            target = docs_root / rel
            if target.exists():
                if digest(target.read_bytes()) != expected_sha:
                    raise RuntimeError(f"Refusing to overwrite modified file: {target}")
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            fd, name = tempfile.mkstemp(prefix=".restore-", dir=target.parent)
            try:
                with os.fdopen(fd, "wb") as f:
                    f.write(payload)
                os.replace(name, target)
            finally:
                Path(name).unlink(missing_ok=True)
            restored += 1
        if verify_all:
            manifest = z.read("PsiOS-v1/MANIFEST.sha256").decode("utf-8")
            verified = 0
            for line in manifest.splitlines():
                if "  " not in line:
                    continue
                sha, rel = line.split("  ", 1)
                target = docs_root / rel
                if not target.is_file() or digest(target.read_bytes()) != sha:
                    raise RuntimeError(f"Documentation file missing or modified: {rel}")
                verified += 1
            print(f"Verified {verified} manifest entries")
    return restored


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write-zip", action="store_true", help="Keep the rebuilt ZIP in docs/releases")
    parser.add_argument("--verify-all", action="store_true", help="Verify every v1 manifest entry after restoration")
    args = parser.parse_args()
    releases = Path(__file__).resolve().parent
    docs_root = releases.parent / "v1"
    temp = rebuild(releases)
    try:
        print(f"Reassembled and verified {ARCHIVE} ({temp.stat().st_size} bytes)")
        restored = restore_from_zip(temp, docs_root, args.verify_all)
        print(f"Restored {restored} missing oversized v1 files")
        if args.write_zip:
            dest = releases / ARCHIVE
            if dest.exists():
                if digest(dest.read_bytes()) != SHA256:
                    raise RuntimeError("Existing ZIP differs; refusing to overwrite")
            else:
                os.replace(temp, dest)
            print(f"Full ZIP available at {dest}")
    finally:
        temp.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
