"""Observe actual native USB hotplug through authorized, paginated Core Tasks."""
import argparse
from contextlib import contextmanager
import json
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlsplit

from native_disk import REPO, boot, call, check_clean_shutdown, password, sha, stage_guest_token
from native_upgrade import shutdown
from test_mcp_tasks import LEGACY, PROFILE, task_params


@contextmanager
def monitor(path):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(15)
        connection.connect(str(path))
        with connection.makefile("rb") as incoming:
            def receive():
                line = incoming.readline(64 * 1024)
                assert line.endswith(b"\n"), "incomplete QMP message"
                return json.loads(line)

            def execute(name, **arguments):
                request = {"execute": name, "arguments": arguments, "id": name}
                connection.sendall(json.dumps(request).encode("ascii") + b"\r\n")
                for _ in range(64):
                    message = receive()
                    if "event" in message:
                        continue
                    assert message == {"return": {}, "id": name}, message
                    return
                raise AssertionError(f"no QMP response for {name}")

            assert "QMP" in receive()
            execute("qmp_capabilities")
            yield execute


def observe(console, key, delegation, parameters, *, error=None):
    payload = shlex.quote(json.dumps(parameters, separators=(",", ":")))
    enrolled = shlex.quote(delegation)
    accepted = json.loads(console.run(f"printf '%s' {payload} | psiosctl submit node.usb --key {key} --delegation {enrolled}"))
    outcome = call(console, "Task.Wait", {"id": accepted["task_id"], "timeout_ms": 5000})
    if error:
        assert outcome["lifecycle"] == "Failed" and outcome["conditions"][0]["type"] == error, outcome
        return None
    assert outcome["lifecycle"] == "Succeeded", outcome
    reference, = outcome["result"]["artifact_refs"]
    return reference, call(console, "Artifact.Read", {"reference": reference})


def tablet(console):
    return console.run("for i in $(seq 1 100); do for entry in /sys/bus/usb/devices/*; do "
                       'test -r "$entry/idVendor" && test "$(cat "$entry/idVendor")" = 0627 && '
                       'printf "%s\\n" "${entry##*/}" && exit 0; done; sleep 0.1; done; exit 1')


def mcp_observation(console, node, expected):
    agent = call(console, "Agent.Register", {"name": "native-usb-stdio", "peer_uid": 1000})
    grant = call(console, "Grant.Issue", {"principal_id": agent["principal_id"], "tool_targets": [node],
                 "actions": ["node.usb", "task.submit", "task.read"], "units": 6, "ttl_seconds": 300})
    token = stage_guest_token(console, agent["token"])
    options = (f"/bin/psios-mcp --socket /run/psios/api.sock --service-uid 1000 --token-file {token} "
               f"--delegation {shlex.quote(grant['delegation_ref']['resource_id'])}")

    def exchange(method, parameters, protocol, *, extra=""):
        frames = []
        if protocol == LEGACY:
            frames.extend([
                {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": LEGACY,
                    "capabilities": {}, "clientInfo": {"name": "native-usb-test", "version": "1"}}},
                {"jsonrpc": "2.0", "method": "notifications/initialized"},
            ])
        frames.append({"jsonrpc": "2.0", "id": 2, "method": method, "params": parameters})
        payload = " ".join(shlex.quote(json.dumps(frame, separators=(",", ":"))) for frame in frames)
        frames_expected = 2 if protocol == LEGACY else 1
        # Keep the request FIFO open while the modern async worker replies.
        receive = (
            'work=$(mktemp -d /tmp/psios-usb-mcp.XXXXXX) && '
            'trap \'rm -rf -- "$work"\' EXIT && mkfifo "$work/in" "$work/out" || exit 1; '
            f'{options} --protocol {protocol} {extra} < "$work/in" > "$work/out" & worker=$!; '
            f'exec 3>"$work/in" 4<"$work/out"; printf \'%s\\n\' {payload} >&3; '
            f'for i in $(seq 1 {frames_expected}); do IFS= read -r -t 15 reply <&4 || exit 1; '
            'printf \'%s\\n\' "$reply"; done; exec 3>&- 4<&-; wait "$worker"'
        )
        reports = [json.loads(line) for line in console.run(receive).splitlines()]
        if protocol == LEGACY:
            assert reports[0]["id"] == 1 and reports[0]["result"]["protocolVersion"] == LEGACY, reports
        reply = reports[-1]
        assert reply["id"] == 2 and "result" in reply, reply
        return reply["result"]

    usb = "--tool node.usb"
    assert exchange("tools/list", {}, LEGACY)["tools"] == []
    descriptor, = exchange("tools/list", {}, LEGACY, extra=usb)["tools"]
    assert descriptor["name"] == "node.usb" and descriptor["annotations"]["readOnlyHint"]
    assert descriptor["inputSchema"]["properties"]["cursor"]["properties"]["offset"]["maximum"] == 255
    request = {"name": "node.usb", "arguments": {"limit": 1}}
    synchronous = exchange("tools/call", request, LEGACY, extra=usb)["structuredContent"]["result"]
    assert synchronous["devices"] == [expected] and synchronous["next_cursor"], synchronous

    modern = f"{usb} --tasks-endpoint native-usb --tasks-ttl-seconds 300"
    handle = exchange("tools/call", task_params(request, invocation="native-usb-inventory"), PROFILE, extra=modern)["taskId"]

    def read():
        cutoff = time.monotonic() + 20
        while time.monotonic() < cutoff:
            view = exchange("tasks/get", task_params({"taskId": handle}), PROFILE, extra=modern)
            if view["status"] == "completed":
                return view["result"]["structuredContent"]
            assert view["status"] == "working", view
        raise AssertionError("USB MCP Task did not complete")

    saved = read()
    assert saved["result"]["devices"] == [expected]
    assert saved["result"]["snapshot_digest"] == synchronous["snapshot_digest"]

    def verify_after_hotplug():
        assert read() == saved
        stale = {"name": "node.usb", "arguments": {"limit": 1, "cursor": synchronous["next_cursor"]}}
        result = exchange("tools/call", stale, LEGACY, extra=usb)["structuredContent"]
        assert result["code"] == "ResyncRequired", result
        console.run("rm " + token)

    return verify_after_hotplug


