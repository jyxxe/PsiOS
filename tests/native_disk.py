"""Boot a generated disk from OVMF, use its local console and recover after power loss."""
import argparse
import base64
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import hmac
import importlib.util
import json
import os
import re
from pathlib import Path
import secrets
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]


def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


class Console:
    def __init__(self, vm, *, pace_commands=False):
        self.vm = vm
        self.buf = b""
        self.pace_commands = pace_commands

    def _serial_failure(self, reason, phase, received):
        status = self.vm.poll()
        qemu = "running" if status is None else f"exited({status})"
        return AssertionError(f"guest serial {reason} at {phase} "
                              f"(QEMU={qemu}, received_bytes={received}, buffered_bytes={len(self.buf)})")

    def until(self, marker, timeout=75, reject=None, *, phase="marker"):
        end = time.monotonic() + timeout
        received = 0
        while True:
            position = self.buf.find(marker)
            if reject is not None:
                failure = self.buf.find(reject)
                if failure >= 0 and (position < 0 or failure < position):
                    raise AssertionError("guest serial console rejected an expected valid login")
            if position >= 0:
                break
            remaining = end - time.monotonic()
            if remaining <= 0 or not select.select([self.vm.stdout], [], [], remaining)[0]:
                raise self._serial_failure("timed out", phase, received)
            chunk = os.read(self.vm.stdout.fileno(), 8192)
            if not chunk:
                raise self._serial_failure("reached EOF", phase, received)
            received += len(chunk)
            self.buf += chunk.replace(b"\r", b"")
            if len(self.buf) > 1024 * 1024:
                raise self._serial_failure("exceeded its output limit", phase, received)
        data, _, self.buf = self.buf.partition(marker)
        return data

    def _write(self, data):
        self.vm.stdin.write(data)
        self.vm.stdin.flush()

    def send(self, line, *, paced=False):
        data = line.encode()
        if paced:
            # Keep bursts smaller than the emulated UART FIFO.
            for start in range(0, len(data), 8):
                self._write(data[start:start + 8])
                time.sleep(0.01)
            data = b""
        self._write(data + b"\r")

    def send_secret(self, secret):
        # Give BusyBox time to switch off echo after printing the prompt.
        time.sleep(0.03)
        self.send(secret, paced=True)

    def credentials(self, user, password):
        self.until(b"login: ", phase="login prompt")
        self.send(user)
        self.until(b"Password: ", phase="password prompt")
        self.send_secret(password)

    def login(self, user, password):
        self.credentials(user, password)
        before_shell = self.until(b"# " if user == "root" else b"$ ", reject=b"Login incorrect",
                                  phase="authenticated shell prompt")
        if password.encode() in before_shell:
            raise AssertionError("guest serial console echoed the fixture password")
        marker = "PSIOSLOGIN" + secrets.token_hex(6)
        self.send(f"stty -echo; export PS1=''; printf '\\n{marker}\\n'", paced=True)
        self.until(("\n" + marker + "\n").encode(), phase="shell login nonce")
        if user == "psios":
            self.run("for i in $(seq 1 100); do psiosctl whoami >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1")

    def run(self, command):
        marker = "PSIOSCMD" + secrets.token_hex(6)
        self.send(f"printf '\\n{marker}\\n'; ( {command} ); r=$?; printf '\\n{marker}:%s\\n' \"$r\"",
                  paced=self.pace_commands)
        self.until(("\n" + marker + "\n").encode(), phase="shell command start nonce")
        output = self.until(("\n" + marker + ":").encode(), phase="shell command result nonce")
        status = self.until(b"\n", phase="shell command exit status")
        if status != b"0":
            if self.pace_commands:
                raise AssertionError(f"paced guest command failed (result_bytes={len(output)}, "
                                     f"exit_status_bytes={len(status)})")
            raise AssertionError(f"guest command {command!r} exited {status!r}: {output[-512:]!r}")
        return output.decode().strip()


