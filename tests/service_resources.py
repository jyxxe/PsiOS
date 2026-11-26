"""Read actual systemd service cgroup counters and inherited limits through the Unix API."""

import argparse
from contextlib import contextmanager
from fractions import Fraction
import json
import os
from pathlib import Path
import subprocess
import uuid

from support import Failure, Node, key, target


TOOL = "service.resources"
CGROUP = Path("/sys/fs/cgroup")


def rejected(code, call):
    try:
        call()
    except Failure as error:
        assert error.code == code, error
    else:
        raise AssertionError(f"expected {code}")


def grant(node, principal, scope=None):
    request = {"principal_id": principal, "actions": ["task.submit", "task.read", TOOL],
               "units": 8, "ttl_seconds": 120}
    if scope is not None:
        request["tool_targets"] = [scope]
    return node.call("Grant.Issue", request)


def start_task(node, token, authority, query):
    prepared = node.prepare(TOOL, query, token=token, delegation=authority["delegation_ref"]["resource_id"])
    return node.call("Task.Submit", prepared, token)["task_id"]


def observe(node, token, authority, query):
    task = node.wait(start_task(node, token, authority, query))
    assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
    reference, = task["result"]["artifact_refs"]
    return node.call("Artifact.Read", {"reference": reference}, token)


def configured(path, name):
    try:
        return (path / name).read_text().strip()
    except FileNotFoundError:
        return None


def limit(fields):
    return {"status": "Observed", "value": {"kind": "Bounded", "value": min(fields)}}


def assert_kernel(report, service, props, unit_slice, before_usage, manager):
    group_name = props["ControlGroup"]
    group = CGROUP / group_name.removeprefix("/")
    pid = int(props["MainPID"])
    assert group_name not in json.dumps(report), "raw cgroup path crossed the API"
    assert report["service_ref"] == service and report["invocation_id"] == props["InvocationID"], report
    assert report["manager"] == manager and report["manager_uid"] == os.geteuid(), report
    assert int(report["manager_started_monotonic_us"]) > 0, report
    assert int(report["from_boot_us"]) <= int(report["through_boot_us"]), report
    assert int(report["cgroup_mount_id"]) > 0, report
    process = report["main_process_ref"]
    assert process["pid"] == pid and process["boot_id"] == report["boot_id"], process
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    assert process["start_ticks"] == stat[19], process
    assert Path(f"/proc/{pid}/cgroup").read_text() == f"0::{group_name}\n"
    assert str(pid) in (group / "cgroup.procs").read_text().splitlines()
    cgroup = report["cgroup"]
    assert cgroup["source"] == "LinuxCgroupV2", cgroup
    assert cgroup["inode"] == str(group.stat().st_ino) == props["ControlGroupId"], cgroup
    assert cgroup["device"] == str(group.stat().st_dev), cgroup
    cpu_usage = cgroup["cpu"]["stat"]["value"]["usage_usec"]
    after_usage = int(dict(line.split() for line in (group / "cpu.stat").read_text().splitlines())["usage_usec"])
    assert before_usage <= cpu_usage <= after_usage, cgroup
    assert cgroup["memory"]["configured_max_bytes"] == limit([112 * 1024 * 1024]), cgroup
    assert cgroup["pids"]["configured_max"] == limit([40]), cgroup

    paths = [group, *(p for p in group.parents if p == CGROUP or CGROUP in p.parents)]
    levels = report["visible_limits"]
    assert (levels["coverage"], levels["groups_sampled"]) == ("VisibleCgroupAncestors", len(paths)), report
    rates = [tuple(map(int, value.split())) for p in paths
             if (value := configured(p, "cpu.max")) and not value.startswith("max ")]
    actual = levels["cpu_bandwidth"]["value"]
    assert Fraction(actual["quota_us"]["value"], actual["period_us"]) == min(
        Fraction(quota, period) for quota, period in rates), levels
    assert group.parent.name == unit_slice, group
    parent_quota, parent_period = map(int, (group.parent / "cpu.max").read_text().split())
    leaf_quota, leaf_period = map(int, (group / "cpu.max").read_text().split())
    assert (parent_quota > leaf_quota
            and Fraction(parent_quota, parent_period) < Fraction(leaf_quota, leaf_period)), rates
    for reported, file in [("memory_max_bytes", "memory.max"), ("swap_max_bytes", "memory.swap.max"),
                           ("pids_max", "pids.max")]:
        assert levels[reported] == limit([int(value) for p in paths
                   if (value := configured(p, file)) is not None and value != "max"]), levels


@contextmanager
def mask_cgroup():
    # Only cover the kernel mount in a private, non-propagating test namespace.
    assert os.geteuid() == 0
    assert Path("/proc/self/ns/mnt").readlink() != Path(f"/proc/{os.getppid()}/ns/mnt").readlink()
    for entry in Path("/proc/self/mountinfo").read_text().splitlines():
        fields = entry.split(" - ", 1)[0].split()
        if fields[4] in ("/", "/sys", "/sys/fs", str(CGROUP)):
            assert not any(field.startswith("shared:") for field in fields[6:]), entry
    subprocess.run(["mount", "-t", "tmpfs", "-o", "size=1m,nodev,nosuid,noexec", "tmpfs", CGROUP],
                   check=True, timeout=10)
    try:
        yield
    finally:
        subprocess.run(["umount", CGROUP], check=True, timeout=10)


