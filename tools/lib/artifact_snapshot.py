"""Copy one regular file for local attestation verification."""
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "images" / "rescue"))
from boot_media import regular_file


def snapshot(source, destination):
    with regular_file(source.resolve(strict=True)) as original, destination.open("xb") as copy:
        before = os.fstat(original.fileno())
        remaining = before.st_size
        while remaining:
            chunk = original.read(min(1024 * 1024, remaining))
            if not chunk:
                raise ValueError(f"artifact was truncated during snapshot: {source}")
            copy.write(chunk)
            remaining -= len(chunk)
        after = os.fstat(original.fileno())
        if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
                after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError(f"artifact changed during snapshot: {source}")


if __name__ == "__main__":
    try:
        snapshot(Path(sys.argv[1]), Path(sys.argv[2]))
    except (OSError, ValueError) as error:
        sys.exit(str(error))