@contextmanager
def boot(work, image, network_mac=None, extra_disk=None, tap=None, *, rtc_base=None,
         disk_format="raw", vars_file="vars.fd", vcpus=2, pace_commands=False,
         disk_bus="virtio", extra_bus="virtio", network_model="virtio-net-pci",
         ipv6_network=None, ssh_port=None, usb_control=False):
    # The privileged TAP fixture has a reproducible serial stall on hosted KVM.
    # Use a portable CPU emulator for that fixture; keep its real virtio and TAP.
    accel = "kvm" if tap is None and os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    cmd = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35",
           "-accel", accel, "-m", "768", "-smp", str(vcpus), "-display", "none",
           "-monitor", "none", "-serial", "stdio",
           "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
           "-drive", f"if=pflash,format=raw,unit=1,file={vars_file}"]
    if rtc_base is not None:
        cmd += ["-rtc", f"base={rtc_base},clock=vm"]
    if usb_control:
        cmd += ["-device", "qemu-xhci,id=psios-usb", "-qmp", "unix:usb-control.sock,server=on,wait=off"]
    if extra_disk is not None or disk_bus != "virtio":
        cmd += ["-boot", "strict=on"]
        if extra_disk is not None:
            cmd += ["-drive", f"if=none,format=raw,id=additional,file={extra_disk.name}",
                    "-device", disk_device(extra_bus, "additional")]
        cmd += ["-drive", f"if=none,format={disk_format},id=installed,file={image.resolve()}",
                "-device", disk_device(disk_bus, "installed") + ",bootindex=1"]
    else:
        cmd += ["-boot", "order=c,strict=on", "-drive", f"if=virtio,format={disk_format},file={image.resolve()}"]
    if network_mac:
        network = (f"tap,id=network,ifname={tap},script=no,downscript=no" if tap
                   else "user,id=network,net=192.0.2.0/24,dhcpstart=192.0.2.20")
        if ipv6_network is not None:
            network += f",ipv6-net={ipv6_network}"
        if ssh_port is not None:
            network += f",hostfwd=tcp:127.0.0.1:{ssh_port}-192.0.2.20:22"
        cmd += ["-netdev", network,
                "-device", f"{network_model},netdev=network,mac={network_mac},romfile="]
    else:
        cmd += ["-nic", "none"]
    with subprocess.Popen(cmd, cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as vm:
        try:
            # TAP tests use TCG and may run under loaded CI hosts. Pace complete
            # commands, including the longer route and configuration requests.
            yield Console(vm, pace_commands=pace_commands or tap is not None)
        finally:
            if vm.poll() is None:
                vm.kill()
            vm.wait(15)


def disk_device(bus, name):
    return {"virtio": f"virtio-blk-pci,drive={name}", "nvme": f"nvme,drive={name},serial=psios-{name}",
            "ahci": f"ide-hd,bus=ide.{0 if name == 'installed' else 1},drive={name}"}[bus]


def password(work, name):
    secret = secrets.token_urlsafe(26)
    path = work / name
    with path.open("xb") as f:
        os.fchmod(f.fileno(), 0o600)
        subprocess.run(["openssl", "passwd", "-6", "-stdin"], input=(secret + "\n").encode(), stdout=f, check=True)
    return secret, path


def check_state(c):
    uid = c.run("id -u")
    assert uid == "1000", uid
    assert c.run(". /etc/os-release; echo \"$ID\"") == "psios"
    assert c.run("awk '/^Uid:/ {print $2}' /proc/$(pidof psiosd)/status") == "1000"
    c.run("test -d /sys/firmware/efi && test -d /sys/class/net/lo || exit 1; "
          "for net in /sys/class/net/*; do test ! -e \"$net/device\" || exit 1; done")
    who = json.loads(c.run("psiosctl whoami"))
    assert who["principal_id"] == "local-owner"
    submitted = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-power-cut"))
    result = json.loads(c.run("psiosctl wait " + submitted["task_id"]))
    assert result["lifecycle"] == "Succeeded"
    return who["authority_id"], submitted, result["result"], c.run("cat /proc/sys/kernel/random/boot_id")


def call(c, method, params, token_file=None):
    body = shlex.quote(json.dumps(params))
    options = f" --token-file {shlex.quote(token_file)}" if token_file else ""
    return json.loads(c.run(f"printf '%s' {body} | psiosctl{options} call {method}"))


def check_resource_watch(c):
    node = call(c, "Node.Get", {})["reference"]
    identity = shlex.quote(node["resource_id"])
    snapshot = json.loads(c.run(f"psiosctl list Node --id {identity}"))
    assert snapshot["items"] == [node] and snapshot["complete"], snapshot
    paused = call(c, "Node.Configure", {"expected_resource_version": node["version"], "scheduling": "Cordoned"})
    other = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-scoped-event"))
    call(c, "Task.Cancel", {"id": other["task_id"], "expected_resource_version": other["task_revision"]})
    enabled = call(c, "Node.Configure", {"expected_resource_version": paused["version"], "scheduling": "Enabled"})
    cursor = shlex.quote(snapshot["watch_start_cursor"])
    events = json.loads(c.run(f"psiosctl watch {cursor} --wait-ms 5000"))["events"]
    assert [event["resource_ref"] for event in events] == [paused, enabled], events


def stage_guest_token(c, secret):
    path = "/tmp/psios-agent-fixture.token"
    try:
        c.run(f"umask 077; printf '%s' {shlex.quote(secret)} > {path}")
    except AssertionError:
        raise AssertionError("guest could not stage the private agent test credential") from None
    return path


def check_native_mcp(c):
    agent = call(c, "Agent.Register", {"name": "native-stdio", "peer_uid": 1000})
    grant = call(c, "Grant.Issue", {
        "principal_id": agent["principal_id"], "actions": ["task.submit", "task.read", "system.status"],
        "tool_targets": ["profile-local"], "units": 1, "ttl_seconds": 120,
    })
    token = stage_guest_token(c, agent["token"])
    frames = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "guest", "version": "1"}}},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "system.status", "arguments": {}}},
    ]
    arguments = " ".join(shlex.quote(json.dumps(frame, separators=(",", ":"))) for frame in frames)
    command = (f"printf '%s\\n' {arguments} | /bin/psios-mcp --socket /run/psios/api.sock --service-uid 1000 "
               f"--token-file {token} --delegation {shlex.quote(grant['delegation_ref']['resource_id'])}")
    initialized, reply = map(json.loads, c.run(command).splitlines())
    assert initialized["id"] == 1 and initialized["result"]["protocolVersion"] == "2025-11-25"
    assert reply["id"] == 2 and not reply["result"]["isError"], reply
    receipt = reply["result"]["structuredContent"]
    task = call(c, "Task.Get", {"id": receipt["task_id"]}, token)
    artifact, = task["result"]["artifact_refs"]
    assert task["lifecycle"] == "Succeeded" and receipt["result"]["kernel_release"] == c.run("uname -r")
    assert call(c, "Artifact.Read", {"reference": artifact}, token) == receipt["result"]
    c.run("rm " + token)


def check_native_chat(c):
    agent = call(c, "Agent.Register", {"name": "native-conversation-peer"})
    call(c, "Grant.Issue", dict(principal_id="local-owner", units=1, ttl_seconds=600,
                               actions=["chat.invite", "chat.read", "chat.send"]))
    call(c, "Grant.Issue", dict(principal_id=agent["principal_id"], units=1, ttl_seconds=600, actions=["chat.read"]))
    token = stage_guest_token(c, agent["token"])
    invite = call(c, "Conversation.OpenDirect", dict(key="native-handoff", name="serial handoff",
                  recipient_principal_id=agent["principal_id"], ttl_seconds=60))
    room = call(c, "Conversation.Respond", dict(id=invite["id"], accept=True,
                expected_resource_version=invite["resource_version"]), token)
    window = call(c, "Message.OpenSendWindow", dict(id=room["id"], ttl_seconds=540))
    prepared = call(c, "Message.Prepare", dict(conversation_id=room["id"],
                    client_send_id=window["namespace"] + ":original", text="offline handoff"))
    receipt = call(c, "Message.Send", prepared)
    history = call(c, "Message.List", {"id": room["id"]}, token)
    entry, = history["items"]
    assert receipt["message_id"] == entry["message"]["message_id"]
    assert history["read_sequence"] == "0"
    assert call(c, "Message.AckRead", dict(id=room["id"], cursor=history["cursor"]), token) == {"read_sequence": "1"}
    c.run("rm " + token)
    return room["id"], agent["token"], entry, prepared, receipt