def main(bin_dir, previous, masked_system):
    manager = "system" if masked_system else "user"
    assert not (masked_system and previous)
    command = ["/usr/bin/systemctl", f"--{manager}", "--no-pager", "--no-ask-password"]

    def ctl(*args):
        return subprocess.check_output([*command, *args], text=True, timeout=12)

    nonce = key()
    unit_slice = f"psiosresource{nonce}.slice"
    unit = f"psios-resources-{nonce}.service"
    other_unit = f"psios-resources-unrelated-{nonce}.service"
    directory = (Path("/run/systemd/system") if masked_system else
                 Path(f"/run/user/{os.geteuid()}/systemd/user"))
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    slicefile, servicefile = directory / unit_slice, directory / unit
    show = ("show", "--property=ControlGroup,ControlGroupId,MainPID,InvocationID", "--", unit)
    node = Node(previous or bin_dir)
    try:
        slicefile.write_text("""[Slice]
CPUQuota=25%
CPUQuotaPeriodSec=100ms
MemoryMax=96M
MemorySwapMax=32M
TasksMax=32
""")
        servicefile.write_text(f"""[Service]
Type=exec
Slice={unit_slice}
ExecStart=/usr/bin/sleep 300
CPUAccounting=yes
CPUQuota=50%
CPUQuotaPeriodSec=20ms
MemoryAccounting=yes
MemoryMax=112M
MemorySwapMax=48M
TasksAccounting=yes
TasksMax=40
""")
        ctl("daemon-reload")
        ctl("start", "--", unit)
        node.stop()
        config = json.loads(node.config.read_text())
        config["services"] = [{"unit": name, "manager": manager, "allow_control": False}
                              for name in (unit, other_unit)]
        node.config.write_text(json.dumps(config))
        if previous:
            node.start()
            old_service = node.get("service-" + unit)["reference"]
            rejected("NotFound", lambda: node.get("tool-" + TOOL))
            node.stop()
            old = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
            assert old["database_format"] == 107, old
            old_daemon = node.daemon
            node.binaries = bin_dir
            node.daemon = [str(bin_dir / "psiosd"), "--config", str(node.config)]
        node.start()
        service = node.get("service-" + unit)["reference"]
        if previous:
            assert service == old_service, service
        assert node.get("tool-" + TOOL)["reference"]["resource_id"] == "tool-" + TOOL
        obs = node.perform("service.inspect", target(service))
        query = {**target(service), "expected_boot_id": obs["boot_id"], "expected_invocation_id": obs["invocation_id"]}
        rejected("PermissionDenied", lambda: node.prepare(TOOL, query))
        actor = node.call("Agent.Register", {"name": "service-resource-observer"})
        token = actor["token"]
        principal = actor["principal_id"]
        rejected("InvalidArgument", lambda: grant(node, principal))
        rejected("InvalidArgument", lambda: grant(node, principal, "profile-local"))
        wrong = grant(node, principal, "service-" + other_unit)
        rejected("PermissionDenied", lambda: start_task(node, token, wrong, query))
        authority = grant(node, principal, service["resource_id"])
        rejected("InvalidArgument", lambda: start_task(node, token, authority, {**query, "pid": 1}))
        rejected("InvalidArgument", lambda: start_task(node, token, authority, {**query, "cgroup_path": "/"}))
        props = dict(line.split("=", 1) for line in ctl(*show).splitlines())
        group = CGROUP / props["ControlGroup"].removeprefix("/")
        before = int(dict(line.split() for line in (group / "cpu.stat").read_text().splitlines())["usage_usec"])
        measured = observe(node, token, authority, query)
        assert_kernel(measured, service, props, unit_slice, before, manager)
        if masked_system:
            node.stop()
            with mask_cgroup():
                node.start()
                try:
                    unavailable = node.wait(start_task(node, token, authority, query))
                    assert unavailable["lifecycle"] == "Failed", unavailable
                    assert unavailable["conditions"][0]["type"] == "UnsupportedCapability", unavailable
                    assert not unavailable["result"]["artifact_refs"], unavailable
                finally:
                    node.stop()
            node.start()
            assert observe(node, token, authority, query)["invocation_id"] == obs["invocation_id"]

        ctl("restart", "--", unit)
        fresh = node.perform("service.inspect", target(service))
        assert fresh["invocation_id"] != obs["invocation_id"], fresh
        stale = node.wait(start_task(node, token, authority, query))
        assert stale["lifecycle"] == "Failed" and stale["conditions"][0]["type"] == "PreconditionFailed", stale
        assert not stale["result"]["artifact_refs"], stale
        next_query = {**query, "expected_invocation_id": fresh["invocation_id"]}
        wrong_boot = node.wait(start_task(node, token, authority, {**next_query, "expected_boot_id": str(uuid.uuid4())}))
        assert wrong_boot["conditions"][0]["type"] == "PreconditionFailed", wrong_boot
        assert observe(node, token, authority, next_query)["invocation_id"] == fresh["invocation_id"]
        node.call("Grant.Revoke", target(authority["grant_ref"]))
        rejected("PermissionDenied", lambda: start_task(node, token, authority, next_query))
        node.stop()
        verified = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
        assert verified["database_format"] >= 108 and not verified["migration_required"], verified
        if previous:
            refused = subprocess.run([*old_daemon, "check"], capture_output=True, text=True, timeout=20)
            assert refused.returncode and "SchemaVersionUnsupported" in refused.stderr, refused
    finally:
        node.close()
        subprocess.run([*command, "stop", "--", unit, unit_slice], check=False, timeout=12)
        servicefile.unlink(missing_ok=True)
        slicefile.unlink(missing_ok=True)
        ctl("daemon-reload")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--previous-bin", type=Path)
    parser.add_argument("--masked-system", action="store_true")
    args = parser.parse_args()
    prior = args.previous_bin.resolve() if args.previous_bin else None
    main(args.bin_dir.resolve(), prior, args.masked_system)
