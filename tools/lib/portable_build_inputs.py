"""Record and check source inputs and the observed portable build toolchain."""

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess


DOCKERFILE = "contrib/portable/Dockerfile"
SOURCES = ("Cargo.toml", "Cargo.lock", DOCKERFILE)
PLATFORM = "linux/amd64"
TARGET = "x86_64-unknown-linux-musl"


def source_inputs(repo, revision):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("expected a full lowercase Git commit")
    files = {path: subprocess.check_output(["git", "-C", repo, "show", f"{revision}:{path}"]) for path in SOURCES}
    builders = re.findall(rb"^FROM (\S+@sha256:[0-9a-f]{64}) AS build$", files[DOCKERFILE], re.MULTILINE)
    if len(builders) != 1:
        raise ValueError("expected a single digest-pinned build image")
    return {"commit": revision, "builder_image": builders[0].decode("ascii"),
            "sha256": {path: hashlib.sha256(content).hexdigest() for path, content in files.items()}}


def observed_toolchain(path):
    return {name: (path / name).read_text(encoding="utf-8").strip()
            for name in ("platform", "target", "alpine-release", "rustc", "cargo")}


def verify_toolchain(toolchain):
    if not isinstance(toolchain, dict) or set(toolchain) != {"platform", "target", "alpine-release", "rustc", "cargo"}:
        raise ValueError("invalid portable toolchain fields")
    if not all(isinstance(value, str) for value in toolchain.values()):
        raise ValueError("invalid portable toolchain values")
    if (toolchain["platform"], toolchain["target"]) != (PLATFORM, TARGET):
        raise ValueError("unexpected portable build platform or compilation target")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", toolchain["alpine-release"]):
        raise ValueError("invalid Alpine builder release")
    for name in ("rustc", "cargo"):
        details = toolchain[name].splitlines()
        if (not details or not re.match(rf"^{name} [0-9]+\.[0-9]+\.[0-9]+\b", details[0])
                or f"host: {TARGET}" not in details
                or not any(re.fullmatch(r"commit-hash: [0-9a-f]{40}", line) for line in details)):
            raise ValueError(f"invalid {name} version, host or source commit")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo", type=Path)
    parser.add_argument("source_commit")
    parser.add_argument("manifest", type=Path, help="build-inputs.json in the portable archive directory")
    parser.add_argument("--toolchain", type=Path, help="record the files exported from the build container")
    args = parser.parse_args()
    try:
        source = source_inputs(args.repo, args.source_commit)
        if args.toolchain:
            toolchain = observed_toolchain(args.toolchain)
            verify_toolchain(toolchain)
            with args.manifest.open("x", encoding="utf-8") as output:
                json.dump({"format": 1, "source": source, "toolchain": toolchain}, output, indent=2, sort_keys=True)
                output.write("\n")
        else:
            manifest = json.loads(args.manifest.read_text(encoding="utf-8"))
            if (not isinstance(manifest, dict) or set(manifest) != {"format", "source", "toolchain"}
                    or type(manifest["format"]) is not int or manifest["format"] != 1 or manifest["source"] != source):
                raise ValueError("build inputs do not match the requested source tree")
            verify_toolchain(manifest["toolchain"])
            with (args.manifest.parent / "Cargo.lock").open("rb") as lock:
                if hashlib.file_digest(lock, "sha256").hexdigest() != source["sha256"]["Cargo.lock"]:
                    raise ValueError("packaged Cargo.lock does not match the requested source tree")
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
