"""Collect real user journal records through scoped Tasks, including a format 33 upgrade."""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import time

from support import Failure, Node, key, target


def refuses(code, call):
    try:
        call()
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code}")


def main(previous):
    current = Path("target/debug").absolute()
    node = Node(previous or current)
    markers = ("scoped-first-" + key(), "scoped-second-" + key(), "outside-" + key())
    main_unit, other_unit = [f"psios-journal-{key()}.service" for _ in range(2)]
    service = f"service-{main_unit}"
    other = f"service-{other_unit}"
    try:
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": unit, "manager": "user"} for unit in (main_unit, other_unit)]
        node.config.write_text(json.dumps(config))
        node.start()
        fixed = node.get(service)["reference"]
        baseline = node.call("Node.Get", {})
        agent = node.call("Agent.Register", {"name": "log-reader"})
        owner, token = agent["principal_id"], agent["token"]
        grant = dict(principal_id=owner, units=24, ttl_seconds=120, tool_targets=[service])
        previous_scope = node.call("Grant.Issue", dict(grant, actions=["task.submit", "task.read", "service.inspect"]))
        if previous:
            node.stop(signal.SIGKILL)
            node.daemon[0] = str(current / "psiosd")
            before = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
            assert before["database_format"] == 33 and before["migration_required"], before
            node.start()
            assert node.get(service)["reference"] == fixed
            assert node.call("Node.Get", {})["reference"] == baseline["reference"]

        request = target(fixed)
        old_delegation = previous_scope["delegation_ref"]["resource_id"]
        refuses("PermissionDenied", lambda: node.prepare("service.logs", request))
        refuses("PermissionDenied", lambda: node.prepare("service.logs", request, token=token, delegation=old_delegation))
        actions = ["task.submit", "task.read", "service.logs"]
        refuses("InvalidArgument", lambda: node.call("Grant.Issue", dict(grant, actions=actions, tool_targets=None)))
        logged = node.call("Grant.Issue", dict(grant, actions=actions))
        delegation = logged["delegation_ref"]["resource_id"]

        def prepare(args):
            return node.prepare("service.logs", args, token=token, delegation=delegation)

        def submit(args):
            return node.call("Task.Submit", prepare(args), token)["task_id"]

        refuses("PermissionDenied", lambda: submit(target(node.get(other)["reference"])))
        refuses("Conflict", lambda: submit(dict(request, expected_resource_version="r0")))
        refuses("InvalidArgument", lambda: prepare(dict(request, limit=33)))
        refuses("InvalidArgument", lambda: prepare(dict(request, unit=other_unit)))

        def run(unit, *lines):
            subprocess.run([
                "systemd-run", "--user", "--quiet", "--wait", "--collect", f"--unit={unit}",
                "/bin/sh", "-c", 'printf "%s\\n" "$@"; sleep 0.2', "journal-fixture", *lines,
            ], timeout=15, check=True)

        run(main_unit, *markers[:2])
        run(other_unit, markers[2])

        def read(limit=16):
            task_id = submit(dict(request, limit=limit))
            task = node.wait(task_id)
            assert task["lifecycle"] == "Succeeded", task
            return node.call("Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, token)

        deadline = time.monotonic() + 6
        while True:
            result = read()
            values = [entry["message"]["data"] for entry in result["entries"]]
            if all(marker in values for marker in markers[:2]):
                break
            assert time.monotonic() < deadline, result
            time.sleep(0.2)
        assert markers[2] not in values, result
        assert result["service_ref"] == fixed and result["unit"] == main_unit and result["manager"] == "user"
        assert result["boot_id"] == node.call("Node.Get", {})["state"]["boot_id"], result
        assert all(int(entry["realtime_usec"]) > 0 and entry["journal_cursor"] for entry in result["entries"]), result
        assert node.get(service)["reference"] == fixed
        tail = read(1)["entries"]
        assert len(tail) == 1 and tail[0]["message"] == {"encoding": "Utf8", "data": markers[1]}, tail
        node.call("Grant.Revoke", target(logged["grant_ref"]))
        refuses("PermissionDenied", lambda: prepare(request))
        node.stop()
        after = json.loads(subprocess.check_output(node.daemon + ["verify"], timeout=15))
        assert after["database_format"] == 34 and not after["migration_required"], after
        print("PASS native service journal scope, tail, permission revocation and intact authority")
    finally:
        node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--previous-bin")
    main(parser.parse_args().previous_bin)
