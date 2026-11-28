"""Check native boot assembly evidence and rejection before image publication."""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, password, sha


TOOLS = ("cpio", "grub-mkstandalone", "isoinfo", "mcopy", "mkfs.fat", "mmd")


def exercise(args, work):
    _, operator_hash = password(work, "operator.hash")
    resolved = {name: Path(shutil.which(name)).absolute() for name in TOOLS}
    expected = {name: sha(path) for name, path in resolved.items()}

    def build(output, *, env=None):
        return subprocess.run([sys.executable, "-B", args.builder, output,
                               "--kernel-iso", args.kernel_iso, "--core-archive", args.archive,
                               "--core-sha256", sha(args.archive), "--console-hash", operator_hash],
                              capture_output=True, text=True, env=env, timeout=180)

    image = work / "native.raw"
    produced = build(image)
    assert produced.returncode == 0, produced.stderr
    receipt = json.loads(produced.stdout)
    assert receipt["boot_assembly_tools"] == {"format": 1, "executable_sha256": expected}, receipt
    original = sha(image)
    assert original == receipt["initial_image_sha256"]

    # Delegate to the actual host tools. After cpio has assembled the initramfs,
    # the final FAT-copy command changes only this build's private cpio executable.
    shims = work / "private-tools"
    shims.mkdir(mode=0o700)
    cpio = shims / "cpio"
    mcopy = shims / "mcopy"
    cpio.write_text(f'#!/bin/sh\nexec {shlex.quote(str(resolved["cpio"]))} "$@"\n')
    mcopy.write_text(f'#!/bin/sh\nset -e\n{shlex.quote(str(resolved["mcopy"]))} "$@"\n'
                     f"printf '%s\\n' '# modified by test' >> {shlex.quote(str(cpio))}\n")
    for shim in (cpio, mcopy):
        shim.chmod(0o700)
    initial_cpio = sha(cpio)
    unpublished = work / "must-not-publish.raw"
    rejected = build(unpublished, env={**os.environ, "PATH": str(shims) + os.pathsep + os.environ["PATH"]})
    assert sha(cpio) != initial_cpio, "the real FAT-copy process did not reach the executable change"
    assert rejected.returncode != 0 and not rejected.stdout, rejected
    assert "boot assembly executable changed before publication: cpio" in rejected.stderr, rejected.stderr
    assert not unpublished.exists() and not list(work.glob(".psios-disk-*"))
    assert sha(image) == original
    print("PASS real native assembly tool evidence and atomic publication refusal after a changed executable")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--builder", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-boot-tools-") as directory:
        exercise(parser.parse_args(), Path(directory))
