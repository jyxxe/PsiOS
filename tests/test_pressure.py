"""Exercise the authorised pressure task against the running Linux kernel."""
from contextlib import ExitStack, contextmanager
import os
from pathlib import Path
import signal
import subprocess
import time
import unittest

from support import Failure, Node, target


TOOL = "node.pressure"


def issue(node, principal, scope):
    return node.call("Grant.Issue", dict(principal_id=principal, tool_targets=[scope],
        actions=["task.submit", "task.read", TOOL], units=4, ttl_seconds=120))


def submit(node, grant, token=None, params=None):
    request = node.prepare(TOOL, {} if params is None else params, token=token,
                           delegation=grant["delegation_ref"]["resource_id"])
    return node.call("Task.Submit", request, token)["task_id"]


def artifact(node, task_id, token=None):
    task = node.wait(task_id)
    assert task["lifecycle"] == "Succeeded", task
    ref, = task["result"]["artifact_refs"]
    return ref, node.call("Artifact.Read", {"reference": ref}, token)


def psi_totals(path):
    try:
        return {line.split()[0]: int(line.rsplit("total=", 1)[1]) for line in path.read_text().splitlines()}
    except FileNotFoundError:
        return {}


@contextmanager
def private_bind(source, target):
    subprocess.run(["mount", "--bind", source, target], check=True, timeout=8)
    try:
        yield
    finally:
        subprocess.run(["umount", target], check=True, timeout=8)


class PressureTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.scope = self.node.call("Node.Get", {})["reference"]["resource_id"]
        self.grant = issue(self.node, "local-owner", self.scope)

    def test_real_kernel(self):
        node = self.node
        paths = {key: Path("/proc/pressure") / key for key in ("cpu", "memory", "io")}
        before = {key: psi_totals(path) for key, path in paths.items()}
        started = time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1000
        spec = node.base / "pressure.json"
        spec.write_text("{}")
        accepted = node.cli(node.witness / "owner.token", "submit", TOOL,
                            "--delegation", self.grant["delegation_ref"]["resource_id"],
                            "--key", "pressure-observation", str(spec))
        _, measured = artifact(node, accepted["task_id"])
        finished = time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1000
        self.assertEqual(measured["node_ref"]["resource_id"], self.scope)
        self.assertEqual(measured["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertEqual(measured["cgroup_namespace_inode"], str(Path(f"/proc/{node.process.pid}/ns/cgroup").stat().st_ino))
        self.assertEqual(measured["mount_namespace_inode"], str(Path(f"/proc/{node.process.pid}/ns/mnt").stat().st_ino))
        self.assertLessEqual(started, int(measured["from_boot_us"]))
        self.assertLessEqual(int(measured["from_boot_us"]), int(measured["through_boot_us"]))
        self.assertLessEqual(int(measured["through_boot_us"]), finished)
        kernel = measured["kernel"]
        for key, path in paths.items():
            if not before[key]:
                self.assertEqual(kernel[key], {"status": "Unsupported"})
                continue
            self.assertEqual(kernel[key]["status"], "Observed")
            after = psi_totals(path)
            window = kernel[key]["value"]
            for name, reading in window.items():
                self.assertLessEqual(before[key][name], reading["total_us"])
                self.assertLessEqual(reading["total_us"], after[name])
                for avg in ("avg10_percent", "avg60_percent", "avg300_percent"):
                    self.assertTrue(0 <= reading[avg] <= 100)
            if key == "cpu":
                self.assertEqual(set(window), {"some"})
        oom = dict(line.split() for line in Path("/proc/vmstat").read_text().splitlines()).get("oom_kill")
        if oom is not None:
            self.assertEqual(kernel["oom_kills"]["status"], "Observed")
            self.assertLessEqual(kernel["oom_kills"]["value"], int(oom))

        cgroup = measured["core_cgroup"]
        if os.getenv("PSIOS_TEST_HIDE_PRESSURE") == "1":
            self.assertTrue(all(not counts for counts in before.values()), before)
            self.assertEqual(cgroup, {"status": "Unsupported"})
        if os.getenv("PSIOS_TEST_PRESSURE_LIMITS") == "1":
            self.assertEqual(cgroup["status"], "Observed", cgroup)
            values = cgroup["value"]
            path = next(line[4:] for line in Path(f"/proc/{node.process.pid}/cgroup").read_text().splitlines()
                        if line.startswith("0::/"))
            directory = Path("/sys/fs/cgroup") / path
            group = directory.stat()
            self.assertEqual(values["device"], str(group.st_dev))
            self.assertEqual(values["inode"], str(group.st_ino))
            self.assertIn(str(node.process.pid), (directory / "cgroup.procs").read_text().splitlines())
            self.assertEqual(values["cpu"]["configured_limit"]["value"], {
                "quota_us": {"kind": "Bounded", "value": 30000}, "period_us": 100000})
            for group_name, field, expected in [
                ("memory", "configured_high_bytes", 96 * 1024 * 1024),
                ("memory", "configured_max_bytes", 128 * 1024 * 1024),
                ("memory", "configured_swap_max_bytes", 0),
                ("pids", "configured_max", 48),
            ]:
                self.assertEqual(values[group_name][field], {"status": "Observed", "value": {
                    "kind": "Bounded", "value": expected}})
            self.assertGreater(values["memory"]["current_bytes"]["value"], 0)
            self.assertGreater(values["pids"]["current"]["value"], 0)
            self.assertGreater(values["cpu"]["stat"]["value"]["usage_usec"], 0)
            self.assertIn("oom_kill", values["memory"]["local_events"]["value"])

    @unittest.skipUnless(os.getenv("PSIOS_TEST_REPLACE_PRESSURE") == "1", "requires a private mount namespace")
    def test_grafted_pressure_sources_do_not_hide_independent_kernel_data(self):
        namespace = Path("/proc/self/ns/mnt").readlink()
        self.assertNotEqual(namespace, Path(f"/proc/{os.getppid()}/ns/mnt").readlink())
        for line in Path("/proc/self/mountinfo").read_text().splitlines():
            fields = line.split(" - ", 1)[0].split()
            if fields[4] in ("/", "/proc", "/proc/pressure"):
                self.assertFalse(any(field.startswith("shared:") for field in fields[6:]), line)

        node = self.node
        # A regression may block the worker on a foreign FIFO. Keep cleanup
        # bounded and target only this fixture's daemon in that case.
        self.addCleanup(node.stop, signal.SIGKILL)
        _, baseline = artifact(node, submit(node, self.grant))
        for key in ("cpu", "memory", "io", "oom_kills"):
            self.assertEqual(baseline["kernel"][key]["status"], "Observed", baseline)
        substitutions = node.base / "foreign-counters"
        substitutions.mkdir()
        fake_cpu = substitutions / "cpu"
        fake_cpu.write_text("some avg10=7.00 avg60=8.00 avg300=9.00 total=987654321\n")
        fake_vmstat = substitutions / "vmstat"
        fake_vmstat.write_text("oom_kill 987654321\n")
        waiting = substitutions / "memory"
        os.mkfifo(waiting, mode=0o600)

        with ExitStack() as mounted:
            for source, target in ((fake_cpu, "/proc/pressure/cpu"), (waiting, "/proc/pressure/memory"),
                                   (fake_vmstat, "/proc/vmstat")):
                mounted.enter_context(private_bind(source, target))
            self.assertEqual(Path("/proc/pressure/cpu").read_bytes(), fake_cpu.read_bytes())
            self.assertEqual(Path("/proc/vmstat").read_bytes(), fake_vmstat.read_bytes())
            self.assertTrue(Path("/proc/pressure/memory").is_fifo())
            started = time.monotonic()
            _, observed = artifact(node, submit(node, self.grant))
            self.assertLess(time.monotonic() - started, 7)
            for key in ("cpu", "memory", "oom_kills"):
                self.assertEqual(observed["kernel"][key], {"status": "Unsupported"}, observed)
            io = observed["kernel"]["io"]
            self.assertEqual(io["status"], "Observed", observed)
            for kind, upper in psi_totals(Path("/proc/pressure/io")).items():
                self.assertLessEqual(baseline["kernel"]["io"]["value"][kind]["total_us"],
                                     io["value"][kind]["total_us"])
                self.assertLessEqual(io["value"][kind]["total_us"], upper)
            self.assertEqual(observed["core_cgroup"]["status"], baseline["core_cgroup"]["status"])
            if observed["core_cgroup"]["status"] == "Observed":
                self.assertEqual(observed["core_cgroup"]["value"]["inode"], baseline["core_cgroup"]["value"]["inode"])
            for key in ("boot_id", "mount_namespace_inode", "cgroup_namespace_inode"):
                self.assertEqual(observed[key], baseline[key])

        _, restored = artifact(node, submit(node, self.grant))
        for key in ("cpu", "memory", "io", "oom_kills"):
            self.assertEqual(restored["kernel"][key]["status"], "Observed", restored)
        node.stop()

    def test_scopes_artifact_and_revocation_through_restart(self):
        node = self.node
        with self.assertRaises(Failure) as default_grant:
            node.submit(TOOL, {})
        self.assertEqual(default_grant.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as user_path:
            submit(node, self.grant, params={"path": "/sys/fs/cgroup"})
        self.assertEqual(user_path.exception.code, "InvalidArgument")

        agent = node.call("Agent.Register", {"name": "resource-observer"})
        token = agent["token"]
        wrong = issue(node, agent["principal_id"], "profile-local")
        with self.assertRaises(Failure) as wrong_scope:
            submit(node, wrong, token)
        self.assertEqual(wrong_scope.exception.code, "PermissionDenied")

        grant = issue(node, agent["principal_id"], self.scope)
        reference, evidence = artifact(node, submit(node, grant, token), token)
        self.assertEqual(evidence["node_ref"]["resource_id"], self.scope)
        node.call("Grant.Revoke", target(grant["grant_ref"]))
        node.stop(signal.SIGKILL)
        node.start()
        with self.assertRaises(Failure) as revoked:
            submit(node, grant, token)
        self.assertEqual(revoked.exception.code, "PermissionDenied")
        self.assertEqual(node.call("Artifact.Read", {"reference": reference}), evidence)