def check_recovered_chat(c, observed):
    conversation, secret, entry, original_request, receipt = observed
    token = stage_guest_token(c, secret)
    history = call(c, "Message.List", {"id": conversation}, token)
    assert history["items"] == [entry] and history["read_sequence"] == "1"
    assert call(c, "Message.Send", original_request) == dict(receipt, deduplicated=True)
    c.run("rm " + token)


def check_native_workflow(c):
    node = json.loads(c.run("psiosctl node"))["reference"]["resource_id"]
    key = "native-workflow-fixture"
    secret = call(c, "Secret.Put", {"name": "native-workflow-key", "value": key})["resource_id"]
    grant = call(c, "Grant.Issue", {
        "principal_id": "local-owner", "tool_targets": ["profile-local", node, secret], "units": 5, "ttl_seconds": 120,
        "actions": ["workflow.manage", "task.submit", "task.read", "system.status", "node.inventory", "secret.mac"],
    })
    definition = call(c, "Workflow.Define", {
        "key": "native-offline-workflow",
        "steps": [
            {"id": "inventory", "tool": "node.inventory", "after": ["status"], "input": {},
             "when": {"step": "status", "pointer": "/authority_id", "equals": node}},
            {"id": "status", "tool": "system.status", "after": [], "input": {}},
            {"id": "unselected", "tool": "node.inventory", "after": ["status"], "input": {},
             "when": {"step": "status", "pointer": "/authority_id", "equals": "other-node"}},
            {"id": "join", "tool": "system.status", "after": [], "after_selected": ["inventory", "unselected"], "input": {}},
            {"id": "seal", "tool": "secret.mac", "after": ["join"], "input": {"secret_id": secret, "message": ""},
             "inputs_from": {"message": {"step": "join", "pointer": "/authority_id"}}},
        ],
    })
    assert definition["definition"]["dsl_version"] == 3
    stamp = int(c.run("date -u +%s"))
    run = call(c, "Workflow.Start", {
        "key": "native-offline-workflow-run", "definition_ref": definition["reference"],
        "delegation_id": grant["delegation_ref"]["resource_id"],
        "deadline_at": datetime.fromtimestamp(stamp + 60, timezone.utc).isoformat(),
    })
    identity = {"id": run["reference"]["resource_id"]}
    deadline = time.monotonic() + 15
    while True:
        observed = call(c, "Workflow.Get", identity)
        phase = observed["state"]["lifecycle"]
        if phase in {"Succeeded", "Failed", "Cancelled"}:
            break
        assert time.monotonic() < deadline, observed
        time.sleep(0.15)
    assert (phase, observed["state"]["outcome_knowledge"]) == ("Succeeded", "Known"), observed
    inventory, status, unselected, join, seal = observed["state"]["steps"]
    assert (inventory["lifecycle"], status["lifecycle"]) == ("Succeeded", "Succeeded")
    assert inventory["condition"]["selected"] is True and inventory["condition"]["source_task_ref"] == status["task_ref"]
    assert unselected["lifecycle"] == "Skipped" and unselected["task_receipt"] is None
    assert join["lifecycle"] == "Succeeded"
    assert join["joined_predecessors"] == [{"step_id": "inventory", "task_ref": inventory["task_ref"]}]
    receipt = inventory["task_receipt"]
    assert receipt["origin_ref"]["origin_id"] == identity["id"]
    task = call(c, "Task.Get", {"id": receipt["task_id"]})
    artifact, = task["result"]["artifact_refs"]
    report = call(c, "Artifact.Read", {"reference": artifact})
    assert report["memory_bytes"]["MemTotal"] > 0, report
    binding = seal["input_binding"]
    assert binding["sources"][0]["source_task_ref"] == join["task_ref"]
    assert seal["task_receipt"]["request_digest"] == binding["request_digest"]
    sealed = call(c, "Task.Get", {"id": seal["task_receipt"]["task_id"]})
    mac_artifact, = sealed["result"]["artifact_refs"]
    result = call(c, "Artifact.Read", {"reference": mac_artifact})
    assert base64.b64decode(result["tag"]) == hmac.digest(key.encode(), node.encode(), "sha256")
    return observed


def hold_terminal(console):
    console.run("test ! -e /dev/pts/0 || exit 1; "
                "(trap '' TERM HUP; exec 8<>/dev/ptmx && exec sleep 300) </dev/null >/dev/null 2>&1 & "
                "for i in $(seq 1 50); do test ! -c /dev/pts/0 || exit 0; sleep 0.1; done; exit 1")
    assert console.run("stat -c '%u:%a' /dev/pts/0") == console.run("id -u") + ":600"


def check_machine_identity(c):
    identity = c.run("cat /etc/machine-id")
    assert re.fullmatch(r"[0-9a-f]{32}", identity) and identity != "0" * 32
    assert c.run("stat -c '%u:%g:%a:%h' /etc/machine-id") == "0:0:444:1"
    c.run("test ! -e /etc/.psios-machine-id.pending")
    return identity


