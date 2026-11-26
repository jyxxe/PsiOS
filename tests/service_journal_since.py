"""Read a real systemd user journal forward through scoped Unix Tasks."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
from time import monotonic, sleep
import uuid

from service_journal import refuses
from support import Failure, Node, key, target


TAIL = "service.logs"
SINCE = "service.logs_since"
CTL = ("systemctl", "--user", "--no-pager", "--no-ask-password")


def native_cursor_with_changed_checksum(cursor):
    def change(match):
        first = "a" if match[0][0] != "a" else "b"
        return first + match[0][1:]

    modified, count = re.subn(r"(?<=;x=)[0-9a-f]+$", change, cursor)
    assert count == 1 and modified != cursor, cursor
    return modified


def configure(node, config, units):
    node.stop()
    config["services"] = [{"unit": unit, "manager": "user", "allow_control": False} for unit in units]
    node.config.write_text(json.dumps(config))
    node.start()


def journal_messages(unit, count):
    command = [
        "/usr/bin/journalctl", "--user", "--boot", "--quiet", "--no-pager",
        "--output=json", "--output-fields=MESSAGE", f"--lines={count}",
        "--", f"_SYSTEMD_USER_UNIT={unit}", f"_SYSTEMD_OWNER_UID={os.geteuid()}",
    ]
    output = subprocess.check_output(command, timeout=8)
    return [json.loads(line).get("MESSAGE") for line in output.splitlines() if line]


def write_messages(unit, input_path, messages):
    input_path.write_text("\n".join(messages) + "\n")
    subprocess.run([*CTL, "start", "--", unit], check=True, timeout=15)
    deadline = monotonic() + 10
    while True:
        observed = journal_messages(unit, len(messages))
        if observed == messages:
            return
        assert monotonic() < deadline, (unit, observed, messages)
        sleep(0.05)


def rows(receipt):
    return [entry["message"]["data"] for entry in receipt["entries"]]


def assert_page(receipt, unit, service, boot, limit, previous, expected, has_more):
    assert receipt["service_ref"] == service and receipt["boot_id"] == boot, receipt
    assert receipt["unit"] == unit and receipt["manager"] == "user" and receipt["limit"] == limit, receipt
    assert receipt["after_cursor"] == previous and receipt["has_more"] is has_more, receipt
    assert rows(receipt) == expected, receipt
    cursors = [entry["journal_cursor"] for entry in receipt["entries"]]
    assert all(isinstance(cursor, str) and cursor for cursor in cursors), receipt
    assert previous not in cursors and len(cursors) == len(set(cursors)), receipt
    assert receipt["next_cursor"] == (cursors[-1] if cursors else previous), receipt
    assert all(entry["message"]["encoding"] == "Utf8"
               and int(entry["realtime_usec"]) > 0 for entry in receipt["entries"]), receipt
    return cursors


class Reader:
    def __init__(self, node, targets):
        self.node = node
        agent = node.call("Agent.Register", {"name": "journal-continuation-reader"})
        self.token = agent["token"]
        self.grant_request = {
            "principal_id": agent["principal_id"], "units": 32, "ttl_seconds": 300, "tool_targets": targets,
        }
        self.tail_grant = self.grant(TAIL)
        self.since_grant = None

    def grant(self, tool, **changes):
        request = {**self.grant_request, "actions": ["task.submit", "task.read", tool], **changes}
        return self.node.call("Grant.Issue", request)

    def prepare(self, tool, params, grant):
        return self.node.prepare(tool, params, token=self.token, delegation=grant["delegation_ref"]["resource_id"])

    def submit(self, tool, params, grant):
        request = self.prepare(tool, params, grant)
        return self.node.call("Task.Submit", request, self.token)["task_id"]

    def result(self, task_id):
        task = self.node.wait(task_id)
        assert (task["lifecycle"], task["outcome_knowledge"]) == ("Succeeded", "Known"), task
        artifact, = task["result"]["artifact_refs"]
        return artifact, self.node.call("Artifact.Read", {"reference": artifact}, self.token)

    def tail(self, service):
        _, receipt = self.result(self.submit(TAIL, {**target(service), "limit": 1}, self.tail_grant))
        return receipt

    def page(self, params, *, cli=False):
        if not cli:
            return self.result(self.submit(SINCE, params, self.since_grant))
        input_path = self.node.base / "continuation-request.json"
        token_path = self.node.base / "reader.token"
        input_path.write_text(json.dumps(params))
        with token_path.open("x") as file:
            token_path.chmod(0o600)
            file.write(self.token)
        task = self.node.cli(token_path, "submit", SINCE, "--key", key(),
                             "--delegation", self.since_grant["delegation_ref"]["resource_id"], input_path)
        return self.result(task["task_id"])

    def rejected(self, code, params):
        try:
            task_id = self.submit(SINCE, params, self.since_grant)
        except Failure as error:
            assert error.code == code, error
            return
        task = self.node.wait(task_id)
        assert task["lifecycle"] == "Failed" and task["conditions"][0]["type"] == code, task
        assert not task["result"]["artifact_refs"], task


def exercise(node, unit, addressed, other, outside):
    config = json.loads(node.config.read_text())
    configure(node, config, (unit, other))
    service_id, other_id = f"service-{unit}", f"service-{other}"
    service = node.get(service_id)["reference"]
    outside_service = node.get(other_id)["reference"]
    boot = node.call("Node.Get", {})["state"]["boot_id"]
    reader = Reader(node, [service_id, other_id])

    trace = key()
    anchor_message, outside_message = f"{trace}-anchor", f"{trace}-other-anchor"
    write_messages(unit, addressed, [anchor_message])
    write_messages(other, outside, [outside_message])
    anchor = reader.tail(service)
    foreign = reader.tail(outside_service)
    assert rows(anchor) == [anchor_message] and rows(foreign) == [outside_message], (anchor, foreign)
    assert anchor["service_ref"] == service and foreign["service_ref"] == outside_service, (anchor, foreign)
    cursor = anchor["entries"][0]["journal_cursor"]
    wrong_cursor = foreign["entries"][0]["journal_cursor"]
    assert cursor != wrong_cursor and anchor["boot_id"] == foreign["boot_id"] == boot, (anchor, foreign)
    request = {**target(service), "boot_id": boot, "after_cursor": cursor, "limit": 32}

    refuses("PermissionDenied", lambda: node.prepare(SINCE, request))
    refuses("PermissionDenied", lambda: reader.prepare(SINCE, request, reader.tail_grant))
    refuses("InvalidArgument", lambda: reader.grant(SINCE, tool_targets=None))
    reader.since_grant = reader.grant(SINCE)
    first_unit_only = reader.grant(SINCE, tool_targets=[service_id])
    outside_request = {**request, **target(outside_service), "after_cursor": wrong_cursor}
    refuses("PermissionDenied", lambda: reader.submit(SINCE, outside_request, first_unit_only))
    refuses("PermissionDenied", lambda: reader.prepare(TAIL, target(service), reader.since_grant))
    for illegal in ({"limit": 33}, {"after_cursor": ""}, {"after_cursor": "z" * 1025},
                    {"after_cursor": cursor + "\n"}, {"boot_id": "invalid-boot"}):
        reader.rejected("InvalidArgument", {**request, **illegal})

    expected = [f"{trace}-main-{index:02d}" for index in range(43)]
    write_messages(unit, addressed, expected)
    write_messages(other, outside, [f"{trace}-other-forever-private"])
    for unusable in ({"after_cursor": wrong_cursor},
                     {"after_cursor": native_cursor_with_changed_checksum(cursor)},
                     {"boot_id": str(uuid.uuid4())}):
        reader.rejected("CursorExpired", {**request, **unusable})

    first_artifact, first_page = reader.page(request, cli=True)
    first = assert_page(first_page, unit, service, boot, 32, cursor, expected[:32], True)
    cursor = first_page["next_cursor"]
    node.stop(signal.SIGKILL)
    node.start()
    assert node.get(service_id)["reference"] == service
    assert node.call("Artifact.Read", {"reference": first_artifact}, reader.token) == first_page

    second_artifact, second_page = reader.page({**request, "after_cursor": cursor, "limit": 8})
    second = assert_page(second_page, unit, service, boot, 8, cursor, expected[32:40], True)
    cursor = second_page["next_cursor"]
    _, third_page = reader.page({**request, "after_cursor": cursor, "limit": 3})
    third = assert_page(third_page, unit, service, boot, 3, cursor, expected[40:], False)
    cursor = third_page["next_cursor"]
    combined = first + second + third
    assert len(combined) == len(set(combined)) == 43, combined
    assert node.call("Artifact.Read", {"reference": second_artifact}, reader.token) == second_page

    final_request = {**target(service), "boot_id": boot, "after_cursor": cursor}
    _, idle = reader.page(final_request)
    assert_page(idle, unit, service, boot, 16, cursor, [], False)
    later = [f"{trace}-next-one", f"{trace}-next-two"]
    write_messages(other, outside, [f"{trace}-other-late"])
    write_messages(unit, addressed, later)
    _, resumed = reader.page({**final_request, "limit": 2})
    assert_page(resumed, unit, service, boot, 2, cursor, later, False)
    cursor = resumed["next_cursor"]
    _, idle_again = reader.page({**final_request, "after_cursor": cursor})
    assert_page(idle_again, unit, service, boot, 16, cursor, [], False)

    configure(node, config, (other,))
    retired = node.get(service_id)
    assert retired["value"]["lifecycle"] == "Unregistered", retired
    reader.rejected("Conflict", {**final_request, "after_cursor": cursor})
    reader.rejected("PreconditionFailed", {**final_request, **target(retired["reference"]), "after_cursor": cursor})
    configure(node, config, (unit, other))
    fresh = node.get(service_id)["reference"]
    assert fresh["version"] != service["version"], (fresh, service)
    reader.rejected("Conflict", {**final_request, "after_cursor": cursor})
    live = {**final_request, **target(fresh), "after_cursor": cursor}
    _, final = reader.page(live)
    assert_page(final, unit, fresh, boot, 16, cursor, [], False)

    prepared = reader.prepare(SINCE, live, reader.since_grant)
    node.call("Grant.Revoke", target(reader.since_grant["grant_ref"]))
    refuses("PermissionDenied", lambda: reader.prepare(SINCE, live, reader.since_grant))
    refuses("PermissionDenied", lambda: node.call("Task.Submit", prepared, reader.token))
    assert rows(reader.tail(fresh)) == later[-1:]
    node.stop()
    report = json.loads(subprocess.check_output([*node.daemon, "verify"], timeout=20))
    assert report["migration_required"] is False and report["sqlite_integrity"] == "Verified", report
    assert report["foreign_keys"] == "Verified" and report["witness"] == "Confirmed", report
    print("PASS real scoped journal continuation across pages, Core restart, config changes and revocation")


def main(binaries):
    node = Node(binaries)
    units = Path(f"/run/user/{os.geteuid()}/systemd/user")
    registered = []
    try:
        units.mkdir(mode=0o700, parents=True, exist_ok=True)
        for _ in range(2):
            unit = f"psios-journal-page-{key()}.service"
            payload = node.base / (unit + ".messages")
            payload.touch(mode=0o600)
            path = units / unit
            with path.open("x") as definition:
                registered.append((path, payload))
                definition.write("[Unit]\nDescription=PsiOS journal continuation fixture\n"
                                 "[Service]\nType=oneshot\nStandardOutput=journal\n"
                                 f"ExecStart=/bin/cat {json.dumps(str(payload))}\n")
        subprocess.run([*CTL, "daemon-reload"], check=True, timeout=15)
        (first, addressed), (second, outside) = registered
        exercise(node, first.name, addressed, second.name, outside)
    finally:
        try:
            try:
                if registered:
                    subprocess.run([*CTL, "stop", "--", *(path.name for path, _ in registered)],
                                   capture_output=True, timeout=15)
            finally:
                for path, _ in registered:
                    path.unlink(missing_ok=True)
                if registered:
                    subprocess.run([*CTL, "daemon-reload"], check=True, timeout=15)
        finally:
            node.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    main(parser.parse_args().bin_dir)
