"""Boot a generated disk from OVMF, use its local console and recover after power loss."""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import select
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[1]


def sha(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


class Console:
    def __init__(self, vm):
        self.vm = vm
        self.buf = b""

    def until(self, marker, timeout=75, reject=None):
        end = time.monotonic() + timeout
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
                raise AssertionError(f"guest did not reach {marker!r}")
            chunk = os.read(self.vm.stdout.fileno(), 8192)
            if not chunk:
                raise AssertionError(f"guest stopped before {marker!r}")
            self.buf += chunk.replace(b"\r", b"")
            if len(self.buf) > 1024 * 1024:
                raise AssertionError("guest serial output exceeded the limit")
        data, _, self.buf = self.buf.partition(marker)
        return data

    def send(self, line):
        self.vm.stdin.write(line.encode() + b"\r")
        self.vm.stdin.flush()

    def send_secret(self, secret):
        # Give BusyBox time to switch off echo after printing the prompt.
        # Under KVM, keep password bursts smaller than the emulated UART FIFO.
        time.sleep(0.03)
        for start in range(0, len(secret), 8):
            self.vm.stdin.write(secret[start:start + 8].encode())
            self.vm.stdin.flush()
            time.sleep(0.01)
        self.send("")

    def credentials(self, user, password):
        self.until(b"login: ")
        self.send(user)
        self.until(b"Password: ")
        self.send_secret(password)

    def login(self, user, password):
        self.credentials(user, password)
        before_shell = self.until(b"# " if user == "root" else b"$ ", reject=b"Login incorrect")
        if password.encode() in before_shell:
            raise AssertionError("guest serial console echoed the fixture password")
        marker = "PSIOSLOGIN" + secrets.token_hex(6)
        self.send(f"stty -echo; export PS1=''; printf '\\n{marker}\\n'")
        self.until(("\n" + marker + "\n").encode())
        if user == "psios":
            self.run("for i in $(seq 1 100); do psiosctl whoami >/dev/null 2>&1 && exit 0; sleep 0.1; done; exit 1")

    def run(self, command):
        marker = "PSIOSCMD" + secrets.token_hex(6)
        self.send(f"printf '\\n{marker}\\n'; ( {command} ); r=$?; printf '\\n{marker}:%s\\n' \"$r\"")
        self.until(("\n" + marker + "\n").encode())
        output = self.until(("\n" + marker + ":").encode())
        status = self.until(b"\n")
        if status != b"0":
            raise AssertionError(f"guest command {command!r} exited {status!r}: {output[-512:]!r}")
        return output.decode().strip()


@contextmanager
def boot(work, image, network_mac=None, extra_disk=None, tap=None, *, rtc_base=None):
    accel = "kvm" if os.access("/dev/kvm", os.R_OK | os.W_OK) else "tcg"
    cmd = ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-machine", "q35",
           "-accel", accel, "-m", "768", "-smp", "2", "-display", "none",
           "-monitor", "none", "-serial", "stdio",
           "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=code.fd",
           "-drive", "if=pflash,format=raw,unit=1,file=vars.fd"]
    if rtc_base is not None:
        cmd += ["-rtc", f"base={rtc_base},clock=vm"]
    if extra_disk is not None:
        cmd += ["-boot", "strict=on",
                "-drive", f"if=none,format=raw,id=additional,file={extra_disk.name}",
                "-device", "virtio-blk-pci,drive=additional",
                "-drive", f"if=none,format=raw,id=installed,file={image.name}",
                "-device", "virtio-blk-pci,drive=installed,bootindex=1"]
    else:
        cmd += ["-boot", "order=c,strict=on", "-drive", f"if=virtio,format=raw,file={image.name}"]
    if network_mac:
        network = (f"tap,id=network,ifname={tap},script=no,downscript=no" if tap
                   else "user,id=network,net=192.0.2.0/24,dhcpstart=192.0.2.20")
        cmd += ["-netdev", network,
                "-device", f"virtio-net-pci,netdev=network,mac={network_mac},romfile="]
    else:
        cmd += ["-nic", "none"]
    with subprocess.Popen(cmd, cwd=work, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as vm:
        try:
            yield Console(vm)
        finally:
            if vm.poll() is None:
                vm.kill()
            vm.wait(15)


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


def stage_guest_chat_token(c, secret):
    path = "/tmp/psios-chat-fixture.token"
    try:
        c.run(f"umask 077; printf '%s' {shlex.quote(secret)} > {path}")
    except AssertionError:
        raise AssertionError("guest could not stage the private chat test credential") from None
    return path


def check_native_chat(c):
    agent = call(c, "Agent.Register", {"name": "native-conversation-peer"})
    call(c, "Grant.Issue", dict(principal_id="local-owner", units=1, ttl_seconds=600,
                               actions=["chat.invite", "chat.read", "chat.send"]))
    call(c, "Grant.Issue", dict(principal_id=agent["principal_id"], units=1, ttl_seconds=600, actions=["chat.read"]))
    token = stage_guest_chat_token(c, agent["token"])
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
    token = stage_guest_chat_token(c, secret)
    history = call(c, "Message.List", {"id": conversation}, token)
    assert history["items"] == [entry] and history["read_sequence"] == "1"
    assert call(c, "Message.Send", original_request) == dict(receipt, deduplicated=True)
    c.run("rm " + token)


def check_native_workflow(c):
    node = json.loads(c.run("psiosctl node"))["reference"]["resource_id"]
    grant = call(c, "Grant.Issue", {
        "principal_id": "local-owner", "tool_targets": ["profile-local", node], "units": 2, "ttl_seconds": 120,
        "actions": ["workflow.manage", "task.submit", "task.read", "system.status", "node.inventory"],
    })
    definition = call(c, "Workflow.Define", {
        "key": "native-offline-workflow",
        "steps": [
            {"id": "inventory", "tool": "node.inventory", "after": ["status"], "input": {}},
            {"id": "status", "tool": "system.status", "after": [], "input": {}},
        ],
    })
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
    inventory, status = observed["state"]["steps"]
    assert (inventory["lifecycle"], status["lifecycle"]) == ("Succeeded", "Succeeded")
    receipt = inventory["task_receipt"]
    assert receipt["origin_ref"]["origin_id"] == identity["id"]
    task = call(c, "Task.Get", {"id": receipt["task_id"]})
    artifact, = task["result"]["artifact_refs"]
    report = call(c, "Artifact.Read", {"reference": artifact})
    assert report["memory_bytes"]["MemTotal"] > 0, report
    return observed


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
    state, settings, homes, local_home, scratch = [mounts[path] for path in
        ("/var/lib", "/etc", "/mnt/psios-homes", "/home/psios", "/tmp")]
    assert state["filesystem_type"] == "ext4" and {"nodev", "nosuid", "noexec"}.issubset(state["flags"]), state
    assert settings["device"] == state["device"] and settings["filesystem_root"] == "/etc", settings
    assert homes["filesystem_type"] == "ext4" and homes["device"] != state["device"], homes
    assert local_home["device"] == homes["device"] and local_home["filesystem_root"] == "/psios", local_home
    assert scratch["filesystem_type"] == "tmpfs" and {"nodev", "nosuid"}.issubset(scratch["flags"]), scratch


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
    with boot(work, image) as c:
        c.credentials("psios", "wrong-" + secrets.token_hex(10))
        c.until(b"Login incorrect")
        c.login("root", admin)
        c.run("touch /tmp/psios-root-scratch")
        c.send("exit")
        c.login("psios", operator)
        manifest = json.loads(c.run("cat /psios-deployment.json"))
        assert manifest["core_archive_sha256"] == sha(args.archive)
        assert manifest["core_source_commit"] == deployment["source_commit"]
        assert manifest["kernel_input"]["sha256"] == sha(args.kernel_iso)
        assert manifest["required_filesystems"] == {"state": deployment["state_uuid"], "home": deployment["home_uuid"]}
        assert c.run("sha256sum /psios-deployment.json").split()[0] == deployment["deployment_manifest_sha256"]
        payload = ("/bin/psiosd", "/bin/psiosctl", "/psios-source", "/psios-init",
                   "/psios-disk-uuid", "/psios-home-uuid", "/psios-shutdown", "/psios-dhcp", "/psios-dhcp-event",
                   "/psios-storage", "/psios-storage-layout", "/psios-enroll")
        assert set(manifest["payload_sha256"]) == set(payload)
        for name in payload:
            assert c.run(f"sha256sum {name}").split()[0] == manifest["payload_sha256"][name]
        assert c.run("pwd") == "/home/psios"
        assert c.run("stat -c '%u:%g:%a' .") == "1000:1000:700"
        check_temporary_storage(c)
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
        check_native_network(c)
        workflow = check_native_workflow(c)
        # Unrelated tool grants issued by the checks above must preserve chat sends.
        first = check_state(c)
        assert not first[1]["deduplicated"]
        pending_schedule = schedule_across_power_cut(c)
        # SIGKILL the VM with the OS and Core still running.
    with boot(work, image) as c:
        c.login("psios", operator)
        assert c.run("./observe") == "persisted"
        c.run("test ! -e /tmp/psios-scratch-marker && test ! -e /tmp/psios-root-scratch && "
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
        c.send("poweroff")
        serial = c.until(b"Power down", timeout=30)
        assert b"PsiOS shutdown could not close" not in serial, serial[-2000:]
        assert c.vm.wait(35) == 0
    check_clean_shutdown(image)
    with boot(work, image) as c:
        c.credentials("psios", operator)
        c.until(b"Login incorrect")
        c.login("psios", rotated)
        assert c.run("./observe") == "persisted"
        c.run("test ! -e /tmp/psios-scratch-marker")
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
    print(f"Verified source {deployment['source_commit']}, node {first[0]}, task {first[1]['task_id']}, "
          "matched deployment receipt, authenticated UEFI console, private persistent homes, full-home isolation, "
          "bounded volatile /tmp, offline management, Task, schedule, workflow and chat recovery after power loss, "
          "clean shutdown with active home and /tmp readers, external rescue, clean ext4 "
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