def check_temporary_storage(c):
    assert c.run("stat -c '%u:%g:%a' /tmp") == "0:0:1777"
    mount = c.run("awk '$5 == \"/tmp\" {for (i=7; i<NF; i++) "
                  "if ($i == \"-\") print $6, $(i+1), $(i+3)}' /proc/self/mountinfo")
    assert mount, "missing dedicated /tmp mount"
    flags, filesystem, limits = mount.split()
    assert filesystem == "tmpfs" and {"rw", "nodev", "nosuid"} <= set(flags.split(",")), mount
    assert {"size=32768k", "nr_inodes=4096"} <= set(limits.split(",")), mount
    c.run("test -f /tmp/psios-root-scratch && ! rm -f /tmp/psios-root-scratch 2>/dev/null && "
          "test -f /tmp/psios-root-scratch")
    assert c.run("tmp=$(mktemp) && stat -c '%u:%a' \"$tmp\" && rm \"$tmp\"") == "1000:600"

    failure = c.run("dd if=/dev/zero of=/tmp/psios-fill bs=64K count=513 2>&1; test $? -ne 0")
    assert "No space left on device" in failure, failure
    assert int(c.run("stat -c %s /tmp/psios-fill")) == 32 * 1024 * 1024
    task = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-full-tmp"))
    done = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert done["lifecycle"] == "Succeeded", done
    c.run("rm /tmp/psios-fill")

    failure = c.run("mkdir /tmp/psios-inodes || exit 1; i=0; "
                    "while true 2>&1 > \"/tmp/psios-inodes/$i\"; do "
                    "i=$((i+1)); test \"$i\" -le 4096 || exit 1; done; printf '\\n%s\\n' \"$i\"")
    assert "No space left on device" in failure, failure
    assert 4064 <= int(failure.splitlines()[-1]) < 4096, failure
    c.run("rm -rf /tmp/psios-inodes && printf 'volatile\\n' > /tmp/psios-scratch-marker")


def check_runtime_directory(c):
    assert c.run("stat -c '%u:%g:%a' /run") == "0:0:755"
    assert c.run("stat -c '%u:%g:%a' /run/psios") == "1000:1000:700"
    c.run("test ! -w /run && test -S /run/psios/api.sock")
    blocks, block_size, inode_total = map(int, c.run("stat -f -c '%b %S %c' /run").split())
    assert blocks * block_size == 8 * 1024 * 1024 and inode_total == 1024

    private = "/run/psios"
    remaining, size = map(int, c.run(f"stat -f -c '%a %S' {private}").split())
    full = c.run(f"dd if=/dev/zero of={private}/test-bytes bs=64K count=129 2>&1; test $? -ne 0")
    assert "No space left on device" in full, full
    assert int(c.run(f"stat -c %s {private}/test-bytes")) == remaining * size
    c.run(f"rm {private}/test-bytes")

    available = int(c.run(f"mkdir {private}/test-inodes && stat -f -c '%d' {private}/test-inodes"))
    full = c.run(f"cd {private}/test-inodes || exit 1; i=0; "
                 'while true 2>&1 > "$i"; do i=$((i+1)); test "$i" -le 1024 || exit 1; done; echo "$i"')
    assert "No space left on device" in full and int(full.splitlines()[-1]) == available, full
    c.run("test -S /run/psios/api.sock && touch /tmp/psios-run-independent && rm /tmp/psios-run-independent")
    task = json.loads(c.run("printf '{}' | psiosctl submit system.status --key native-full-run"))
    receipt = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert receipt["lifecycle"] == "Succeeded", receipt
    c.run(f"rm -rf {private}/test-inodes && umask 077 && echo volatile > {private}/psios-run-cut")


def check_native_volume_usage(c):
    receipt = json.loads(c.run("printf '{\"name\":\"native-usage\"}' | psiosctl submit volume.create --key native-usage-volume"))
    task = json.loads(c.run("psiosctl wait " + shlex.quote(receipt["task_id"])))
    assert task["lifecycle"] == "Succeeded", task
    volume = call(c, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
    root = shlex.quote("/var/lib/psios/volumes/" + volume["resource_id"] + "/data")
    c.run(f"cd {root} && dd if=/dev/zero of=sparse bs=1 count=1 seek=8388607 && "
          "dd if=/dev/zero of=allocated bs=4096 count=32 && sync -f .")
    a, b = [tuple(map(int, line.split())) for line in c.run(f"cd {root} && stat -c '%s %b' sparse allocated").splitlines()]
    assert a[0] == 8 * 1024 * 1024 and b[0] == 128 * 1024 and a[1] < b[1], (a, b)
    params = shlex.quote(json.dumps({"target": {"id": volume["resource_id"], "expected_resource_version": volume["version"]}}))
    receipt = json.loads(c.run(f"printf '%s' {params} | psiosctl submit volume.usage --key native-usage-report"))
    task = json.loads(c.run("psiosctl wait " + shlex.quote(receipt["task_id"])))
    assert task["lifecycle"] == "Succeeded", task
    report = call(c, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
    assert report["volume_ref"] == volume and (report["files"], report["directories"]) == (2, 1), report
    assert report["logical_file_bytes"] == str(a[0] + b[0]), report
    assert report["allocated_file_bytes"] == str((a[1] + b[1]) * 512), report
    assert report["allocated_directory_bytes"] == str(int(c.run(f"stat -c %b {root}")) * 512), report


def check_native_mount_topology(c):
    node = json.loads(c.run("psiosctl node"))["reference"]["resource_id"]
    grant = call(c, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                 "actions": ["task.submit", "task.read", "node.mounts"], "units": 1, "ttl_seconds": 120})
    delegation = shlex.quote(grant["delegation_ref"]["resource_id"])
    receipt = json.loads(c.run(f"printf '{{}}' | psiosctl submit node.mounts --delegation {delegation} --key native-mounts"))
    task = json.loads(c.run("psiosctl wait " + shlex.quote(receipt["task_id"])))
    assert task["lifecycle"] == "Succeeded", task
    report = call(c, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]})
    assert report["mount_namespace_inode"] == c.run("stat -Lc %i /proc/self/ns/mnt")
    mounts = {mount["mount_point"]: mount for mount in report["mounts"] if isinstance(mount["mount_point"], str)}
    state, settings, homes, local_home, scratch, shared, runtime = [mounts[path] for path in
        ("/var/lib", "/etc", "/mnt/psios-homes", "/home/psios", "/tmp", "/dev/shm", "/run")]
    assert state["filesystem_type"] == "ext4" and {"nodev", "nosuid", "noexec"}.issubset(state["flags"]), state
    assert settings["device"] == state["device"] and settings["filesystem_root"] == "/etc", settings
    assert homes["filesystem_type"] == "ext4" and homes["device"] != state["device"], homes
    assert local_home["device"] == homes["device"] and local_home["filesystem_root"] == "/psios", local_home
    assert shared["filesystem_type"] == "tmpfs" and {"nodev", "nosuid", "noexec"}.issubset(shared["flags"]), shared
    assert runtime["filesystem_type"] == "tmpfs" and {"nodev", "nosuid", "noexec"}.issubset(runtime["flags"]), runtime
    assert runtime["device"] not in (shared["device"], scratch["device"]), runtime
    assert shared["device"] != scratch["device"], (shared, scratch)
    assert scratch["filesystem_type"] == "tmpfs" and {"nodev", "nosuid"}.issubset(scratch["flags"]), scratch


