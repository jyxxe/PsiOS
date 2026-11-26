"""Compare fenced block layouts with Linux sysfs and optional real loop devices."""
from contextlib import closing, contextmanager
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import time
import unittest
import uuid

from support import Failure, Node, target


TOOL = "node.block_layout"
BLOCK = Path("/sys/dev/block")


def whole_disk():
    for path in sorted(BLOCK.iterdir()):
        if (path / "diskseq").is_file() and (path / "queue/physical_block_size").is_file():
            if (path / "size").read_text().strip() != "0":
                return path
    return None


def issue(node, principal, scope):
    return node.call("Grant.Issue", dict(principal_id=principal, actions=["task.submit", "task.read", TOOL],
        tool_targets=[scope], units=12, ttl_seconds=120))


def submit(node, grant, device, *, token=None, expected=None):
    params = {"major_minor": device}
    if expected is not None:
        params["expected"] = expected
    request = node.prepare(TOOL, params, token=token, delegation=grant["delegation_ref"]["resource_id"])
    return node.call("Task.Submit", request, token)["task_id"]


def observe_trackable(node, grant, device):
    request = node.prepare(TOOL, {"major_minor": device, "trackable": True},
                           delegation=grant["delegation_ref"]["resource_id"])
    return receipt(node, node.call("Task.Submit", request)["task_id"])


def enroll(node, sysfs, layout_grant, tracking_grant):
    dev = (sysfs / "dev").read_text().strip()
    sample = observe_trackable(node, layout_grant, dev)
    task = node.prepare("node.track_block_device", {"major_minor": dev, "expected": sample["fence"]},
                        delegation=tracking_grant["delegation_ref"]["resource_id"])
    return receipt(node, node.call("Task.Submit", task)["task_id"])


def receipt(node, task, token=None):
    result = node.wait(task)
    assert result["lifecycle"] == "Succeeded", result
    reference, = result["result"]["artifact_refs"]
    return node.call("Artifact.Read", {"reference": reference}, token)


@contextmanager
def attached(image, kernel_name=None):
    command = ["losetup", "--read-only", "--partscan"]
    if kernel_name is None:
        disk = Path(subprocess.check_output([*command, "--find", "--show", image], text=True, timeout=8).strip())
    else:
        disk = Path("/dev") / kernel_name
        subprocess.run([*command, disk, image], check=True, timeout=8)
    sysfs = Path("/sys/class/block") / disk.name
    try:
        deadline = time.monotonic() + 4
        while not (sysfs / (disk.name + "p1") / "partition").exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert (sysfs / (disk.name + "p1") / "partition").is_file(), sysfs
        yield disk.name, sysfs
    finally:
        backing = sysfs / "loop/backing_file"
        if backing.is_file() and backing.read_text().strip() == str(image):
            subprocess.run(["losetup", "--detach", disk], check=True, timeout=8)
            deadline = time.monotonic() + 4
            while backing.exists() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert not backing.exists(), backing


def scratch_gpt(path):
    with path.open("xb") as image:
        image.truncate(16 * 1024 * 1024)
    subprocess.run(["sfdisk", path], input="label: gpt\nunit: sectors\n\nstart=2048,size=4096\n",
                   text=True, capture_output=True, check=True, timeout=15)


@contextmanager
def readonly_mapper(device, sectors):
    name = "psios-layout-" + uuid.uuid4().hex
    subprocess.run(["dmsetup", "create", name, "--readonly", "--noudevsync",
                    "--table", f"0 {sectors} linear {device} 0"], check=True, timeout=10)
    try:
        info = subprocess.check_output(["dmsetup", "info", "--columns", "--noheadings", "--separator", ":",
                                       "--options", "major,minor", name], text=True, timeout=8)
        yield info.strip()
    finally:
        subprocess.run(["dmsetup", "remove", "--noudevsync", name], check=True, timeout=8)