def shell_observation(console, node, attached):
    def grant(action):
        return call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                    "actions": [action, "task.submit", "task.read"], "units": 8, "ttl_seconds": 300})

    usb = grant("node.usb")
    pci = grant("node.pci")
    usb_id = usb["delegation_ref"]["resource_id"]
    pci_id = pci["delegation_ref"]["resource_id"]
    launch = shlex.join(["psiosctl", "shell", "--usb-delegation", usb_id, "--pci-delegation", pci_id])
    info = console.run(
        'work=$(mktemp -d /tmp/psios-usb-shell.XXXXXX) || exit 1; '
        f'{launch} </dev/null > "$work/session" 2> "$work/error" & pid=$!; '
        'for i in $(seq 1 100); do '
        'if [ "$(wc -l < "$work/session")" -ge 2 ]; then '
        'printf "%s\\n" "$work" "$pid"; head -n 2 "$work/session"; exit 0; fi; '
        'kill -0 "$pid" || exit 1; sleep 0.1; done; exit 1'
    ).splitlines()
    workdir, pid, address, pairing = info
    origin = urlsplit(address)
    assert origin.scheme == "http" and origin.hostname == "127.0.0.1" and origin.port
    assert pairing.startswith("Pairing code: ") and pid.isascii() and pid.isdecimal()
    assert workdir.startswith("/tmp/psios-usb-shell.")

    def request(path, payload, bearer=None):
        data = json.dumps(payload, separators=(",", ":"))
        headers = [f"POST {path} HTTP/1.1", f"Host: {origin.netloc}", f"Origin: {address}",
                   "Content-Type: application/json", f"Content-Length: {len(data)}", "Connection: close"]
        if bearer is not None:
            headers.append(f"Authorization: Bearer {bearer}")
        arguments = " ".join(shlex.quote(header) for header in (*headers, ""))
        fifo = shlex.quote(workdir + "/request")
        # Keep the write side open until the Shell finishes its Core Task.
        response = console.run(
            f"mkfifo {fifo} || exit 1; busybox nc -w 8 127.0.0.1 {origin.port} < {fifo} & network=$!; "
            f"exec 3>{fifo}; printf '%s\\r\\n' {arguments} >&3; printf '%s' {shlex.quote(data)} >&3; "
            f'wait "$network" || exit 1; exec 3>&-; rm {fifo}'
        )
        head, _, body = response.partition("\n\n")
        assert head.startswith("HTTP/1.1 ") and body, "native Shell did not return a complete HTTP response"
        return int(head.splitlines()[0].split()[1]), json.loads(body)

    status, paired = request("/api/session", {"code": pairing.removeprefix("Pairing code: ")})
    assert status == 200 and paired["observe_usb"] and paired["observe_pci"]
    token = paired["token"]
    route = "/api/node-usb"
    status, page = request(route, {}, token)
    assert status == 200, page
    first = next(device for device in page["devices"] if device["location"] == attached["location"])
    for key in ("vendor_id", "product_id", "bus_number", "device_address", "authorized"):
        assert first[key] == attached[key], (key, first, attached)
    assert "serial" not in first and "mount_namespace_inode" not in page
    _, previous = observe(console, "native-shell-usb-anchor", usb_id, {"limit": 1})
    cursor = previous["next_cursor"]
    status, remaining = request(route, {"cursor": cursor}, token)
    assert status == 200 and remaining["devices"] and first not in remaining["devices"], remaining
    assert request("/api/node-pci", {}, token)[0] == 200

    def verify_after_hotplug(current):
        status, stale = request(route, {"cursor": cursor}, token)
        assert status == 409 and stale["error"]["code"] == "ResyncRequired", stale
        status, refreshed = request(route, {}, token)
        assert status == 200, refreshed
        fresh = next(device for device in refreshed["devices"] if device["location"] == first["location"])
        assert fresh["device_address"] == current["device_address"] != first["device_address"], fresh
        ref = usb["grant_ref"]
        call(console, "Grant.Revoke", {"id": ref["resource_id"], "expected_resource_version": ref["version"]})
        status, denied = request(route, {}, token)
        assert status == 403 and denied["error"]["code"] == "PermissionDenied", denied
        assert request("/api/node-pci", {}, token)[0] == 200
        assert request("/api/logout", {}, token)[0] == 200
        console.run(f"rm -r -- {shlex.quote(workdir)}")

    return verify_after_hotplug


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    for source, target in (("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / target)
    disk = work / "native.raw"
    subprocess.run([sys.executable, "-B", args.installer, disk, "--kernel-iso", args.kernel_iso,
                    "--core-archive", args.archive, "--core-sha256", sha(args.archive),
                    "--console-hash", operator_hash, "--root-console-hash", admin_hash],
                   check=True, stdout=subprocess.DEVNULL, timeout=180)

    with boot(work, disk, pace_commands=True, usb_control=True) as console:
        console.login("root", admin)
        console.run("modprobe xhci_pci && test -d /sys/bus/usb/devices")
        console.send("exit")
        console.login("psios", operator)
        missing = console.run("printf '{}' | psiosctl submit node.usb --key usb-without-grant 2>&1; test $? -ne 0")
        assert "PermissionDenied" in missing, missing
        node = call(console, "Node.Get", {})["reference"]["resource_id"]

        def grant(units):
            issued = call(console, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                          "units": units, "ttl_seconds": 300, "actions": ["node.usb", "task.submit", "task.read"]})
            return issued["delegation_ref"]["resource_id"]

        delegation = grant(12)
        fixture, before = observe(console, "usb-before", delegation, {"limit": 1})
        cursor = before["next_cursor"]
        assert cursor and before["devices"][0]["location"].startswith("usb"), before
        _, settled = observe(console, "usb-same", delegation, {"limit": 1, "cursor": cursor})
        assert settled["snapshot_digest"] == before["snapshot_digest"], settled
        observe(console, "usb-other-grant", grant(2), {"limit": 1, "cursor": cursor}, error="ResyncRequired")

        with monitor(work / "usb-control.sock") as qmp:
            qmp("device_add", driver="usb-tablet", id="tablet-first", bus="psios-usb.0", port="1")
            location = tablet(console)
            observe(console, "usb-hotplug-old", delegation, {"limit": 1, "cursor": cursor}, error="ResyncRequired")
            retained, inserted = observe(console, "usb-inserted", delegation, {"limit": 1})
            attached = inserted["devices"][0]
            assert attached["location"] == location and attached["vendor_id"] == "0627" and attached["authorized"], inserted
            assert inserted["boot_id"] == before["boot_id"] and inserted["next_cursor"], inserted
            verify_mcp = mcp_observation(console, node, attached)
            verify_shell = shell_observation(console, node, attached)
            qmp("device_del", id="tablet-first")
            console.run(f"for i in $(seq 1 100); do test ! -e /sys/bus/usb/devices/{location} && exit 0; "
                        "sleep 0.1; done; exit 1")
            qmp("device_add", driver="usb-tablet", id="tablet-replacement", bus="psios-usb.0", port="1")
            assert tablet(console) == location
            observe(console, "usb-replaced-old", delegation, {"limit": 1, "cursor": inserted["next_cursor"]},
                    error="ResyncRequired")
            _, replacement = observe(console, "usb-replacement", delegation, {"limit": 1})
            current, = replacement["devices"]
            for key in ("location", "parent_location", "vendor_id", "product_id", "bus_number"):
                assert current[key] == attached[key], (key, attached, current)
            assert current["device_address"] != attached["device_address"], (attached, current)
            assert "serial" not in current and "sysfs_identity" not in current
            verify_mcp()
            verify_shell(current)
        assert call(console, "Artifact.Read", {"reference": fixture}) == before
        assert call(console, "Artifact.Read", {"reference": retained}) == inserted
        shutdown(console, admin)
    check_clean_shutdown(disk)
    print("PASS observed real USB root hubs, USB attachment, unchanged slot replacement, cursor invalidation and durable artifacts")
    print("PASS Shell USB pages track the same device, invalidate stale cursors and retain separate PCI authority")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for flag in ("--kernel-iso", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-usb-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