def check_native_pci(c):
    node = call(c, "Node.Get", {})["reference"]["resource_id"]
    grant = call(c, "Grant.Issue", {"principal_id": "local-owner", "tool_targets": [node],
                 "actions": ["task.submit", "task.read", "node.pci"], "units": 1, "ttl_seconds": 120})
    delegation = shlex.quote(grant["delegation_ref"]["resource_id"])
    receipt = json.loads(c.run(f"printf '{{}}' | psiosctl submit node.pci --delegation {delegation} --key native-pci"))
    task = call(c, "Task.Wait", {"id": receipt["task_id"], "timeout_ms": 5000})
    assert task["lifecycle"] == "Succeeded", task
    reference, = task["result"]["artifact_refs"]
    report = call(c, "Artifact.Read", {"reference": reference})
    address = c.run('for path in /sys/bus/pci/devices/*; do '
                    '[ "$(cat "$path/vendor")" = 0x1af4 ] || continue; '
                    '[ "$(basename "$(readlink "$path/driver")")" = virtio-pci ] || continue; '
                    'basename "$path"; break; done')
    device, = (item for item in report["devices"] if item["address"] == address)
    assert (device["vendor_id"], device["driver"]) == ("0x1af4", "virtio-pci"), device
    assert report["boot_id"] == c.run("cat /proc/sys/kernel/random/boot_id")


