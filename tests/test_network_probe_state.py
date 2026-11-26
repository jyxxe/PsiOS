"""Publish real loopback measurements through the broker and event scheduler."""
import json
import os
from pathlib import Path
import signal
import socket
import struct
import subprocess
import time
import unittest

from support import Failure, Node, key, target
from test_network_probe import POINTER, QNAME, TOOL as STATELESS_TOOL, dns_reply, record
from test_schedule import timestamp


TOOL = "node.observe_network_probe"
KIND = "NetworkProbeState"


class NetworkProbeStateTests(unittest.TestCase):
    def start_node(self, binaries=None):
        self.node = Node(binaries or os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.node.close)
        self.actor = self.node.call("Agent.Register", {"name": "probe-state-worker"})

    def socket(self, listening=False):
        stream = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.addCleanup(stream.close)
        stream.bind(("127.0.0.1", 0))
        stream.settimeout(6)
        if listening:
            stream.listen()
        return stream

    @staticmethod
    def endpoint(stream):
        host, port = stream.getsockname()
        return f"{host}:{port}"

    def configure(self, *probes, **settings):
        self.node.stop()
        config = json.loads(self.node.config.read_text())
        self.node.config.write_text(json.dumps(dict(config, network_probes=probes, **settings)))
        self.node.start()

    def probe(self, name):
        refs = self.node.call("Resource.List", {"kind": "NetworkProbe"})["items"]
        data, = [doc for ref in refs if (doc := self.node.get(ref["resource_id"]))["value"]["name"] == name
                 and doc["value"]["lifecycle"] == "Registered"]
        return data["reference"]

    def state_id(self, probe):
        refs = self.node.call("Resource.List", {"kind": KIND})["items"]
        state, = [ref["resource_id"] for ref in refs
                  if self.node.get(ref["resource_id"])["resource"]["spec"]["probe_ref"] == probe]
        return state

    def grant(self, reference, tool=TOOL):
        return self.node.call("Grant.Issue", {
            "principal_id": self.actor["principal_id"], "actions": ["task.submit", "task.read", tool],
            "tool_targets": [reference["resource_id"]], "units": 10, "ttl_seconds": 120,
        })

    def submit(self, reference, grant, tool=TOOL):
        token = self.actor["token"]
        request = self.node.prepare(tool, target(reference), token=token,
                                    delegation=grant["delegation_ref"]["resource_id"])
        return self.node.call("Task.Submit", request, token)["task_id"]

    def result(self, task_id):
        task = self.node.wait(task_id)
        self.assertEqual(task["lifecycle"], "Succeeded", task)
        artifact, = task["result"]["artifact_refs"]
        return self.node.call("Artifact.Read", {"reference": artifact}, self.actor["token"])

    def rejects(self, code, method, *args):
        with self.assertRaises(Failure) as failure:
            method(*args)
        self.assertEqual(failure.exception.code, code, failure.exception)

    def wait_trigger(self, schedule):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            rule = self.node.call("Schedule.Get", {"id": schedule["resource_id"]})["state"]
            if rule["trigger_refs"]:
                trigger, = rule["trigger_refs"]
                return trigger
            time.sleep(0.05)
        self.fail("probe lifecycle transition did not trigger the schedule")

    @staticmethod
    def dns_request(connection):
        connection.settimeout(6)
        with connection.makefile("rb") as reader:
            size, = struct.unpack("!H", reader.read(2))
            return reader.read(size)

    @staticmethod
    def dns_response(connection, reply):
        connection.sendall(struct.pack("!H", len(reply)) + reply)

    def assert_no_connection(self, listener):
        previous = listener.gettimeout()
        listener.settimeout(0.15)
        try:
            with self.assertRaises(TimeoutError):
                connection, _ = listener.accept()
                connection.close()
        finally:
            listener.settimeout(previous)

    def test_exact_grant_and_transition_drive_watch_and_schedule(self):
        self.start_node()
        n = self.node
        stream = self.socket()
        address = self.endpoint(stream)
        self.configure(dict(name="primary", endpoint=address), dict(name="other", endpoint=address),
                       read_result_bytes_per_principal=4 * 1024 * 1024)
        probe = self.probe("primary")
        other = self.probe("other")
        state_id = self.state_id(probe)
        initial = n.get(state_id)
        self.assertEqual(initial["value"]["lifecycle"], "Unknown")
        self.assertEqual(initial["resource"]["kind"], KIND)
        self.assertEqual(initial["resource"]["spec"]["probe_ref"], probe)
        self.assertNotIn(address, json.dumps(initial))
        grant = self.grant(probe)
        self.rejects("PermissionDenied", self.submit, probe, grant, STATELESS_TOOL)
        self.rejects("PermissionDenied", self.submit, other, grant)
        stateless = self.grant(probe, STATELESS_TOOL)
        self.rejects("PermissionDenied", self.submit, probe, stateless)
        raw = self.result(self.submit(probe, stateless, STATELESS_TOOL))
        self.assertEqual(raw["observation"]["tcp"]["status"], "Refused")
        self.assertEqual(n.get(state_id)["reference"], initial["reference"])

        schedule_grant = n.call("Grant.Issue", {
            "principal_id": "local-owner", "tool_targets": ["profile-local", state_id],
            "actions": ["schedule.manage", "task.submit", "task.read", "resource.read", "event.read", "system.status"],
            "units": 2, "ttl_seconds": 90,
        })
        schedule = n.call("Schedule.Create", {
            "key": key(), "tool": "system.status", "input": {},
            "delegation_id": schedule_grant["delegation_ref"]["resource_id"],
            "event": {"source_ref": initial["reference"],
                      "condition": {"kind": "LifecycleEntered", "lifecycle": "Healthy"},
                      "expires_at": timestamp(45), "max_triggers": 2},
        })["reference"]
        cursor = n.call("Resource.List", {"kind": KIND, "id": state_id})["watch_start_cursor"]

        closed = self.result(self.submit(probe, grant))
        refused = n.get(state_id)
        self.assertEqual(closed["measurement"]["target_ref"], probe)
        self.assertEqual(closed["measurement"]["observation"]["endpoint"], address)
        self.assertEqual(closed["network_probe_state_ref"], refused["reference"])
        self.assertEqual(closed["projection"], "Published")
        self.assertEqual((refused["value"]["lifecycle"], refused["value"]["reason"]), ("Unhealthy", "TcpRefused"))
        self.assertNotIn(address, json.dumps(refused))

        stream.listen()
        opened_task = self.submit(probe, grant)
        opened = self.result(opened_task)
        healthy = n.get(state_id)
        self.assertEqual(opened["projection"], "Published")
        self.assertEqual(opened["network_probe_state_ref"], healthy["reference"])
        self.assertEqual((healthy["value"]["lifecycle"], healthy["value"]["reason"]), ("Healthy", "TcpOpen"))
        self.assertNotIn(address, json.dumps(healthy))
        self.assertEqual(n.get(probe["resource_id"])["reference"], probe)

        trigger_ref = self.wait_trigger(schedule)
        trigger = n.call("Schedule.TriggerGet", {"id": trigger_ref["resource_id"]})["state"]
        self.assertEqual(trigger["lifecycle"], "Triggered", trigger)
        self.assertEqual(n.wait(trigger["task_receipt"]["task_id"])["lifecycle"], "Succeeded")
        page = n.call("Resource.Watch", {"cursor": cursor})
        self.assertEqual([e["resource_ref"] for e in page["events"]], [refused["reference"], healthy["reference"]])
        self.assertEqual(trigger["source_event"]["event_id"], page["events"][1]["event_id"])
        self.assertNotIn(address, json.dumps(page["events"]))

        def evict(task):
            return n.call("Task.EvictResults", dict(target(n.get(task)["reference"]), reason="test aging"))

        self.rejects("Conflict", evict, opened_task)
        repeated_task = self.submit(probe, grant)
        repeated = self.result(repeated_task)
        self.assertEqual((repeated["projection"], repeated["network_probe_state_ref"]), ("Unchanged", healthy["reference"]))
        self.assertEqual(n.call("Resource.Watch", {"cursor": page["cursor"]})["events"], [])
        evict(opened_task)
        self.assertEqual(n.wait(opened_task)["result"]["availability"], "Evicted")
        self.rejects("Conflict", evict, repeated_task)
        n.stop(signal.SIGKILL)
        subprocess.run(n.daemon + ["verify", "--scratch-dir", str(n.base)],
                       check=True, stdout=subprocess.DEVNULL, timeout=15)
        n.start()
        self.assertEqual(n.get(state_id), healthy)
        self.assertEqual(self.wait_trigger(schedule), trigger_ref)

        self.configure(dict(name="primary", endpoint=address, timeout_ms=700))
        current = self.probe("primary")
        self.assertEqual(current["resource_id"], probe["resource_id"])
        self.assertNotEqual(current, probe)
        reset = n.get(state_id)
        self.assertEqual(reset["value"]["lifecycle"], "Unknown")
        self.assertEqual(reset["resource"]["spec"]["probe_ref"], current)
        evict(repeated_task)
        self.assertEqual(n.wait(repeated_task)["result"]["availability"], "Evicted")
        self.rejects("Conflict", self.submit, probe, grant)
        self.configure()
        self.assertEqual(n.get(state_id)["value"]["lifecycle"], "Retired")

    def test_dns_name_absence_and_invalid_response_have_distinct_states(self):
        self.start_node()
        service, resolver = self.socket(listening=True), self.socket(listening=True)
        endpoint, resolver_endpoint = self.endpoint(service), self.endpoint(resolver)
        self.configure(dict(name="checked", endpoint=endpoint, timeout_ms=5000,
                            dns=dict(name=QNAME, resolver=resolver_endpoint)))
        probe = self.probe("checked")
        grant = self.grant(probe)
        for flags, invalid_id, lifecycle, reason in [
            (0x8183, False, "Unhealthy", "DnsNameMissing"),
            (0x8180, True, "Unknown", "DnsInvalidResponse"),
        ]:
            with self.subTest(reason=reason):
                task_id = self.submit(probe, grant)
                connection, _ = resolver.accept()
                with connection:
                    request = self.dns_request(connection)
                    response_id = (int.from_bytes(request[:2], "big") ^ 1) if invalid_id else None
                    self.dns_response(connection, dns_reply(request, flags=flags, response_id=response_id))
                result = self.result(task_id)
                self.assertEqual(result["measurement"]["observation"]["tcp"]["status"], "Skipped")
                state = self.node.get(result["network_probe_state_ref"]["resource_id"])
                self.assertEqual((state["value"]["lifecycle"], state["value"]["reason"]), (lifecycle, reason))
                for private in (endpoint, resolver_endpoint, QNAME):
                    self.assertNotIn(private, json.dumps(state))
        self.assert_no_connection(service)

    def test_revoked_during_dns_does_not_publish_measurement(self):
        self.start_node()
        n = self.node
        service, resolver = self.socket(listening=True), self.socket(listening=True)
        self.configure(dict(name="held", endpoint=self.endpoint(service), timeout_ms=5000,
                            dns=dict(name=QNAME, resolver=self.endpoint(resolver))))
        probe = self.probe("held")
        state_id = self.state_id(probe)
        initial = n.get(state_id)
        self.assertEqual(initial["value"]["lifecycle"], "Unknown")
        grant = self.grant(probe)
        task_id = self.submit(probe, grant)
        connection, _ = resolver.accept()
        with connection:
            request = self.dns_request(connection)
            self.assertEqual(n.call("Task.Get", {"id": task_id})["lifecycle"], "Running")
            n.call("Grant.Revoke", target(grant["grant_ref"]))
            page = n.call("Resource.List", {"kind": KIND, "id": state_id})
            self.assertEqual(page["items"], [initial["reference"]])
            reply = dns_reply(request, [record(POINTER, 1, socket.inet_aton("127.0.0.1"))])
            self.dns_response(connection, reply)

        task = n.wait(task_id)
        self.assertEqual(task["lifecycle"], "Failed", task)
        self.assertEqual(task["conditions"][0]["type"], "PermissionDenied", task)
        self.assertEqual(task["result"], {"availability": "Unavailable", "artifact_refs": []})
        self.assertEqual(n.get(state_id), initial)
        self.assertEqual(n.call("Resource.Watch", {"cursor": page["watch_start_cursor"]})["events"], [])
        self.assert_no_connection(service)

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_PROBE_NETNS") == "1",
                         "requires an isolated Linux namespace with working loopback and nested unshare")
    def test_real_netns_change_requires_fresh_measurement(self):
        self.start_node()
        n = self.node
        listener = self.socket(listening=True)
        self.configure(dict(name="isolated", endpoint=self.endpoint(listener)))
        probe = self.probe("isolated")
        state_id = self.state_id(probe)
        grant = self.grant(probe)

        def measure(lifecycle, reason, tcp):
            result = self.result(self.submit(probe, grant))
            state = n.get(state_id)
            self.assertEqual(result["projection"], "Published", result)
            self.assertEqual(result["network_probe_state_ref"], state["reference"])
            self.assertEqual((state["value"]["lifecycle"], state["value"]["reason"]), (lifecycle, reason))
            self.assertEqual(result["measurement"]["observation"]["tcp"]["status"], tcp)
            return result["measurement"]

        def assert_reset(previous):
            state = n.get(state_id)
            self.assertNotEqual(state["reference"], previous)
            self.assertEqual(state["resource"]["spec"]["probe_ref"], probe)
            self.assertEqual(state["value"], {"lifecycle": "Unknown", "reason": None, "observed_at": None})
            self.assertEqual(self.probe("isolated"), probe)

        outer_inode = str(os.stat("/proc/self/ns/net").st_ino)
        reachable = measure("Healthy", "TcpOpen", "Connected")
        self.assertEqual(reachable["network_namespace_inode"], outer_inode)
        old_state = n.get(state_id)["reference"]

        n.stop()
        daemon = n.daemon
        # Keep the fresh namespace's loopback down to prove a routing failure.
        n.daemon = ["unshare", "--net", "--", *daemon]
        try:
            n.start()
            assert_reset(old_state)
            unreachable = measure("Unhealthy", "TcpUnreachable", "Unreachable")
            self.assertNotEqual(unreachable["network_namespace_inode"], outer_inode)
            self.assertEqual(unreachable["boot_id"], reachable["boot_id"])
            isolated_state = n.get(state_id)["reference"]
        finally:
            n.stop()
            n.daemon = daemon

        n.start()
        assert_reset(isolated_state)
        restored = measure("Healthy", "TcpOpen", "Connected")
        self.assertEqual(restored["network_namespace_inode"], outer_inode)
        self.assertEqual(restored["boot_id"], reachable["boot_id"])

    @unittest.skipUnless(os.environ.get("PSIOS_TEST_PREVIOUS_PROBE_BIN_DIR"), "requires a format-70 daemon")
    def test_format_70_probe_upgrade_preserves_identity_and_scoped_grant(self):
        self.start_node(os.environ["PSIOS_TEST_PREVIOUS_PROBE_BIN_DIR"])
        n = self.node
        listener = self.socket(listening=True)
        self.configure(dict(name="upgrade", endpoint=self.endpoint(listener)))
        probe = self.probe("upgrade")
        legacy = self.grant(probe, STATELESS_TOOL)
        token = self.actor["token"]
        probe_view = n.call("NetworkProbe.Get", {"id": probe["resource_id"]}, token)

        def assert_connected(measurement):
            self.assertEqual(measurement["target_ref"], probe)
            self.assertEqual(measurement["observation"]["tcp"]["status"], "Connected")
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(2)
                self.assertEqual(connection.recv(1), b"")

        def verify():
            output = subprocess.check_output(n.daemon + ["verify", "--scratch-dir", str(n.base)], timeout=15)
            return json.loads(output)

        old_task = self.submit(probe, legacy, STATELESS_TOOL)
        old_result = self.result(old_task)
        assert_connected(old_result)
        n.stop(signal.SIGKILL)
        current = Path(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug")).resolve()
        n.daemon[0] = str(current / "psiosd")
        before = verify()
        self.assertEqual(before["database_format"], 70, before)
        self.assertTrue(before["migration_required"], before)

        n.start()
        self.assertEqual(self.probe("upgrade"), probe)
        self.assertEqual(n.call("NetworkProbe.Get", {"id": probe["resource_id"]}, token), probe_view)
        self.assertEqual(self.result(old_task), old_result)
        self.assertEqual(n.get(legacy["grant_ref"]["resource_id"])["reference"], legacy["grant_ref"])
        state_id = self.state_id(probe)
        initial = n.get(state_id)
        self.assertEqual(initial["resource"]["spec"]["probe_ref"], probe)
        self.assertEqual(initial["value"], {"lifecycle": "Unknown", "reason": None, "observed_at": None})
        self.assert_no_connection(listener)

        self.rejects("PermissionDenied", self.submit, probe, legacy)
        assert_connected(self.result(self.submit(probe, legacy, STATELESS_TOOL)))
        self.assertEqual(n.get(state_id), initial)

        grant = self.grant(probe)
        result = self.result(self.submit(probe, grant))
        assert_connected(result["measurement"])
        state = n.get(state_id)
        self.assertEqual(result["projection"], "Published", result)
        self.assertEqual(result["network_probe_state_ref"], state["reference"])
        self.assertEqual((state["value"]["lifecycle"], state["value"]["reason"]), ("Healthy", "TcpOpen"))
        n.stop()
        after = verify()
        self.assertEqual(after["node_id"], before["node_id"], after)
        self.assertGreaterEqual(after["database_format"], 72, after)
        self.assertFalse(after["migration_required"], after)
