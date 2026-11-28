"""Run a pinned CPU inference server inside a networkless native PsiOS guest."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

from local_inference import GENERATION_TOKENS, PINS
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha


def write_private(console, path, value):
    body = shlex.quote(value)
    console.run(f"umask 077; printf '%s' {body} > {path}")


def install_inputs(console, server_digest, model_digest):
    module_dir = "/home/psios/local-inference"
    console.run(
        f"set -eu; mkdir -m 700 /run/inference-input {module_dir}; "
        "state=$(awk '$2 == \"/var/lib\" { print $1 }' /proc/mounts); "
        "case \"$state\" in /dev/vd?2) ;; *) exit 1 ;; esac; "
        "set -- /dev/vd?; test \"$#\" = 2; "
        "test \"$1\" != \"${state%2}\" || shift; "
        "mount -t ext4 -o ro,noload,nodev,nosuid,noexec \"$1\" /run/inference-input; "
        "trap 'cd /; umount /run/inference-input' EXIT; cd /run/inference-input; "
        f"printf '%s  %s\\n' {server_digest} llama-server {model_digest} model.gguf | sha256sum -c -; "
        f"cp llama-server model.gguf {module_dir}/; chmod 0700 {module_dir}/llama-server; "
        f"chmod 0600 {module_dir}/model.gguf; chown -R 1000:1000 {module_dir}; sync -f {module_dir}"
    )


def drive_model(console):
    console.run("umask 077; mkdir local-inference/journal; mkdir /tmp/local-inference")
    console.run(
        "umask 077; ./local-inference/llama-server -m ./local-inference/model.gguf "
        "--host /tmp/local-inference/inference.sock --alias psios-small -t 2 "
        "-c 2048 -np 1 -ngl 0 --no-webui --seed 1 --no-cache-prompt "
        "</dev/null >/tmp/local-inference/server.log 2>&1 & echo $! >/tmp/local-inference/server.pid"
    )
    console.run(
        "for n in $(seq 1 200); do "
        "test -S /tmp/local-inference/inference.sock && grep -q 'model loaded' /tmp/local-inference/server.log && exit 0; "
        "kill -0 $(cat /tmp/local-inference/server.pid) 2>/dev/null || exit 1; sleep 0.2; done; exit 1"
    )
    provider = call(console, "Agent.Register", {"name": "native-cpu-inference"})
    agent = call(console, "Agent.Register", {"name": "native-inventory-reader"})
    write_private(console, "local-inference/provider.token", provider["token"])
    write_private(console, "local-inference/agent.token", agent["token"])
    call(console, "Grant.Issue", dict(principal_id=provider["principal_id"], actions=["model.serve"],
                                     units=1, ttl_seconds=900))
    model = call(console, "Model.Register", dict(name="SmolLM2-135M-Instruct", revision="Q4_K_M",
                 provider_principal_id=provider["principal_id"], lease_seconds=270, max_context_bytes=16384,
                 max_output_bytes=4096, max_in_flight=1))
    skill = call(console, "Skill.Publish", dict(name="read-native-inventory", version="1",
                 tools=["node.inventory"],
                 instructions="Call node.inventory with an empty input to obtain the local hardware inventory.",
                 license="MIT", source="operator"))
    node = call(console, "Node.Get", {})["reference"]["resource_id"]
    grant = call(console, "Grant.Issue", dict(principal_id=agent["principal_id"],
                 actions=["session.run", "task.submit", "task.read", "node.inventory"],
                 tool_targets=[node, model["resource_id"], skill["resource_id"]],
                 units=2, model_calls=2, ttl_seconds=900))
    # This baseline binary also runs in software-emulated guests without KVM.
    config = dict(socket="/tmp/local-inference/inference.sock", server_uid=1000,
                  model_ref=model, model_alias="psios-small", max_tokens=GENERATION_TOKENS,
                  timeout_ms=240000)
    write_private(console, "local-inference/config.json", json.dumps(config))
    write_private(console, "local-inference/model.json", json.dumps(model))
    boot_id = console.run("cat /proc/sys/kernel/random/boot_id")
    memory_bytes = int(console.run("awk '$1 == \"MemTotal:\" { print $2 }' /proc/meminfo")) * 1024
    agent_file = "local-inference/agent.token"
    provider_cli = "psiosctl --token-file local-inference/provider.token"
    settings = "--local-model local-inference/config.json --decision-dir local-inference/journal"
    receipts = []
    for queued in (False, True):
        session = call(console, "Session.Open", dict(key=uuid.uuid4().hex,
                       goal="Call node.inventory now with an empty input object.", skill_ref=skill,
                       model_ref=model, delegation_id=grant["delegation_ref"]["resource_id"],
                       context_policy="ReferencesOnly", max_turns=2,
                       **({"provider_queue": True} if queued else {})), agent_file)
        session_id = session["reference"]["resource_id"]
        command = ("model-worker local-inference/model.json --once --wait-ms 5000" if queued
                   else f"model-step {shlex.quote(session_id)} --key native-first")
        start = time.monotonic()
        output = console.run(f"{provider_cli} {command} {settings}; printf '\\n%d\\n' \"$?\"", timeout=260)
        response, _, status = output.rpartition("\n")
        assert status == "0", (response, console.run("tail -n 12 /tmp/local-inference/server.log"))
        receipt = json.loads(response)
        assert receipt["session_ref"]["resource_id"] == session_id, receipt
        task = call(console, "Task.Wait", {"id": receipt["task_id"], "timeout_ms": 8000}, agent_file)
        assert task["lifecycle"] == "Succeeded", task
        run = call(console, "Run.Get", {"id": task["current_run_id"]})
        execution = call(console, "Resource.Resolve", {"reference": run["execution_snapshot_ref"]})["value"]
        assert execution["model_ref"] == model
        assert [tool["resource_id"] for tool in execution["tool_refs"]] == ["tool-node.inventory"]
        report = call(console, "Artifact.Read", {"reference": task["result"]["artifact_refs"][0]}, agent_file)
        assert report["boot_id"] == boot_id and report["memory_bytes"]["MemTotal"] == memory_bytes, report
        usage = call(console, "Resource.Resolve", {"reference": receipt["usage_ref"]})["value"]
        assert usage["provider_principal_id"] == provider["principal_id"]
        assert usage["budget_ref"] == grant["budget_ref"]
        tokens = usage["reported_tokens"]
        assert tokens["input_tokens"] > 0 and 0 < tokens["output_tokens"] <= GENERATION_TOKENS
        receipts.append((receipt, task["result"]))
        route = "queue" if queued else "direct"
        print(f"PASS native {route} inference submitted a scoped inventory Task with usage {tokens} "
              f"after {time.monotonic() - start:.1f}s", flush=True)
    budget = call(console, "Budget.Get", {"id": grant["budget_ref"]["resource_id"]}, agent_file)
    assert (budget["model_calls_used"], budget["spent"]) == (2, 2), budget
    console.run("kill $(cat /tmp/local-inference/server.pid); rm -f /tmp/local-inference/inference.sock")
    direct = receipts[0][0]
    replay = json.loads(console.run(f"{provider_cli} model-step {shlex.quote(direct['session_ref']['resource_id'])} "
                                   f"--key native-first {settings}"))
    assert replay == direct
    ordinary = json.loads(console.run("printf '{}' | psiosctl submit system.status --key native-no-inference"))
    assert call(console, "Task.Wait", {"id": ordinary["task_id"], "timeout_ms": 5000})["lifecycle"] == "Succeeded"
    print("PASS direct receipt replay and ordinary management after inference stopped", flush=True)
    return node, receipts


def poweroff(console, root_password):
    console.send("exit")
    console.login("root", root_password)
    console.send("poweroff")
    assert console.vm.wait(35) == 0


def exercise(args, work):
    assert args.weights.stat().st_size == PINS["model"]["size_bytes"]
    model_digest = sha(args.weights)
    assert model_digest == PINS["model"]["sha256"]
    server_digest = sha(args.server)
    operator, operator_hash = password(work, "operator.hash")
    root, root_hash = password(work, "root.hash")
    disk = work / "native.raw"
    for name in ("CODE", "VARS"):
        shutil.copyfile(args.ovmf_dir / f"OVMF_{name}_4M.fd", work / f"{name.lower()}.fd")
    deployed = json.loads(subprocess.check_output([
        sys.executable, args.installer, disk, "--kernel-iso", args.kernel_iso, "--core-archive", args.archive,
        "--core-sha256", sha(args.archive), "--console-hash", operator_hash, "--root-console-hash", root_hash,
        "--home-mib", "192"], timeout=240))
    assert deployed["initial_image_sha256"] == sha(disk)
    store = work / "model-inputs"
    store.mkdir(mode=0o700)
    for source, name in ((args.server, "llama-server"), (args.weights, "model.gguf")):
        shutil.copyfile(source, store / name)
    inputs = work / "inference-inputs.raw"
    volume_id = str(uuid.uuid4())
    with inputs.open("xb") as handle:
        handle.truncate(160 * 1024 * 1024)
    subprocess.run(["mkfs.ext4", "-q", "-F", "-U", volume_id, "-d", store, inputs], check=True, timeout=90)
    input_digest = sha(inputs)

    with boot(work, disk, extra_disk=inputs, pace_commands=True) as console:
        console.login("root", root)
        install_inputs(console, server_digest, model_digest)
        console.send("exit")
        console.login("psios", operator)
        console.run("test -d /sys/class/net/lo && "
                    "for interface in /sys/class/net/*; do test ! -e \"$interface/device\" || exit 1; done")
        assert console.run("stat -c '%u:%g:%a' local-inference") == "1000:1000:700"
        print("PASS installed verified engine and weights in a guest without a network device", flush=True)
        node, receipts = drive_model(console)
        poweroff(console, root)
    assert sha(inputs) == input_digest
    check_clean_shutdown(disk)
    empty_media = work / "empty.raw"
    with empty_media.open("xb") as handle:
        handle.truncate(1024 * 1024)
    # Replace the transfer media while keeping the boot disk's PCI slot.
    with boot(work, disk, extra_disk=empty_media, pace_commands=True) as console:
        console.login("psios", operator)
        assert call(console, "Node.Get", {})["reference"]["resource_id"] == node
        assert console.run("sha256sum local-inference/model.gguf").split()[0] == model_digest
        assert console.run("sha256sum local-inference/llama-server").split()[0] == server_digest
        console.run("test ! -e /tmp/local-inference/inference.sock")
        for receipt, result in receipts:
            current = call(console, "Task.Wait", {"id": receipt["task_id"], "timeout_ms": 5000})
            assert current["lifecycle"] == "Succeeded" and current["result"] == result, current
        poweroff(console, root)
    check_clean_shutdown(disk)
    print("PASS native model files and original Tasks persisted across reboot", flush=True)


if __name__ == "__main__":
    cli = argparse.ArgumentParser(description=__doc__)
    for name in ("kernel-iso", "archive", "server", "weights"):
        cli.add_argument(f"--{name}", type=Path, required=True)
    cli.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    cli.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    with tempfile.TemporaryDirectory(prefix="psios-native-inference-") as directory:
        exercise(cli.parse_args(), Path(directory))
