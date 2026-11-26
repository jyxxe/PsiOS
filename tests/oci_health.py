"""Run approved engine-native probes in real offline-built scratch images."""
import json
import sqlite3
import subprocess
import time

from oci_health_signed import verify_signed_health
from support import key, target


PROBE = {"argv": ["/probe", "--health"], "interval_ms": 1000, "timeout_ms": 750, "retries": 2}


def build_image(docker, context, tag, command):
    (context / "Dockerfile").write_text("FROM scratch\nCOPY probe /probe\n"
        "HEALTHCHECK --interval=1s --timeout=750ms --retries=2 CMD " + command + "\n")
    subprocess.run(docker + ["build", "--network=none", "--pull=false", "--no-cache", "-t", tag, str(context)],
                   check=True, capture_output=True, timeout=90)
    return subprocess.check_output(docker + ["image", "inspect", "--format", "{{.Id}}", tag],
                                   text=True, timeout=15).strip()


def verify_image_health(node, docker):
    context = node.base / "probe-image"
    context.mkdir()
    source = context / "probe.c"
    source.write_text('''#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--health") == 0) {
        puts("private health probe output");
        return access("/data/ready", R_OK) != 0;
    }
    for (;;) pause();
}
''')
    subprocess.run(["cc", "-static", "-O2", str(source), "-o", str(context / "probe")], check=True, timeout=30)
    good = "psios-exec-health:" + key()
    shell = "psios-shell-health:" + key()
    app_id = None

    def observed(app, expected):
        deadline = time.monotonic() + 12
        while True:
            state = node.perform("application.health", target(app))
            if state["health"] == expected:
                return state
            assert time.monotonic() < deadline, state
            time.sleep(0.2)

    def settle(tool, data):
        return node.wait(node.submit(tool, data), autoapprove=True)

    try:
        image = build_image(docker, context, good, '["/probe", "--health"]')
        unsupported = build_image(docker, context, shell, "/probe --health")
        volume = node.perform("volume.create", {"name": "image-health-fixture"})
        spec = {"image_id": image, "name": "image-health", "release_version": "1.0.0",
                "argv": ["/probe"], "volume": target(volume), "license_expression": "0BSD",
                "cpu_millicores": 200, "memory_max_bytes": 33554432, "pids_max": 8, "ephemeral_max_bytes": 1048576}
        for params in ({**spec, "image_id": unsupported, "health_check": PROBE},
                       {**spec, "health_check": {**PROBE, "argv": ["/probe", "--other"]}}):
            failure = settle("application.install", params)
            assert (failure["lifecycle"], failure["conditions"][0]["type"]) == ("Failed", "UnsupportedCapability"), failure

        pending = node.submit("application.install", {**spec, "health_check": PROBE})
        approval = node.get(node.wait(pending)["conditions"][0]["message"])["value"]
        plan = node.call("Resource.Resolve", {"reference": approval["plan_ref"]})["value"]
        purpose = node.call("Resource.Resolve", {"reference": plan["purpose_ref"]})["value"]
        assert purpose["health_check"] == PROBE and purpose["image_id"] == image, purpose
        completed = node.wait(pending, autoapprove=True)
        assert completed["lifecycle"] == "Succeeded", completed
        app = node.call("Artifact.Read", {"reference": completed["result"]["artifact_refs"][0]})
        app_id = app["resource_id"]
        release = node.get(app_id)["resource"]["spec"]["release_ref"]
        manifest = node.call("Resource.Resolve", {"reference": release})["value"]
        profile = node.call("Resource.Resolve", {"reference": manifest["health_check_ref"]})["value"]
        assert profile["probe"] == PROBE and profile["config_digest"] == image, profile
        profile_id = manifest["health_check_ref"]["resource_id"]
        database = node.state / "state.db"
        node.stop()
        with sqlite3.connect(database, timeout=10) as db:
            original, = db.execute("SELECT body FROM versions WHERE id=? AND rev=1", (profile_id,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.probe.interval_ms',2000) WHERE id=? AND rev=1",
                       (profile_id,))
        try:
            bad = subprocess.run(node.daemon + ["verify"], capture_output=True, timeout=20)
            assert bad.returncode and b"reference digest mismatch" in bad.stderr, bad
            node.start()
            blocked = settle("application.start", target(app))
            assert blocked["lifecycle"] == "Failed" and blocked["conditions"][0]["type"] in {
                "RecoveryRequired", "PreconditionFailed"}, blocked
            assert not subprocess.check_output(docker + ["ps", "-aq", "--filter", f"label=io.psios.application={app_id}"], timeout=15).strip()
        finally:
            node.stop()
            with sqlite3.connect(database, timeout=10) as db:
                db.execute("UPDATE versions SET body=? WHERE id=? AND rev=1", (original, profile_id))
        node.start()
        ready = node.state / "volumes" / volume["resource_id"] / "data" / "ready"
        ready.write_text("ready\n")
        app = node.perform("application.start", target(app))
        healthy = observed(app, "Healthy")
        assert healthy["readiness"] == "Ready" and healthy["process_running"], healthy
        assert "private health probe output" not in json.dumps(healthy), healthy
        container = healthy["container_id"]
        runtime, = json.loads(subprocess.check_output(docker + ["inspect", container], timeout=15))
        assert runtime["Config"]["Healthcheck"]["Test"] == ["CMD", *PROBE["argv"]], runtime["Config"]
        assert runtime["HostConfig"]["NetworkMode"] == "none" and runtime["HostConfig"]["ReadonlyRootfs"]

        ready.unlink()
        unhealthy = observed(app, "Unhealthy")
        assert unhealthy["readiness"] == "NotReady" and unhealthy["process_running"], unhealthy
        app = node.perform("application.refresh", target(app))
        state = node.get(app["resource_id"])["value"]
        assert (state["health"], state["readiness"]) == ("Unhealthy", "NotReady"), state
        grant = node.call("Grant.Issue", {"principal_id": "local-owner", "units": 4, "ttl_seconds": 90,
            "actions": ["application.inspect_instance", "task.submit", "task.read"],
            "tool_targets": [app["resource_id"], volume["resource_id"]]})
        prepare = node.prepare("application.inspect_instance", target(app),
                               delegation=grant["delegation_ref"]["resource_id"])
        sampled = node.wait(node.call("Task.Submit", prepare)["task_id"])
        assert sampled["lifecycle"] == "Succeeded", sampled
        instance = node.call("Artifact.Read", {"reference": sampled["result"]["artifact_refs"][0]})
        assert instance["container_id"] == container and instance["health"] == "Unhealthy", instance
        app = node.perform("application.stop", target(app))

        # The same digest on a new release without opt-in must disable its image probe.
        staged = {k: v for k, v in spec.items() if k != "volume"}
        staged["release_version"] = "2.0.0"
        disabled = node.perform("application.stage_release", {"target": target(app), "release": staged})
        app = node.perform("application.select_release", {"target": target(app), "release_ref": disabled})
        app = node.perform("application.start", target(app))
        unconfigured = node.perform("application.health", target(app))
        assert (unconfigured["health"], unconfigured["readiness"]) == ("Unknown", "Unknown"), unconfigured
        inspect, = json.loads(subprocess.check_output(docker + ["inspect", unconfigured["container_id"]], timeout=15))
        assert inspect["Config"]["Healthcheck"]["Test"] == ["NONE"] and not inspect["State"].get("Health"), inspect
        app = node.perform("application.stop", target(app))
        node.perform("application.uninstall", target(app))
        verify_signed_health(node, docker, spec, PROBE)
        node.stop()
        subprocess.run(node.daemon + ["verify"], check=True, capture_output=True, timeout=20)
        node.start()
        print("PASS real OCI exec probes, health transitions, release scope, shell refusal and disabled default", flush=True)
    finally:
        if app_id:
            containers = subprocess.check_output(docker + ["ps", "-aq", "--filter", f"label=io.psios.application={app_id}"],
                                                 text=True, timeout=15).split()
            if containers:
                subprocess.run(docker + ["rm", "-f", *containers], capture_output=True, timeout=20)
        subprocess.run(docker + ["image", "rm", good, shell], capture_output=True, timeout=20)
