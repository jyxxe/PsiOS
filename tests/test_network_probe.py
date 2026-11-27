"""Run granted probes through psiosd and real IPv4, IPv6 and DNS-over-TCP sockets."""
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import struct
import subprocess
import unittest

from support import Failure, Node, target


TOOL = "node.network_probe"
QNAME = "service.test."
POINTER = bytes((0xC0, 0x0C))


def wire_name(name):
    return b"".join(bytes([len(label)]) + label.encode("ascii") for label in name.rstrip(".").split(".")) + bytes(1)


def record(owner, kind, data):
    return owner + struct.pack("!HHIH", kind, 1, 30, len(data)) + data


def dns_reply(request, answers=(), flags=0x8180, response_id=None):
    ident, _, questions, _, _, _ = struct.unpack("!HHHHHH", request[:12])
    assert questions == 1
    return struct.pack("!HHHHHH", ident if response_id is None else response_id,
                       flags, 1, len(answers), 0, 0) + request[12:] + b"".join(answers)


class NetworkProbeTests(unittest.TestCase):
    def setUp(self):
        self.n = Node(os.environ.get("PSIOS_TEST_BIN_DIR", "target/debug"))
        self.addCleanup(self.n.close)
        self.worker = self.n.call("Agent.Register", {"name": "network-observer"})
        self.token = self.worker["token"]

    def listener(self, address="127.0.0.1", port=0, listening=True):
        family = socket.AF_INET6 if ":" in address else socket.AF_INET
        stream = socket.socket(family, socket.SOCK_STREAM)
        self.addCleanup(stream.close)
        stream.bind((address, port))
        if listening:
            stream.listen()
        stream.settimeout(5)
        return stream

    @staticmethod
    def endpoint(stream):
        host, port, *_ = stream.getsockname()
        return f"[{host}]:{port}" if ":" in host else f"{host}:{port}"

    def configure(self, *probes):
        self.n.stop()
        self.n.config.write_text(json.dumps(dict(
            json.loads(self.n.config.read_text()), network_probes=probes)))
        self.n.start()

    def resources(self):
        listed = self.n.call("Resource.List", {"kind": "NetworkProbe"})["items"]
        return [self.n.get(ref["resource_id"]) for ref in listed]

    def registered(self, name):
        resource, = [d for d in self.resources() if d["value"]["name"] == name
                     and d["value"]["lifecycle"] == "Registered"]
        return resource["reference"]

    def grant(self, *references):
        return self.n.call("Grant.Issue", {
            "principal_id": self.worker["principal_id"], "actions": [TOOL, "task.submit", "task.read"],
            "tool_targets": [ref["resource_id"] for ref in references], "units": 10, "ttl_seconds": 120,
        })

    def preparation(self, reference, grant):
        return self.n.prepare(TOOL, target(reference), token=self.token,
                              delegation=grant["delegation_ref"]["resource_id"])

    def submit(self, reference, grant):
        return self.n.call("Task.Submit", self.preparation(reference, grant), self.token)["task_id"]

    def result(self, task):
        status = self.n.wait(task)
        self.assertEqual(status["lifecycle"], "Succeeded", status)
        return self.n.call("Artifact.Read", {"reference": status["result"]["artifact_refs"][0]}, self.token)

    def rejected(self, code, operation, *args, **kwargs):
        with self.assertRaises(Failure) as denied:
            operation(*args, **kwargs)
        self.assertEqual(denied.exception.code, code, denied.exception)

    def serve_dns(self, listener, respond, family=socket.AF_INET):
        def handle():
            connection, _ = listener.accept()
            with connection, connection.makefile("rb") as reader:
                connection.settimeout(5)
                size, = struct.unpack("!H", reader.read(2))
                request = reader.read(size)
                self.assertEqual(request[12:], wire_name(QNAME) + struct.pack(
                    "!HH", 1 if family == socket.AF_INET else 28, 1))
                respond(connection, request)

        executor = ThreadPoolExecutor(max_workers=1)
        self.addCleanup(executor.shutdown)
        return executor.submit(handle)

    @staticmethod
    def write_frame(connection, reply):
        connection.sendall(struct.pack("!H", len(reply)) + reply)

    def connected(self, listener):
        connection, _ = listener.accept()
        with connection:
            connection.settimeout(2)
            self.assertEqual(connection.recv(1), b"")

    def no_connection(self, listener):
        listener.settimeout(0.15)
        try:
            with self.assertRaises(TimeoutError):
                connection, _ = listener.accept()
                connection.close()
        finally:
            listener.settimeout(5)

    def test_real_tcp_v4_v6_and_refusal(self):
        v4, v6 = self.listener(), self.listener("::1")
        refused = self.listener(listening=False)
        self.configure(
            dict(name="loopback4", endpoint=self.endpoint(v4)),
            dict(name="loopback6", endpoint=self.endpoint(v6)),
            dict(name="refused", endpoint=self.endpoint(refused)),
        )
        for name, listener in [("loopback4", v4), ("loopback6", v6), ("refused", refused)]:
            with self.subTest(name=name):
                reference = self.registered(name)
                grant = self.grant(reference)
                evidence = self.result(self.submit(reference, grant))
                self.assertEqual(evidence["target_ref"], reference)
                self.assertEqual(evidence["network_namespace_inode"], str(os.stat("/proc/self/ns/net").st_ino))
                self.assertEqual(evidence["boot_id"], Path("/proc/sys/kernel/random/boot_id").read_text().strip())
                result = evidence["observation"]
                self.assertEqual(result["endpoint"], self.endpoint(listener))
                self.assertIsNone(result["dns"])
                if name == "refused":
                    self.assertEqual((result["status"], result["tcp"]["status"]), ("Limited", "Refused"))
                    self.assertIsInstance(result["tcp"]["errno"], int)
                else:
                    self.assertEqual((result["status"], result["tcp"]["status"]), ("Connected", "Connected"))
                    self.assertIn("::1" if listener.family == socket.AF_INET6 else "127.0.0.1",
                                  result["tcp"]["local_address"])
                    self.connected(listener)

    def test_dns_tcp_verifies_cname_and_aaaa_before_connecting(self):
        for family, ip, kind in [(socket.AF_INET, "127.0.0.1", 1), (socket.AF_INET6, "::1", 28)]:
            with self.subTest(ip=ip):
                service, resolver = self.listener(ip), self.listener(ip)
                self.configure(dict(name="checked", endpoint=self.endpoint(service),
                                    dns=dict(name=QNAME, resolver=self.endpoint(resolver))))
                reference = self.registered("checked")
                grant = self.grant(reference)
                alias = "canonical.test."
                answers = [record(POINTER, 5, wire_name(alias)),
                           record(wire_name(alias), kind, socket.inet_pton(family, ip))]
                future = self.serve_dns(resolver, lambda conn, query: self.write_frame(
                    conn, dns_reply(query, answers)), family)
                evidence = self.result(self.submit(reference, grant))["observation"]
                future.result(timeout=5)
                self.assertEqual(evidence["status"], "Connected")
                self.assertEqual(evidence["dns"]["result"], dict(
                    status="Verified", response_code=0, aliases=[alias], addresses=[ip]))
                self.connected(service)

    def test_dns_mismatch_malformed_no_answer_and_deadline_never_dial(self):
        service, resolver = self.listener(), self.listener()
        alternate = self.listener("127.0.0.2", service.getsockname()[1])
        self.configure(dict(name="guarded", endpoint=self.endpoint(service), timeout_ms=400,
                            dns=dict(name=QNAME, resolver=self.endpoint(resolver))))
        reference = self.registered("guarded")
        grant = self.grant(reference)
        answers = [record(POINTER, 1, socket.inet_aton(ip)) for ip in ("127.0.0.1", "127.0.0.2")]

        def stall(connection, _):
            self.assertEqual(connection.recv(1), b"")

        def truncated_stream(connection, query):
            reply = dns_reply(query, answers[:1])
            connection.sendall(struct.pack("!H", len(reply)) + reply[:1])
            stall(connection, query)

        cycle = [record(POINTER, 5, wire_name("loop.test.")),
                 record(wire_name("loop.test."), 5, wire_name(QNAME))]
        responses = [
            ("AddressMismatch", lambda conn, query: self.write_frame(conn, dns_reply(query, answers))),
            ("InvalidResponse", lambda conn, query: self.write_frame(conn, dns_reply(
                query, answers[:1], response_id=int.from_bytes(query[:2], "big") ^ 1))),
            ("InvalidResponse", lambda conn, query: conn.sendall(struct.pack("!H", 8193))),
            ("InvalidResponse", lambda conn, query: self.write_frame(conn, dns_reply(query, cycle))),
            ("InvalidResponse", lambda conn, query: self.write_frame(
                conn, dns_reply(query, answers[:1], flags=0x8380))),
            ("NoAddress", lambda conn, query: self.write_frame(conn, dns_reply(query))),
            ("ResponseError", lambda conn, query: self.write_frame(conn, dns_reply(query, flags=0x8183))),
            ("TimedOut", stall),
            ("TimedOut", truncated_stream),
        ]
        for status, handler in responses:
            with self.subTest(status=status):
                future = self.serve_dns(resolver, handler)
                result = self.result(self.submit(reference, grant))["observation"]
                future.result(timeout=5)
                self.assertEqual(result["dns"]["result"]["status"], status)
                self.assertEqual(result["tcp"], dict(status="Skipped", reason="DnsUnverified"))
                self.assertNotEqual(result["status"], "Connected")
                if status == "TimedOut":
                    self.assertGreaterEqual(result["elapsed_ms"], 400)
                    self.assertLess(result["elapsed_ms"], 5000)
        self.no_connection(service)
        self.no_connection(alternate)

    def test_authority_fences_endpoint_and_measurement_after_dns(self):
        for name, verified, cancelled in (("revoked", True, False),
                                          ("revoked_without_endpoint", False, False),
                                          ("cancelled", True, True)):
            with self.subTest(case=name):
                service, resolver = self.listener(), self.listener()
                self.configure(dict(name=name, endpoint=self.endpoint(service), timeout_ms=5000,
                                    dns=dict(name=QNAME, resolver=self.endpoint(resolver))))
                reference = self.registered(name)
                grant = self.grant(reference)
                task_id = self.submit(reference, grant)
                connection, _ = resolver.accept()
                with connection, connection.makefile("rb") as reader:
                    connection.settimeout(5)
                    size, = struct.unpack("!H", reader.read(2))
                    request = reader.read(size)
                    self.assertEqual(request[12:], wire_name(QNAME) + struct.pack("!HH", 1, 1))
                    running = self.n.get(task_id)
                    self.assertEqual(running["value"]["lifecycle"], "Running")
                    if cancelled:
                        self.n.call("Task.Cancel", target(running["reference"]))
                    else:
                        self.n.call("Grant.Revoke", target(grant["grant_ref"]))
                    answers = [record(POINTER, 1, socket.inet_aton("127.0.0.1"))] if verified else []
                    self.write_frame(connection, dns_reply(request, answers))

                status = self.n.wait(task_id)
                self.no_connection(service)
                expected = ("Cancelled", ["Fenced"]) if cancelled else ("Failed", ["PermissionDenied"])
                self.assertEqual((status["lifecycle"], [item["type"] for item in status["conditions"]]),
                                 expected, status)
                self.assertEqual(status["result"], {"availability": "Unavailable", "artifact_refs": []})
                self.n.stop(signal.SIGKILL)
                self.n.start()
                self.assertEqual(self.n.wait(task_id), status)
                with closing(sqlite3.connect(self.n.state / "state.db")) as db:
                    self.assertIsNone(db.execute(
                        "SELECT 1 FROM current WHERE kind='Artifact' AND spec->>'task_id'=?",
                        (task_id,),
                    ).fetchone())

    def test_authority_fences_stale_target_after_configuration_restart(self):
        service, replacement, disallowed = self.listener(), self.listener(), self.listener()
        original = dict(name="managed", endpoint=self.endpoint(service))
        other = dict(name="other", endpoint=self.endpoint(disallowed))
        self.configure(original, other)
        first = self.registered("managed")
        other_ref = self.registered("other")
        grant = self.grant(first)
        self.rejected("PermissionDenied", self.n.call, "NetworkProbe.Get", {"id": first["resource_id"]})
        self.rejected("PermissionDenied", self.n.call, "NetworkProbe.Get", {"id": other_ref["resource_id"]}, self.token)
        self.rejected("PermissionDenied", self.submit, other_ref, grant)
        self.rejected("InvalidArgument", self.n.prepare, TOOL, dict(
            target(first), endpoint=self.endpoint(disallowed)), token=self.token,
            delegation=grant["delegation_ref"]["resource_id"])
        view = self.n.call("NetworkProbe.Get", {"id": first["resource_id"]}, self.token)
        self.assertEqual(view["configuration"]["endpoint"], self.endpoint(service))
        self.assertNotIn(self.endpoint(service), json.dumps(self.n.get(first["resource_id"])))

        def scheduling(mode):
            ref = self.n.call("Node.Get", {})["reference"]
            return self.n.call("Node.Configure", dict(scheduling=mode, expected_resource_version=ref["version"]))

        scheduling("Cordoned")
        pending = self.submit(first, grant)
        self.assertEqual(self.n.call("Task.Get", {"id": pending})["lifecycle"], "Queued")
        self.n.stop(signal.SIGKILL)
        self.configure(dict(original, endpoint=self.endpoint(replacement)), other)
        new = self.registered("managed")
        self.assertNotEqual(new["resource_id"], first["resource_id"])
        retired = self.n.call("NetworkProbe.Get", {"id": first["resource_id"]}, self.token)
        self.assertEqual((retired["state"]["lifecycle"], retired["configuration"]), ("Unregistered", None))
        self.rejected("PermissionDenied", self.submit, new, grant)
        scheduling("Enabled")
        failed = self.n.wait(pending)
        self.assertEqual((failed["lifecycle"], failed["outcome_knowledge"]), ("Failed", "NotStarted"))
        for listener in (service, replacement, disallowed):
            self.no_connection(listener)

        new_grant = self.grant(new)
        self.configure(dict(original, endpoint=self.endpoint(replacement), timeout_ms=700), other)
        current = self.registered("managed")
        self.assertEqual(current["resource_id"], new["resource_id"])
        self.assertNotEqual(current["version"], new["version"])
        self.rejected("Conflict", self.submit, new, new_grant)
        self.assertEqual(self.result(self.submit(current, new_grant))["observation"]["status"], "Connected")
        self.connected(replacement)
        self.n.call("Grant.Revoke", target(new_grant["grant_ref"]))
        self.rejected("PermissionDenied", self.n.call, "NetworkProbe.Get", {"id": current["resource_id"]}, self.token)

    def test_invalid_configuration_refuses_to_start(self):
        self.n.stop()
        base = json.loads(self.n.config.read_text())
        valid = dict(name="test", endpoint="127.0.0.1:8080")
        cases = [
            [valid, valid],
            [dict(valid, endpoint="127.0.0.1:0")],
            [dict(valid, endpoint="0.0.0.0:8080")],
            [dict(valid, endpoint="255.255.255.255:8080")],
            [dict(valid, endpoint="[ff02::1]:8080")],
            [dict(valid, endpoint="[::ffff:127.0.0.1]:8080")],
            [dict(valid, endpoint="[fe80::1]:8080")],
            [dict(valid, timeout_ms=10)],
            [dict(valid, dns=dict(name="relative-name", resolver=self.endpoint(self.listener())))],
        ]
        for probes in cases:
            self.n.config.write_text(json.dumps(dict(base, network_probes=probes)))
            process = subprocess.run(self.n.daemon + ["check"], capture_output=True, check=False)
            self.assertNotEqual(process.returncode, 0)
            self.assertIn(b"InvalidArgument", process.stderr)