def check_native_network(c):
    node = json.loads(c.run("psiosctl node"))["reference"]["resource_id"]
    grant = call(c, "Grant.Issue", dict(principal_id="local-owner", tool_targets=[node],
                 actions=["task.submit", "task.read", "node.interfaces", "node.routes", "node.routing_rules", "node.route_lookup"],
                 units=4, ttl_seconds=120))
    delegation = shlex.quote(grant["delegation_ref"]["resource_id"])
    task = json.loads(c.run(f"printf '{{}}' | psiosctl submit node.interfaces --delegation {delegation} --key native-network"))
    done = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert done["lifecycle"] == "Succeeded", done
    report = call(c, "Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert report["network_namespace_inode"] == c.run("stat -Lc %i /proc/self/ns/net")
    row, = report["interfaces"]
    assert row["ifname"] == "lo" and row["link_type"] == "loopback" and "UP" in row["flags"]
    assert any(a["local"] == "127.0.0.1" and a["prefixlen"] == 8 and a["scope"] == "host" for a in row["addr_info"])
    query = shlex.quote(json.dumps({"family": "inet", "table": 255}))
    task = json.loads(c.run(f"printf '%s' {query} | psiosctl submit node.routes --delegation {delegation} --key native-routes"))
    done = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert done["lifecycle"] == "Succeeded", done
    routes = call(c, "Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert routes["network_namespace_inode"] == report["network_namespace_inode"], routes
    assert any(r["destination"] == {"address": "127.0.0.1", "length": 32}
               and r["kind"] == 2 and r["output_ifindex"] == row["ifindex"] for r in routes["routes"]), routes
    task = json.loads(c.run(f"printf '{{\"family\":\"inet\"}}' | psiosctl submit node.routing_rules "
                            f"--delegation {delegation} --key native-routing-rules"))
    done = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert done["lifecycle"] == "Succeeded", done
    rules = call(c, "Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert rules["network_namespace_inode"] == report["network_namespace_inode"], rules
    assert any(r["priority"] == 0 and r["table"] == 255 and r["action"] == 1 for r in rules["rules"]), rules
    task = json.loads(c.run(f"printf '{{\"destination\":\"127.0.0.1\"}}' | psiosctl submit node.route_lookup "
                            f"--delegation {delegation} --key native-route-lookup"))
    done = json.loads(c.run("psiosctl wait " + shlex.quote(task["task_id"])))
    assert done["lifecycle"] == "Succeeded", done
    lookup = call(c, "Artifact.Read", {"reference": done["result"]["artifact_refs"][0]})
    assert lookup["outcome"] == "Resolved" and lookup["route"]["output_ifindex"] == row["ifindex"], lookup
    assert lookup["route"]["table"] > 0 and lookup["route"]["kind"] == 2, lookup


def schedule_across_power_cut(c):
    grant = call(c, "Grant.Issue", dict(principal_id="local-owner", tool_targets=["profile-local"],
                 actions=["schedule.manage", "task.submit", "task.read", "system.status"], units=1, ttl_seconds=180))
    stamp = int(c.run("date -u +%s"))
    schedule = call(c, "Schedule.Create", dict(key="native-power-cut-schedule", tool="system.status", input={},
                    delegation_id=grant["delegation_ref"]["resource_id"],
                    run_at=datetime.fromtimestamp(stamp + 5, timezone.utc).isoformat(),
                    deadline_at=datetime.fromtimestamp(stamp + 120, timezone.utc).isoformat()))
    assert schedule["state"]["lifecycle"] == "Pending", schedule
    return schedule


def check_recovered_schedule(c, pending):
    request = {"id": pending["reference"]["resource_id"]}
    until = time.monotonic() + 20
    while True:
        current = call(c, "Schedule.Get", request)["state"]
        if current["lifecycle"] != "Pending" or time.monotonic() >= until:
            break
        time.sleep(0.25)
    assert current["lifecycle"] == "Triggered", current
    assert current["trigger_id"] == pending["state"]["trigger_id"], current
    receipt = current["task_receipt"]
    assert receipt["idempotency_key"] == current["trigger_id"], receipt
    result = json.loads(c.run("psiosctl wait " + shlex.quote(receipt["task_id"])))
    assert result["lifecycle"] == "Succeeded", result


def replace_partition(image, filesystem, offset_mib):
    with image.open("r+b") as dst, filesystem.open("rb") as src:
        dst.seek(offset_mib * 1024 * 1024)
        shutil.copyfileobj(src, dst)


def extract_partition(image, output, offset_mib, size_mib):
    with image.open("rb") as src, output.open("wb") as dst:
        src.seek(offset_mib * 1024 * 1024)
        for _ in range(size_mib):
            chunk = src.read(1024 * 1024)
            assert len(chunk) == 1024 * 1024
            dst.write(chunk)


def check_clean_shutdown(image, starts=(65, 257)):
    with image.open("rb") as disk:
        for start in starts:
            disk.seek(start * 1024 * 1024 + 1024)
            superblock = disk.read(1024)
            magic, state = struct.unpack_from("<HH", superblock, 56)
            incompatible, readonly_compatible = struct.unpack_from("<II", superblock, 96)
            assert magic == 0xEF53 and state == 1, (start, state)
            assert incompatible & 4 == 0 and readonly_compatible & 0x10000 == 0, start


def reject_damaged_payload(args, work, image, deployment, intact_efi):
    # Build a real UEFI application, changing just the source marker after its
    # expected digest has been sealed. Keep the original firmware to restore.
    damaged = work / "damaged"
    damaged.mkdir()
    spec = importlib.util.spec_from_file_location("native_installer", args.installer.resolve())
    installer = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(installer)
    inputs = argparse.Namespace(kernel_iso=args.kernel_iso, core_archive=args.archive, core_sha256=sha(args.archive))
    installer.ramfs(damaged, inputs, deployment["state_uuid"], deployment["home_uuid"])
    (damaged / "psios-source").write_text("damaged source marker\n")
    with (damaged / "initrd").open("ab") as out:
        out.write(b"\0" * (-out.tell() % 4))
        out.flush()
        subprocess.run(["cpio", "--quiet", "-o", "-H", "newc", "--owner=0:0"], cwd=damaged,
                       input=b"psios-source\n", stdout=out, check=True)
    efi = installer.boot_media.efi_application(damaged, installer.CMDLINE)
    destination = ["mcopy", "-o", "-i", f"{image}@@1048576"]
    subprocess.run([*destination, efi, "::/EFI/BOOT/BOOTX64.EFI"], check=True)
    before = sha(image)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at deployment-payload")
        assert c.vm.wait(25) == 0
    assert sha(image) == before
    subprocess.run([*destination, intact_efi, "::/EFI/BOOT/BOOTX64.EFI"], check=True)


def set_machine_id_fixture(work, image, destination, text, mode, *, replace=False):
    partition = work / "machine-identity.ext4"
    payload = work / "machine-id-test-input"
    extract_partition(image, partition, 65, 192)
    payload.write_text(text)
    commands = [f"rm {destination}"] if replace else []
    commands += [f"write {payload.name} {destination}", f"set_inode_field {destination} uid 0",
                 f"set_inode_field {destination} gid 0", f"set_inode_field {destination} mode {mode}"]
    subprocess.run(["debugfs", "-w", "-f", "-", partition], cwd=work,
                   input="\n".join(commands) + "\n", text=True, check=True, stdout=subprocess.DEVNULL)
    replace_partition(image, partition, 65)
    partition.unlink()
    payload.unlink()


def check_machine_id_clone(work, image, operator, admin):
    clone = work / "machine-identity-clone.raw"
    subprocess.run(["qemu-img", "convert", "-q", "-f", "raw", "-O", "raw", image, clone], check=True, timeout=90)
    assert sha(image) == sha(clone)
    shutil.copyfile(work / "vars.fd", work / "machine-identity-vars.fd")
    set_machine_id_fixture(work, clone, "/etc/.psios-machine-id.pending", "partial pending identity", "0100600")
    with boot(work, clone, vars_file="machine-identity-vars.fd", pace_commands=True) as c:
        c.login("psios", operator)
        observed = check_machine_identity(c)
        node = call(c, "Principal.WhoAmI", {})["authority_id"]
        c.send("exit")
        c.login("root", admin)
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    check_clean_shutdown(clone)

    set_machine_id_fixture(work, clone, "/etc/machine-id", "0" * 32 + "\n", "0100444", replace=True)
    with boot(work, clone, vars_file="machine-identity-vars.fd", pace_commands=True) as c:
        c.until(b"PsiOS boot stopped at machine-identity", reject=b"psios login:")
        assert c.vm.wait(25) == 0
    check_clean_shutdown(clone)
    clone.unlink()
    print("PASS stale machine ID preparation recovered and an invalid stored ID refused boot", flush=True)
    return observed, node


def exercise(args, work):
    operator, op_hash = password(work, "operator.hash")
    admin, root_hash = password(work, "root.hash")
    image = work / "installed.raw"
    build = [sys.executable, args.installer.resolve(), image, "--kernel-iso", args.kernel_iso,
             "--core-archive", args.archive, "--core-sha256", sha(args.archive), "--console-hash", op_hash,
             "--root-console-hash", root_hash]
    deployment = json.loads(subprocess.check_output(build))
    before = sha(image)
    assert before == deployment["initial_image_sha256"]
    extracted_efi = work / "boot.efi"
    subprocess.run(["mcopy", "-i", f"{image}@@1048576", "::/EFI/BOOT/BOOTX64.EFI", extracted_efi], check=True)
    assert sha(extracted_efi) == deployment["efi_application_sha256"]
    assert subprocess.run(build, capture_output=True).returncode != 0 and sha(image) == before
    assert image.stat().st_mode & 0o077 == 0
    for source, target in [("OVMF_CODE_4M.fd", "code.fd"), ("OVMF_VARS_4M.fd", "vars.fd")]:
        shutil.copyfile(args.ovmf_dir / source, work / target)
    clone_id, clone_node = check_machine_id_clone(work, image, operator, admin)
    with boot(work, image) as c:
        c.credentials("psios", "wrong-" + secrets.token_hex(10))
        c.until(b"Login incorrect")
        c.login("root", admin)
        c.run("touch /tmp/psios-root-scratch")
        c.send("exit")
        c.login("psios", operator)
        machine_id = check_machine_identity(c)
        assert machine_id != clone_id
        assert call(c, "Principal.WhoAmI", {})["authority_id"] != clone_node
        manifest = json.loads(c.run("cat /psios-deployment.json"))
        assert manifest["core_archive_sha256"] == sha(args.archive)
        assert manifest["core_source_commit"] == deployment["source_commit"]
        assert manifest["kernel_input"]["sha256"] == sha(args.kernel_iso)
        assert manifest["required_filesystems"] == {"state": deployment["state_uuid"], "home": deployment["home_uuid"]}
        assert c.run("sha256sum /psios-deployment.json").split()[0] == deployment["deployment_manifest_sha256"]
        payload = ("/bin/psiosd", "/bin/psiosctl", "/psios-source", "/psios-init",
                   "/psios-disk-uuid", "/psios-home-uuid", "/psios-shutdown", "/psios-dhcp", "/psios-dhcp-event",
                   "/psios-ipv4", "/psios-ipv6", "/psios-static", "/psios-storage", "/psios-storage-layout",
                   "/psios-enroll")
        with tarfile.open(args.archive, "r:gz") as archive:
            has_mcp = "psios-mcp" in archive.getnames()
            if has_mcp:
                with archive.extractfile("psios-mcp") as adapter:
                    assert hashlib.file_digest(adapter, "sha256").hexdigest() == manifest["payload_sha256"]["/bin/psios-mcp"]
                payload += ("/bin/psios-mcp",)
        assert set(manifest["payload_sha256"]) == set(payload), set(manifest["payload_sha256"]) ^ set(payload)
        for name in payload:
            assert c.run(f"sha256sum {name}").split()[0] == manifest["payload_sha256"][name]
        assert c.run("pwd") == "/home/psios"
        assert c.run("stat -c '%u:%g:%a' .") == "1000:1000:700"
        check_temporary_storage(c)
        check_runtime_directory(c)
        if has_mcp:
            check_native_mcp(c)
        chat = check_native_chat(c)
        c.run("test ! -e /etc/psios/dhcp-mac && ! pidof udhcpc")
        c.run("test ! -r /root && test ! -w /mnt/psios-homes")
        c.run("printf '#!/bin/sh\\necho persisted\\n' > ./observe && chmod 700 ./observe && sync -f .")
        assert c.run("./observe") == "persisted"
        # Fill only the user's filesystem; a fresh Core submission still needs
        # durable writes to the authority and its witness on partition two.
        failure = c.run("dd if=/dev/zero of=./full bs=64K 2>&1; test $? -ne 0")
        assert "No space left on device" in failure, failure
        assert int(c.run("stat -c %s ./full")) >= 40 * 1024 * 1024
        check_native_volume_usage(c)
        check_native_mount_topology(c)
        check_native_pci(c)
        check_native_network(c)
        check_resource_watch(c)
        workflow = check_native_workflow(c)
        # Unrelated tool grants issued by the checks above must preserve chat sends.
        first = check_state(c)
        assert not first[1]["deduplicated"]
        assert c.run("stat -c '%u:%g:%a' /dev/shm") == "0:0:1777"
        c.run("umask 077; echo volatile > /dev/shm/psios-cut")
        pending_schedule = schedule_across_power_cut(c)
        hold_terminal(c)
        # SIGKILL the VM with the OS and Core still running.
    with boot(work, image) as c:
        c.login("psios", operator)
        assert check_machine_identity(c) == machine_id
        assert c.run("./observe") == "persisted"
        c.run("test ! -e /run/psios/psios-run-cut && touch /run/psios/psios-run-shutdown")
        c.run("test ! -e /dev/shm/psios-cut && touch /dev/shm/psios-shutdown")
        c.run("test ! -e /dev/pts/0 && test ! -e /tmp/psios-scratch-marker && test ! -e /tmp/psios-root-scratch && "
              "touch /tmp/psios-scratch-marker")
        # Only the executable was synced before the power cut. The fill file
        # has served its capacity test and may be absent after journal replay.
        c.run("rm -f ./full")
        c.run("sync -f .")
        assert c.run("sha256sum /psios-deployment.json").split()[0] == deployment["deployment_manifest_sha256"]
        second = check_state(c)
        assert first[0] == second[0] and second[1]["deduplicated"]
        assert first[1]["task_id"] == second[1]["task_id"] and first[2] == second[2] and first[3] != second[3]
        assert call(c, "Workflow.Get", {"id": workflow["reference"]["resource_id"]}) == workflow
        check_recovered_chat(c, chat)
        check_recovered_schedule(c, pending_schedule)
        c.send("exit")
        c.login("root", admin)
        assert c.run("id -u") == "0"
        assert c.run("pwd; stat -c '%u:%g:%a' .").splitlines() == ["/root", "0:0:700"]
        c.run("umask 077; echo operator-rotated > ./maintenance")
        # passwd replaces /etc/shadow atomically. The host account database must
        # survive a reboot without restoring the credentials from initial enrollment.
        rotated = secrets.token_urlsafe(26)
        c.send("passwd psios")
        c.until(b"New password:")
        c.send_secret(rotated)
        c.until(b"Retype password:")
        c.send_secret(rotated)
        c.until(b"password for psios changed")
        # Keep a home file open across a normal poweroff from the root home.
        c.run("(trap '' TERM HUP; exec tail -f ./maintenance) </dev/null >/dev/null 2>&1 & "
              "p=$!; sleep 0.1; test \"$(readlink /proc/$p/cwd)\" = /root")
        c.run("(cd /tmp && trap '' TERM HUP && exec tail -f ./psios-scratch-marker) "
              "</dev/null >/dev/null 2>&1 & p=$!; sleep 0.1; test \"$(readlink /proc/$p/cwd)\" = /tmp")
        c.run("(cd /dev/shm && trap '' TERM HUP && exec tail -f ./psios-shutdown) "
              "</dev/null >/dev/null 2>&1 & p=$!; sleep 0.1; test \"$(readlink /proc/$p/cwd)\" = /dev/shm")
        c.run("(cd /run/psios && trap '' TERM HUP && exec tail -f ./psios-run-shutdown) "
              "</dev/null >/dev/null 2>&1 & p=$!; sleep 0.1; test \"$(readlink /proc/$p/cwd)\" = /run/psios")
        hold_terminal(c)
        c.send("poweroff")
        serial = c.until(b"Power down", timeout=30)
        assert b"PsiOS shutdown could not close" not in serial, serial[-2000:]
        assert c.vm.wait(35) == 0
    check_clean_shutdown(image)
    with boot(work, image) as c:
        c.credentials("psios", operator)
        c.until(b"Login incorrect")
        c.login("psios", rotated)
        assert check_machine_identity(c) == machine_id
        assert c.run("./observe") == "persisted"
        c.run("test ! -e /tmp/psios-scratch-marker && test ! -e /dev/shm/psios-shutdown")
        c.run("test ! -e /run/psios/psios-run-shutdown && test -S /run/psios/api.sock")
        c.run("test ! -r /root/maintenance")
        assert json.loads(c.run("psiosctl whoami"))["authority_id"] == first[0]
        c.send("exit")
        c.login("root", admin)
        assert c.run("cat ./maintenance") == "operator-rotated"
        c.send("poweroff")
        assert c.vm.wait(35) == 0
    # Inspect the *installed* second partition from the external read-only rescue kernel.
    report = subprocess.check_output([sys.executable, args.inspector, image, "--iso", args.kernel_iso,
                                      "--core", args.psiosd, "--core-sha256", sha(args.psiosd),
                                      "--partition", "2", "--state", "/psios", "--uid", "1000"])
    checked = json.loads(report)
    assert checked["sqlite_integrity"] == "Verified" and checked["node_id"] == first[0] and checked["foreign_keys"] == "Verified"
    reject_damaged_payload(args, work, image, deployment, extracted_efi)
    # A valid, independently formatted home volume cannot replace the expected
    # filesystem; detecting that mismatch must precede writing either volume.
    original_home = work / "original-home.ext4"
    extract_partition(image, original_home, 257, 64)
    foreign_home = work / "foreign-home.ext4"
    with foreign_home.open("xb") as f:
        f.truncate(64 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", foreign_home], check=True)
    replace_partition(image, foreign_home, 257)
    mismatched = sha(image)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at home-device")
        assert c.vm.wait(25) == 0
    assert sha(image) == mismatched
    replace_partition(image, original_home, 257)
    # Lose only the database of the stopped guest. The consumed enrollment must
    # never authorize a second initialization, even with the genuine disk UUID.
    partition = work / "state.ext4"
    extract_partition(image, partition, 65, 192)
    def read_owner():
        return subprocess.check_output(["debugfs", "-R", "cat /psios.witness/owner.token", partition],
                                       stderr=subprocess.DEVNULL)
    original_owner = read_owner()
    assert len(original_owner) == 64
    subprocess.run(["debugfs", "-w", "-R", "rm /psios/state.db", partition], check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    replace_partition(image, partition, 65)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at authority")
        assert c.vm.wait(25) == 0
    check_clean_shutdown(image)
    extract_partition(image, partition, 65, 192)
    assert read_owner() == original_owner
    absent = subprocess.run(["debugfs", "-R", "stat /psios/state.db", partition], capture_output=True)
    assert b"File not found" in absent.stderr
    partition.unlink()
    # Swap in a different ext4 filesystem on a disposable copy, including a fresh enrollment marker.
    alien = work / "other.ext4"
    seed = work / "foreign"
    seed.mkdir()
    (seed / "initialize-once").touch()
    with alien.open("xb") as f:
        f.truncate(192 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", "-d", seed, alien], check=True)
    replace_partition(image, alien, 65)
    pre_reject = sha(image)
    with boot(work, image) as c:
        c.until(b"PsiOS boot stopped at state-device")
        assert c.vm.wait(25) == 0
    assert sha(image) == pre_reject
    print("PASS Linux machine identity differs for clones and survives power loss and orderly shutdown")
    print(f"Verified source {deployment['source_commit']}, node {first[0]}, task {first[1]['task_id']}, "
          "matched deployment receipt, authenticated UEFI console, private persistent homes, full-home isolation, "
          "bounded volatile /tmp, /dev/shm and /run, offline management, Task, schedule, workflow and chat recovery after power loss, "
          "clean shutdown with active home, /tmp, /dev/shm, /run and PTY handles, external rescue, clean ext4 "
          "after authority refusal, consumed enrollment and damaged payload and mismatched state/home refusal.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("--kernel-iso", "--archive", "--psiosd", "--inspector"):
        parser.add_argument(flag, type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    opts = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-installed-test-") as directory:
        exercise(opts, Path(directory))
