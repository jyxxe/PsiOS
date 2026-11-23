"""Explicit live-engine acceptance. Builds a unique scratch image without a registry or network."""
import argparse
import io
import json
import subprocess
import tarfile

from support import Node, key, target


def main(binaries):
    node = Node(binaries)
    tag = "psios-acceptance:" + key()
    app_id = None
    docker = ["docker", "--host", "unix:///var/run/docker.sock"]
    try:
        c = node.base / "probe.c"
        c.write_text('''#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(void) {
    FILE *f=fopen("/data/starts", "a");
    if (!f) return 2;
    fprintf(f,"%s\\n",getenv("TEST_NAME"));
    if (fclose(f)) return 3;
    puts("probe ready"); fflush(stdout);
    for (;;) pause();
}
''')
        binary = node.base / "probe"
        subprocess.run(["cc", "-static", "-O2", str(c), "-o", str(binary)], check=True)
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode="w") as tar:
            tar.add(binary, arcname="probe")
        image_id = subprocess.check_output(docker + ["import", "-", tag], input=archive.getvalue()).decode().strip()
        volume = node.perform("volume.create", {"name": "persistent-data"})
        spec = dict(image_id=image_id, name="acceptance", release_version="1.0.0", argv=["/probe"],
                    environment={"TEST_NAME": "first"}, license_expression="0BSD", volume=target(volume),
                    cpu_millicores=200, memory_max_bytes=33554432, pids_max=16, ephemeral_max_bytes=1048576)
        app = node.perform("application.install", spec)
        app_id = app["resource_id"]
        data = node.state / "volumes" / volume["resource_id"] / "data"
        detached = data.with_name("test-unavailable")
        data.rename(detached)
        missing = node.wait(node.submit("application.start", target(app)), autoapprove=True)
        assert missing["lifecycle"] == "Failed" and not data.exists(), missing
        detached.rename(data)
        app = node.perform("application.start", target(app))
        health = node.perform("application.health", target(app))
        assert health["process_running"] is True, health
        assert "probe ready" in node.perform("application.logs", target(app))["stdout"]
        cid = health["container_id"]
        inspected = json.loads(subprocess.check_output(docker + ["inspect", cid]))[0]
        assert inspected["HostConfig"]["NetworkMode"] == "none"
        assert inspected["HostConfig"]["ReadonlyRootfs"]
        assert inspected["HostConfig"]["Memory"] == spec["memory_max_bytes"]
        assert inspected["HostConfig"]["PidsLimit"] == 16
        volume = node.get(volume["resource_id"])["reference"]
        busy = node.wait(node.submit("backup.create", target(volume)))
        assert busy["lifecycle"] == "Failed" and busy["conditions"][0]["type"] == "Conflict", busy
        node.stop()
        node.start()
        assert node.perform("application.health", target(app))["container_id"] == cid
        app = node.perform("application.stop", target(app))
        app = node.call("Application.Configure", dict(**target(app), environment={"TEST_NAME": "second"}))
        app = node.perform("application.start", target(app))
        app = node.perform("application.stop", target(app))
        node.perform("application.uninstall", target(app))
        volume = node.get(volume["resource_id"])["reference"]
        assert node.perform("volume.read", dict(**target(volume), name="starts"))["content"] == "first\nsecond\n"
        backup = node.perform("backup.create", target(volume))
        restored = node.perform("backup.restore", target(backup))
        assert restored["resource_id"] != volume["resource_id"]
        assert node.perform("volume.read", dict(**target(restored), name="starts"))["content"] == "first\nsecond\n"
        node.perform("volume.activate", target(restored))
        print("PASS live OCI install/start/health/logs, restart, configure, stop/uninstall, persistent volume and backup restore")
    finally:
        if app_id:
            ids = subprocess.check_output(docker + ["ps", "-aq", "--filter", "label=io.psios.application=" + app_id], text=True).split()
            for cid in ids:
                subprocess.run(docker + ["rm", "-f", cid], check=True, stdout=subprocess.DEVNULL)
        if subprocess.run(docker + ["image", "inspect", tag], capture_output=True).returncode == 0:
            subprocess.run(docker + ["image", "rm", tag], check=True, stdout=subprocess.DEVNULL)
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default="target/debug")
    main(parser.parse_args().bin_dir)
