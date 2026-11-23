"""Exercise the published binaries as the only programs in a Linux root filesystem."""
import argparse
import json
from pathlib import Path
import subprocess
import time
import uuid


def docker(*args, check=True, data=None):
    result = subprocess.run(["docker", *args], input=data, text=True,
                            capture_output=True, timeout=90)
    if check and result.returncode:
        raise RuntimeError(f"docker {args[0]} failed: {result.stderr or result.stdout}")
    return result


def verify_recovery(archive, setup_image):
    volume = "psios-portable-" + uuid.uuid4().hex
    container = volume + "-node"
    image = docker("import", str(archive)).stdout.strip()
    mount = f"type=volume,src={volume},dst=/data"
    limits = ["--user", "65534:65534", "--network", "none", "--read-only",
              "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
              "--tmpfs", "/tmp:rw,nosuid,nodev,size=128m,mode=1777"]
    daemon = ["/psiosd", "--state", "/data/core"]
    client = ["/psiosctl", "--socket", "/data/core/api.sock", "--token-file",
              "/data/core.witness/owner.token"]

    def start():
        docker("run", "-d", "--name", container, *limits, "--mount", mount, image, *daemon, "serve")
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if docker("exec", container, *client, "whoami", check=False).returncode == 0:
                return
            time.sleep(0.15)
        raise RuntimeError("authority did not start: " + docker("logs", container).stderr)

    def ctl(*args, data=None):
        response = docker("exec", "-i", container, *client, *args, data=data)
        return json.loads(response.stdout)

    try:
        docker("volume", "create", volume)
        # Only this setup container sees the test runner's Alpine tools.
        docker("run", "--rm", "--network", "none", "--mount", mount, setup_image,
               "chmod", "1777", "/data")
        docker("run", "--rm", *limits, "--mount", mount, image, *daemon, "init")
        start()
        node = ctl("node")["reference"]["resource_id"]
        submit = ["submit", "system.status", "--key", "portable-recovery"]
        task = ctl(*submit, data="{}")["task_id"]
        result = ctl("wait", task)
        assert result["lifecycle"] == "Succeeded"

        docker("kill", "--signal=KILL", container)
        docker("rm", container)
        verified = docker("run", "--rm", *limits, "--mount", mount + ",readonly",
                          image, *daemon, "verify")
        report = json.loads(verified.stdout)
        assert report["node_id"] == node and report["sqlite_integrity"] == "Verified"

        start()
        replay = ctl(*submit, data="{}")
        assert replay["deduplicated"] and replay["task_id"] == task
        assert ctl("wait", task)["result"] == result["result"]
        print("Verified the original node, Task and result after SIGKILL and read-only inspection.")
    finally:
        docker("rm", "-f", container, check=False)
        docker("volume", "rm", volume, check=False)
        docker("image", "rm", image, check=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("setup_image", help="image providing chmod to initialize an empty test volume")
    args = parser.parse_args()
    verify_recovery(args.archive, args.setup_image)