class BlockLayoutTests(unittest.TestCase):
    def setUp(self):
        self.node = Node(os.getenv("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.scope = self.node.call("Node.Get", {})["reference"]["resource_id"]
        self.grant = issue(self.node, "local-owner", self.scope)

    def enable_reconciler(self):
        self.node.stop()
        config = json.loads(self.node.config.read_text())
        config["block_device_reconcile_ms"] = 250
        self.node.config.write_text(json.dumps(config))
        self.node.start()

    def tracking_delegation(self):
        return self.node.call("Grant.Issue", {"principal_id": "local-owner", "ttl_seconds": 120, "units": 8,
            "actions": ["node.track_block_device", "task.submit", "task.read"], "tool_targets": [self.scope]})

    def enroll_for_reconciliation(self, sysfs, grant):
        return enroll(self.node, sysfs, self.grant, grant)

    def tracked_reader(self, device):
        node = self.node
        actor = node.call("Agent.Register", {"name": "tracked-block-observer"})
        grant = node.call("Grant.Issue", {"principal_id": actor["principal_id"], "ttl_seconds": 120, "units": 8,
            "actions": ["device.block_layout", "task.submit", "task.read", "resource.read", "event.read"],
            "tool_targets": [device["resource_id"]]})
        return actor["token"], grant["delegation_ref"]["resource_id"]

    def wait_revision(self, device, token, version, lifecycle):
        deadline = time.monotonic() + 7
        while True:
            current = self.node.call("BlockDevice.Get", {"id": device["resource_id"]}, token)
            actual = (current["reference"]["version"], current["state"]["lifecycle"])
            if actual == (version, lifecycle) or time.monotonic() >= deadline:
                self.assertEqual(actual, (version, lifecycle), current)
                return current
            time.sleep(.05)

    def assert_exact_event_refs(self, cursor, references):
        report = self.node.call("Event.Poll", {"cursor": cursor})
        events = report["events"]
        self.assertEqual([event["resource_ref"] for event in events], references, events)
        return report["cursor"]

    def test_sysfs_geometry_and_fence(self):
        node = self.node
        source = whole_disk()
        if source is None:
            self.skipTest("kernel exposes no whole disk with a disk sequence and usable geometry")
        observed = receipt(node, submit(node, self.grant, source.name))
        self.assertEqual(observed["node_ref"]["resource_id"], self.scope)
        fence = observed["fence"]
        disk = observed["layout"]
        self.assertEqual(fence["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
        self.assertEqual(fence["disk_sequence"], (source / "diskseq").read_text().strip())
        self.assertEqual(disk["major_minor"], source.name)
        self.assertEqual(disk["sysfs_path"], str(source.resolve()))
        self.assertEqual(disk["size_sectors_512"], (source / "size").read_text().strip())
        self.assertEqual(disk["read_only"], (source / "ro").read_text().strip() == "1")
        for key in ("logical", "physical"):
            self.assertEqual(disk[key + "_block_bytes"], int((source / ("queue/" + key + "_block_size")).read_text()))
        self.assertEqual(receipt(node, submit(node, self.grant, source.name, expected=fence))["fence"], fence)
        forged = dict(fence, layout_digest="sha256:" + "0" * 64)
        task = node.wait(submit(node, self.grant, source.name, expected=forged))
        self.assertEqual(task["conditions"][0]["type"], "PreconditionFailed", task)
        missing = node.wait(submit(node, self.grant, "4095:1048575"))
        self.assertEqual(missing["conditions"][0]["type"], "NotFound", missing)

    def test_requires_explicit_scope_and_rejects_user_paths(self):
        node = self.node
        with self.assertRaises(Failure) as owner:
            node.submit(TOOL, {"major_minor": "7:0"})
        self.assertEqual(owner.exception.code, "PermissionDenied")
        with self.assertRaises(Failure) as bad_path:
            submit(node, self.grant, "../class/block")
        self.assertEqual(bad_path.exception.code, "InvalidArgument")
        agent = node.call("Agent.Register", {"name": "layout-observer"})
        token = agent["token"]
        wrong = issue(node, agent["principal_id"], "profile-local")
        with self.assertRaises(Failure) as wrong_scope:
            submit(node, wrong, "7:0", token=token)
        self.assertEqual(wrong_scope.exception.code, "PermissionDenied")
        grant = issue(node, agent["principal_id"], self.scope)
        node.call("Grant.Revoke", target(grant["grant_ref"]))
        with self.assertRaises(Failure) as revoked:
            submit(node, grant, "7:0", token=token)
        self.assertEqual(revoked.exception.code, "PermissionDenied")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_LOOP") == "1", "requires read-only loop fixtures")
    def test_loop_replacement_with_same_device_number_and_partition_geometry(self):
        node = self.node
        first, second = node.base / "first.raw", node.base / "second.raw"
        scratch_gpt(first)
        scratch_gpt(second)
        with attached(first) as (name, sysfs):
            device = (sysfs / "dev").read_text().strip()
            observed = receipt(node, submit(node, self.grant, device))
            disk = observed["layout"]
            self.assertTrue(disk["read_only"])
            part, = disk["partitions"]
            self.assertEqual((part["number"], part["start_sectors_512"], part["size_sectors_512"]), (1, "2048", "4096"))
            self.assertEqual(part["major_minor"], (sysfs / (name + "p1") / "dev").read_text().strip())
            invalid = node.wait(submit(node, self.grant, part["major_minor"]))
            self.assertEqual(invalid["conditions"][0]["type"], "InvalidArgument", invalid)

            if os.getenv("PSIOS_TEST_BLOCK_MAPPER") == "1":
                with readonly_mapper(part["major_minor"], part["size_sectors_512"]) as mapper:
                    parent = receipt(node, submit(node, self.grant, device))
                    layered = receipt(node, submit(node, self.grant, mapper))
                    self.assertEqual(parent["layout"]["partitions"][0]["holders"], [mapper])
                    self.assertEqual(layered["layout"]["backing_devices"], [part["major_minor"]])
                    self.assertNotEqual(parent["fence"]["layout_digest"], observed["fence"]["layout_digest"])
                    stale = node.wait(submit(node, self.grant, device, expected=observed["fence"]))
                    self.assertEqual(stale["conditions"][0]["type"], "PreconditionFailed", stale)

            # A capacity change on the same backing file must invalidate the geometry seal.
            with first.open("r+b") as image:
                image.truncate(18 * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
            resized = receipt(node, submit(node, self.grant, device))
            self.assertEqual(resized["fence"]["disk_sequence"], observed["fence"]["disk_sequence"])
            self.assertNotEqual(resized["fence"]["layout_digest"], observed["fence"]["layout_digest"])
            stale = node.wait(submit(node, self.grant, device, expected=observed["fence"]))
            self.assertEqual(stale["conditions"][0]["type"], "PreconditionFailed", stale)

        with attached(second, name) as (_, sysfs):
            self.assertEqual((sysfs / "dev").read_text().strip(), device)
            new = receipt(node, submit(node, self.grant, device))
            new_partition, = new["layout"]["partitions"]
            partition_device = (sysfs / (name + "p1") / "dev").read_text().strip()
            self.assertEqual(new_partition, dict(part, major_minor=partition_device))
            self.assertNotEqual(new["fence"]["disk_sequence"], observed["fence"]["disk_sequence"])
            stale = node.wait(submit(node, self.grant, device, expected=observed["fence"]))
            self.assertEqual(stale["conditions"][0]["type"], "PreconditionFailed", stale)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_LOOP") == "1", "requires read-only loop fixtures")
    def test_loop_tracked_grant_keeps_its_original_attachment_across_restart_and_reuse(self):
        node = self.node
        track = "node.track_block_device"
        inspect = "device.block_layout"
        first, second = node.base / "tracked-first.raw", node.base / "tracked-second.raw"
        scratch_gpt(first)
        scratch_gpt(second)

        def grant(principal, tool, scope):
            request = {"principal_id": principal, "actions": [tool, "task.submit", "task.read"],
                       "units": 12, "ttl_seconds": 120}
            if scope is not None:
                request["tool_targets"] = [scope]
            return node.call("Grant.Issue", request)

        def send(tool, params, scope, token=None):
            request = node.prepare(tool, params, token=token,
                                   delegation=scope["delegation_ref"]["resource_id"])
            return node.call("Task.Submit", request, token)["task_id"]

        def run(tool, params, scope, token=None):
            return receipt(node, send(tool, params, scope, token), token)

        def denied(code, call):
            with self.assertRaises(Failure) as error:
                call()
            self.assertEqual(error.exception.code, code, error.exception)

        owner_scope = grant("local-owner", track, self.scope)
        actor = node.call("Agent.Register", {"name": "one-block-attachment"})
        actor_id, token = actor["principal_id"], actor["token"]
        denied("PermissionDenied", lambda: grant(actor_id, track, self.scope))
        denied("InvalidArgument", lambda: grant(actor_id, inspect, self.scope))
        denied("InvalidArgument", lambda: grant(actor_id, inspect, None))

        with attached(first) as (name, sysfs):
            major_minor = (sysfs / "dev").read_text().strip()
            observed = observe_trackable(node, self.grant, major_minor)
            fence = observed["fence"]
            self.assertRegex(fence["attachment_digest"], r"^sha256:[0-9a-f]{64}$")
            self.assertNotIn(str(first), str(observed))
            if os.getenv("PSIOS_TEST_BLOCK_MAPPER") == "1":
                part, = observed["layout"]["partitions"]
                with readonly_mapper(part["major_minor"], part["size_sectors_512"]) as mapper:
                    request = node.prepare(TOOL, {"major_minor": mapper, "trackable": True},
                                           delegation=self.grant["delegation_ref"]["resource_id"])
                    denied_mapping = node.wait(node.call("Task.Submit", request)["task_id"])
                    self.assertEqual(denied_mapping["conditions"][0]["type"], "UnsupportedCapability")
            request = {"major_minor": major_minor, "expected": fence}
            denied("PermissionDenied", lambda: node.prepare(track, request))
            enrolled = run(track, request, owner_scope)
            original = enrolled["device_ref"]
            self.assertEqual(run(track, request, owner_scope)["device_ref"], original)
            old_id = original["resource_id"]
            access = grant(actor_id, inspect, old_id)
            identity = {"id": old_id}
            current = node.call("BlockDevice.Get", identity, token)
            self.assertEqual(current["reference"], original)
            self.assertTrue(current["on_current_boot"])
            self.assertEqual(current["state"]["device_generation"], "1")
            denied("PermissionDenied", lambda: node.call("BlockDevice.Get", identity))
            cursor = node.call("Resource.List", {"kind": "BlockDevice", "id": old_id})["watch_start_cursor"]

            node.stop(signal.SIGKILL)
            node.start()
            self.assertEqual(node.call("BlockDevice.Get", identity, token), current)
            same = run(inspect, target(original), access, token)
            self.assertEqual((same["device_ref"], same["layout"]), (original, observed["layout"]))
            with first.open("r+b") as file:
                file.truncate(18 * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
            resized = run(inspect, target(original), access, token)
            self.assertEqual(resized["identity"], same["identity"])
            self.assertNotEqual(resized["fence"]["layout_digest"], fence["layout_digest"])
            self.assertEqual(resized["device_ref"]["version"], "r2")
            denied("Conflict", lambda: send(inspect, target(original), access, token))

        with attached(second, name) as (_, sysfs):
            self.assertEqual((sysfs / "dev").read_text().strip(), major_minor)
            terminated = run(inspect, target(resized["device_ref"]), access, token)
            self.assertEqual(terminated["lifecycle"], "Replaced")
            self.assertEqual(terminated["identity"], enrolled["identity"])
            self.assertEqual(terminated["device_ref"]["version"], "r3")
            for field in ("layout", "fence", "mount_namespace_inode"):
                self.assertNotIn(field, terminated)
            events = node.call("Event.Poll", {"cursor": cursor})["events"]
            revisions = [item["resource_ref"] for item in events if item["resource_ref"]["resource_id"] == old_id]
            self.assertEqual(revisions, [resized["device_ref"], terminated["device_ref"]])

            fresh = observe_trackable(node, self.grant, major_minor)
            self.assertNotEqual(fresh["fence"]["attachment_digest"], fence["attachment_digest"])
            new_attempt = node.prepare(track, request, delegation=owner_scope["delegation_ref"]["resource_id"])
            refused = node.wait(node.call("Task.Submit", new_attempt)["task_id"])
            self.assertEqual(refused["conditions"][0]["type"], "PreconditionFailed", refused)
            new = run(track, {"major_minor": major_minor, "expected": fresh["fence"]}, owner_scope)["device_ref"]
            self.assertNotEqual(new["resource_id"], old_id)
            denied("PermissionDenied", lambda: send(inspect, target(new), access, token))
            denied("PermissionDenied", lambda: node.call("BlockDevice.Get", {"id": new["resource_id"]}, token))
            self.assertEqual(run(inspect, target(terminated["device_ref"]), access, token), terminated)
            new_access = grant(actor_id, inspect, new["resource_id"])
            self.assertEqual(run(inspect, target(new), new_access, token)["layout"], fresh["layout"])

        node.stop()
        subprocess.run(node.daemon + ["verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_LOOP") == "1", "requires read-only loop fixtures")
    def test_loop_reconciler_persists_geometry_and_detects_reuse_across_process_death(self):
        node = self.node
        self.enable_reconciler()
        first, second = node.base / "watched-first.raw", node.base / "watched-second.raw"
        scratch_gpt(first)
        scratch_gpt(second)
        tracker = self.tracking_delegation()
        with attached(first) as (name, sysfs):
            enrolled = self.enroll_for_reconciliation(sysfs, tracker)
            original = enrolled["device_ref"]
            token, _ = self.tracked_reader(original)
            where = {"kind": "BlockDevice", "id": original["resource_id"]}
            cursor = node.call("Resource.List", where)["watch_start_cursor"]
            self.assertEqual(node.call("Resource.List", where, token)["items"], [])
            before = self.wait_revision(original, token, "r1", "Present")
            with first.open("r+b") as disk:
                disk.truncate(18 * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
            changed = self.wait_revision(original, token, "r2", "Present")
            self.assertEqual(changed["state"]["identity"], before["state"]["identity"])
            self.assertNotEqual(changed["state"]["layout_digest"], before["state"]["layout_digest"])
            self.assertEqual(changed["state"]["device_generation"], "2")
            after_resize = self.assert_exact_event_refs(cursor, [changed["reference"]])
            self.assertEqual(node.call("Resource.Watch", {"cursor": after_resize, "timeout_ms": 750})["events"], [])
            node.stop(signal.SIGKILL)

        with attached(second, name) as (_, sysfs):
            self.assertEqual((sysfs / "dev").read_text().strip(), enrolled["identity"]["major_minor"])
            self.assertNotEqual((sysfs / "diskseq").read_text().strip(), enrolled["identity"]["disk_sequence"])
            node.start()
            terminal = self.wait_revision(original, token, "r3", "Replaced")
            self.assertEqual(terminal["state"]["identity"], enrolled["identity"])
            self.assertIsNone(terminal["state"]["layout_digest"])
            self.assertNotIn("layout", terminal["state"])
            self.assert_exact_event_refs(cursor, [changed["reference"], terminal["reference"]])
            replacement = self.enroll_for_reconciliation(sysfs, tracker)["device_ref"]
            self.assertNotEqual(replacement["resource_id"], original["resource_id"])
            with self.assertRaises(Failure) as denied:
                node.call("BlockDevice.Get", {"id": replacement["resource_id"]}, token)
            self.assertEqual(denied.exception.code, "PermissionDenied")
            self.assertEqual(node.call("BlockDevice.Get", {"id": original["resource_id"]}, token), terminal)
            self.assert_exact_event_refs(cursor, [changed["reference"], terminal["reference"]])
            node.stop()
            subprocess.run([*node.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_MASK_BLOCK") == "1", "requires a private mount namespace")
    def test_loop_reconciler_does_not_treat_masked_sysfs_as_absent(self):
        # The opt-in test may cover /sys/dev/block only inside a non-propagating namespace.
        namespace = Path("/proc/self/ns/mnt").readlink()
        self.assertNotEqual(namespace, Path(f"/proc/{os.getppid()}/ns/mnt").readlink())
        for mount in Path("/proc/self/mountinfo").read_text().splitlines():
            fields = mount.split(" - ", 1)[0].split()
            if fields[4] in ("/", "/sys", "/sys/dev", str(BLOCK)):
                self.assertFalse(any(field.startswith("shared:") for field in fields[6:]), mount)
        node = self.node
        self.enable_reconciler()
        image = node.base / "watched-masked.raw"
        scratch_gpt(image)
        empty = node.base / "private-masked-sysfs"
        empty.mkdir()
        tracker = self.tracking_delegation()
        with attached(image) as (name, sysfs):
            original = self.enroll_for_reconciliation(sysfs, tracker)["device_ref"]
            token, delegation = self.tracked_reader(original)
            cursor = node.call("Resource.List", {"kind": "BlockDevice", "id": original["resource_id"]})[
                "watch_start_cursor"]
            before = self.wait_revision(original, token, "r1", "Present")
            node.stop()
            subprocess.run(["mount", "--bind", empty, BLOCK], check=True, timeout=8)
            try:
                self.assertEqual(list(BLOCK.iterdir()), [])
                with image.open("r+b") as disk:
                    disk.truncate(18 * 1024 * 1024)
                subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)
                node.start()
                request = node.prepare("device.block_layout", target(original), token=token, delegation=delegation)
                failed = node.wait(node.call("Task.Submit", request, token)["task_id"])
                self.assertEqual(failed["conditions"][0]["type"], "DependencyUnavailable", failed)
                deadline = time.monotonic() + 1
                while time.monotonic() < deadline:
                    current = node.call("BlockDevice.Get", {"id": original["resource_id"]}, token)
                    self.assertEqual(current, before)
                    time.sleep(.05)
                self.assert_exact_event_refs(cursor, [])
            finally:
                subprocess.run(["umount", BLOCK], check=True, timeout=8)

            changed = self.wait_revision(original, token, "r2", "Present")
            self.assertNotEqual(changed["state"]["layout_digest"], before["state"]["layout_digest"])
            self.assert_exact_event_refs(cursor, [changed["reference"]])
            node.stop()
            subprocess.run([*node.daemon, "verify"], check=True, stdout=subprocess.DEVNULL, timeout=20)

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BLOCK_LOOP") == "1", "requires read-only loop fixtures")
    def test_loop_retire_is_exact_and_keeps_live_device_reconciliation_after_sigkill(self):
        node = self.node
        self.enable_reconciler()
        retired_image = node.base / "retired.raw"
        live_image = node.base / "still-watched.raw"
        next_image = node.base / "replacement.raw"
        for image in (retired_image, live_image, next_image):
            scratch_gpt(image)
        tracker = self.tracking_delegation()

        def denied(code, callback):
            with self.assertRaises(Failure) as failure:
                callback()
            self.assertEqual(failure.exception.code, code, failure.exception)

        def resize(image, name, mib):
            with image.open("r+b") as disk:
                disk.truncate(mib * 1024 * 1024)
            subprocess.run(["losetup", "--set-capacity", Path("/dev") / name], check=True, timeout=8)

        with attached(retired_image) as (name, sysfs), attached(live_image) as (live_name, live_sysfs):
            original = self.enroll_for_reconciliation(sysfs, tracker)["device_ref"]
            other = self.enroll_for_reconciliation(live_sysfs, tracker)["device_ref"]
            token, delegation = self.tracked_reader(original)
            live_token, _ = self.tracked_reader(other)
            selector = {"kind": "BlockDevice", "id": original["resource_id"]}
            cursor = node.call("Resource.List", selector)["watch_start_cursor"]
            initial = self.wait_revision(original, token, "r1", "Present")
            live_initial = self.wait_revision(other, live_token, "r1", "Present")
            resize(retired_image, name, 18)
            before = self.wait_revision(original, token, "r2", "Present")
            denied("PermissionDenied", lambda: node.call("BlockDevice.Retire", target(before["reference"]), token))
            denied("Conflict", lambda: node.call("BlockDevice.Retire", target(original)))
            retired = node.cli(node.witness / "owner.token", "block-device-retire",
                               original["resource_id"], "--revision", "r2")
            self.assertEqual(retired, self.wait_revision(original, token, "r3", "Retired"))
            self.assertEqual(retired["state"]["retired_from_ref"], before["reference"])
            self.assertEqual(retired["state"]["identity"], initial["state"]["identity"])
            self.assertEqual(retired["state"]["observed_at"], before["state"]["observed_at"])
            self.assertIsNone(retired["state"]["layout_digest"])
            self.assertEqual(node.call("Resource.Resolve", {"reference": original})["value"], initial["state"])
            end = self.assert_exact_event_refs(cursor, [before["reference"], retired["reference"]])
            audit = [entry for entry in node.call("Audit.List", {"limit": 128}) if
                     entry["method"] == "BlockDevice.Retire" and entry["decision"] == "Allowed"]
            self.assertEqual([(entry["actor"], entry["resource"]) for entry in audit],
                             [("local-owner", original["resource_id"])])

            denied("PreconditionFailed", lambda: node.call("BlockDevice.Retire", target(retired["reference"])))
            observation = node.prepare("device.block_layout", target(retired["reference"]), token=token,
                                       delegation=delegation)
            denied("PreconditionFailed", lambda: node.call("Task.Submit", observation, token))
            device = (sysfs / "dev").read_text().strip()
            sample = observe_trackable(node, self.grant, device)
            request = node.prepare("node.track_block_device", {"major_minor": device, "expected": sample["fence"]},
                                   delegation=tracker["delegation_ref"]["resource_id"])
            refused = node.wait(node.call("Task.Submit", request)["task_id"])
            self.assertEqual(refused["conditions"][0]["type"], "PreconditionFailed", refused)
            node.stop(signal.SIGKILL)
            resize(retired_image, name, 20)
            resize(live_image, live_name, 18)
            node.start()
            live = self.wait_revision(other, live_token, "r2", "Present")
            self.assertNotEqual(live["state"]["layout_digest"], live_initial["state"]["layout_digest"])
            self.assertEqual(node.call("BlockDevice.Get", {"id": original["resource_id"]}, token), retired)
            self.assert_exact_event_refs(end, [])

        with attached(next_image, name) as (_, sysfs):
            self.assertEqual((sysfs / "dev").read_text().strip(), device)
            replacement = self.enroll_for_reconciliation(sysfs, tracker)["device_ref"]
            self.assertNotEqual(replacement["resource_id"], original["resource_id"])
            denied("PermissionDenied", lambda: node.call("BlockDevice.Get", {"id": replacement["resource_id"]}, token))
            self.assertEqual(node.call("BlockDevice.Get", {"id": original["resource_id"]}, token), retired)

        node.stop()
        verify = [*node.daemon, "verify"]
        subprocess.run(verify, check=True, stdout=subprocess.DEVNULL, timeout=20)
        with closing(sqlite3.connect(node.state / "state.db")) as db:
            id_ = original["resource_id"]
            body, = db.execute("SELECT body FROM versions WHERE id=? AND rev=3", (id_,)).fetchone()
            db.execute("UPDATE versions SET body=json_set(body,'$.retired_from_ref',json(?)) WHERE id=? AND rev=3",
                       (json.dumps(original), id_))
            db.commit()
            bad = subprocess.run(verify, text=True, capture_output=True, timeout=20)
            self.assertNotEqual(bad.returncode, 0, bad)
            self.assertIn("block retirement does not match its prior active revision", bad.stderr + bad.stdout)
            db.execute("UPDATE versions SET body=? WHERE id=? AND rev=3", (body, id_))
            db.commit()
        subprocess.run(verify, check=True, stdout=subprocess.DEVNULL, timeout=20)
