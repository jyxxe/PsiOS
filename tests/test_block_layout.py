"""Compare fenced block layouts with Linux sysfs and optional real loop devices."""
from contextlib import contextmanager
import os
from pathlib import Path
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
