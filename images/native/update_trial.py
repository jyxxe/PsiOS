"""Boot and independently verify a staged native update in a private overlay."""

import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import time
from types import SimpleNamespace
import uuid

import create


MIB = create.MIB
OVERLAY_LIMIT = 512 * MIB
COPY_LIMIT_MIB = 128
CONSOLE_LIMIT = 128 * 1024
REPORT_LIMIT = 64 * 1024
TRIAL_MARKER = b"PSIOS-TRIAL/1"
RESCUE_MARKER = b"PSIOS-RESCUE/1"
SECURE_BOOT_MARKER = b"PSIOS-SECURE-BOOT/1"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(stream):
    stream.seek(0)
    return hashlib.file_digest(stream, "sha256").hexdigest()


def unique_fields(pairs):
    result = dict(pairs)
    require(len(result) == len(pairs), "duplicate trial report field")
    return result


def json_report(body):
    require(len(body) <= REPORT_LIMIT, "trial JSON report exceeds size limit")
    report = json.loads(body.decode("ascii"), object_pairs_hook=unique_fields)
    require(isinstance(report, dict), "trial report is not a JSON object")
    return report


def receipt_lineage(prepared):
    candidate, trial = prepared.candidate_receipt, prepared.trial_receipt
    signed = "secure_boot" in candidate
    candidate_formats = (3, 4, 5, 6) if signed else (2, 4, 5, 6)
    require(trial.get("boot_mode") == create.TRIAL_BOOT_MODE and trial.get("format") == create.TRIAL_RECEIPT_FORMAT
            and candidate.get("format") in candidate_formats and "boot_mode" not in candidate,
            "expected native trial and normal candidate receipts")
    if signed:
        trust = candidate["secure_boot"]
        require(isinstance(trust, dict) and set(trust) == {"scheme", "certificate_sha256", "efi_stub_sha256"}
                and trust["scheme"] == create.boot_media.SECURE_BOOT_SCHEME
                and all(isinstance(trust[name], str) and re.fullmatch(r"[0-9a-f]{64}", trust[name])
                        for name in ("certificate_sha256", "efi_stub_sha256"))
                and trial.get("secure_boot") == trust
                and candidate.get("secure_boot_enforcement_required") is True
                and trial.get("secure_boot_enforcement_required") is True,
                "signed trial and candidate trust receipts or enforcement requirements disagree")
    else:
        require("secure_boot" not in trial and "secure_boot_enforcement_required" not in candidate
                and "secure_boot_enforcement_required" not in trial,
                "unsigned candidate has incompatible trial trust")
    require(all(trial[name] == candidate[name] for name in
                ("geometry_mib", "state_uuid", "home_uuid", "source_commit", "upgrade"))
            and all(trial.get(name) == candidate.get(name) for name in ("network_profile", "core_limits"))
            and trial["deployment_manifest_sha256"] != candidate["deployment_manifest_sha256"],
            "trial and candidate deployment lineage differs")
    geometry = trial["geometry_mib"]
    layout = create.Layout(geometry["state_mib"], geometry["home_mib"])
    require(geometry == layout.receipt(), "invalid trial geometry")
    for field in ("state_uuid", "home_uuid", "trial_disk_uuid"):
        require(str(uuid.UUID(trial[field])) == trial[field], "invalid trial UUID")
    require(len({trial[name] for name in ("state_uuid", "home_uuid", "trial_disk_uuid")}) == 3,
            "trial UUIDs are not distinct")
    upgrade = candidate["upgrade"]
    source = upgrade["verified_authority"]
    rehearsal = source["migration_rehearsal"]
    configuration = hashlib.sha256(prepared.configuration).hexdigest()
    target_format = rehearsal.get("target_format")
    require(upgrade["normalized_source_sha256"] == prepared.normalized_source_sha256
            and isinstance(source.get("node_id"), str) and source["node_id"]
            and rehearsal.get("status") == "Verified" and rehearsal.get("configuration_sha256") == configuration
            and type(source.get("database_format")) is int and type(target_format) is int
            and 0 < source["database_format"] == rehearsal.get("source_format") <= target_format
            and source.get("migration_required") is (source["database_format"] < target_format),
            "incomplete original authority or migration rehearsal")
    return layout, configuration, source["node_id"], target_format


def verify_base_images(candidate, trial, prepared, layout):
    size = layout.image_mib * MIB
    require(os.fstat(candidate.fileno()).st_size == os.fstat(trial.fileno()).st_size == size,
            "unexpected candidate or trial image size")
    candidate_hash, trial_hash = hashlib.sha256(), hashlib.sha256()
    data_start = layout.partitions[1][0]
    data_end = data_start + layout.state_mib + layout.home_mib
    for block in range(layout.image_mib):
        left, right = candidate.read(MIB), trial.read(MIB)
        require(len(left) == len(right) == MIB, "incomplete candidate or trial image")
        candidate_hash.update(left)
        trial_hash.update(right)
        if data_start <= block < data_end:
            require(left == right, "trial starts with different state or home bytes than the candidate")
    require(candidate_hash.hexdigest() == prepared.candidate_receipt["initial_image_sha256"]
            and trial_hash.hexdigest() == prepared.trial_receipt["initial_image_sha256"],
            "candidate or trial image differs from its build receipt")


def virtual_machine():
    return ["qemu-system-x86_64", "-nodefaults", "-no-user-config", "-no-reboot", "-accel", "tcg,thread=single",
            "-m", "768", "-smp", "1", "-display", "none", "-monitor", "none", "-nic", "none"]


def readonly_disk(fd, name):
    return ["-add-fd", f"fd={fd},set=1",
            "-blockdev", f"driver=file,node-name={name}-file,filename=/dev/fdset/1,read-only=on,locking=on",
            "-blockdev", f"driver=raw,node-name={name},file={name}-file,read-only=on"]


def virtio_disk(name):
    return ["-device", f"virtio-blk-pci,drive={name},werror=report,rerror=report"]


def run_guest(work, command, fds, purpose, *, file_limit=None):
    # Keep the serial UART separate from QEMU diagnostics, and never allow a
    # broken guest to fill memory or consume the caller's entire run.
    with subprocess.Popen(command, cwd=work, pass_fds=fds, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          preexec_fn=file_limit) as process:
        deadline = time.monotonic() + 195
        with selectors.DefaultSelector() as selector:
            serial = bytearray()
            diagnostics = 0
            selector.register(process.stdout, selectors.EVENT_READ)
            selector.register(process.stderr, selectors.EVENT_READ)
            try:
                while selector.get_map():
                    wait = deadline - time.monotonic()
                    require(wait > 0, f"{purpose} did not shut down before its deadline")
                    for key, _ in selector.select(wait):
                        block = os.read(key.fd, 8192)
                        if not block:
                            selector.unregister(key.fileobj)
                        elif key.fileobj == process.stdout:
                            serial.extend(block)
                        else:
                            diagnostics += len(block)
                        require(len(serial) + diagnostics <= CONSOLE_LIMIT, f"{purpose} output exceeds size limit")
                result = process.wait(timeout=max(0, deadline - time.monotonic()))
                require(result == 0, f"{purpose} QEMU exited with status {result}")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
        return bytes(serial).replace(b"\r\n", b"\n")


def verify_ready(serial, expected_manifest, *, secure_boot_required=False):
    prefix = TRIAL_MARKER + b" "
    lines = [line for line in serial.split(b"\n")[:-1] if line.startswith(prefix)]
    require(serial.count(TRIAL_MARKER) == len(lines) == 1, "trial did not emit one complete boot report")
    report = json_report(lines[0][len(prefix):])
    if report.get("status") == "Failed":
        stage = report.get("stage")
        if isinstance(stage, str) and re.fullmatch(r"[a-z0-9-]{1,64}", stage):
            raise ValueError(f"native trial failed at {stage}")
    require(report == {"status": "Ready", "deployment_manifest_sha256": expected_manifest},
            "native trial did not attest the expected Ready deployment")
    if secure_boot_required:
        enforced = b"\n" + SECURE_BOOT_MARKER + b" Enforced\n"
        require(serial.count(SECURE_BOOT_MARKER) == 1 and serial.count(enforced) == 1
                and serial.index(enforced) < serial.index(prefix),
                "native trial did not attest firmware Secure Boot enforcement")


def boot_trial(work, disk, code, vars_file, receipt, size):
    writable_vars = work / "vars.fd"
    variables = vars_file.read(4 * MIB + 1)
    code_size = os.fstat(code.fileno()).st_size
    require(code_size > 0 and variables and code_size + len(variables) == 4 * MIB,
            "expected a matching 4 MiB OVMF code and variable-store pair")
    with writable_vars.open("xb") as saved:
        writable_vars.chmod(0o600)
        saved.write(variables)

    limit = create.file_limit(OVERLAY_LIMIT)
    subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "changes.qcow2", str(size)],
                   cwd=work, check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                   timeout=20, preexec_fn=limit)
    (work / "changes.qcow2").chmod(0o600)
    disk_fd, code_fd = disk.fileno(), code.fileno()
    secure_boot_required = receipt.get("secure_boot_enforcement_required") is True
    machine = ["-machine", "q35"]
    if secure_boot_required:
        machine = ["-machine", "q35,smm=on", "-global", "driver=cfi.pflash01,property=secure,value=on"]
    command = virtual_machine() + [
        *machine, "-boot", "order=c,strict=on", "-serial", "stdio",
        "-add-fd", f"fd={code_fd},set=2", "-drive", "if=pflash,format=raw,unit=0,readonly=on,file=/dev/fdset/2",
        "-drive", "if=pflash,format=raw,unit=1,file=vars.fd",
        *readonly_disk(disk_fd, "base"),
        "-blockdev", "driver=file,node-name=changes,filename=changes.qcow2",
        "-blockdev", "driver=qcow2,node-name=trial,file=changes,backing=base",
        *virtio_disk("trial")]
    serial = run_guest(work, command, (disk_fd, code_fd), "native trial", file_limit=limit)
    verify_ready(serial, receipt["deployment_manifest_sha256"], secure_boot_required=secure_boot_required)


def verify_trial_filesystem(work, image, receipt, layout, configuration):
    create.validate_layout(work, image, layout)
    disk_uuid = subprocess.check_output(["sfdisk", "--disk-id", image], cwd=work, timeout=20).decode("ascii").strip()
    require(str(uuid.UUID(disk_uuid)) == receipt["trial_disk_uuid"], "trial GPT identity changed")
    start, size, _ = layout.partitions[1]
    state = work / "trial-state.ext4"
    with create.boot_media.regular_file(image) as source, state.open("xb") as out:
        state.chmod(0o600)
        source.seek(start * MIB)
        create.copy_mib(source, out, size, replace=False)
        out.truncate(size * MIB)
    require(create.filesystem_state(state, receipt["state_uuid"]) == (1, False),
            "trial state was not cleanly unmounted")
    create.boot_media.run(work, ["e2fsck", "-f", "-n", state])
    observed = create.read_root_file(work, state, "/etc/psios/core.json", 64 * 1024, "0(?:444|644)")
    require(observed == configuration, "trial changed its persisted Core configuration")
    require(create.read_partition_file(work, state, "/deployment-uuid", 64) == (receipt["state_uuid"] + "\n").encode(),
            "trial changed its deployment marker")
    state.unlink()


def verify_overlay(work, disk, prepared, layout, core_hash, node, target_format):
    # Bake a sparse inspection copy of the exact explicit backing graph. The
    # QCOW2 contains no backing filename and QEMU never discovers a host path.
    fd = disk.fileno()
    size = layout.image_mib * MIB
    image = work / "trial-verified.raw"
    options = ("driver=qcow2,file.driver=file,file.filename=changes.qcow2,"
               f"backing.driver=raw,backing.file.driver=file,backing.file.filename=/proc/self/fd/{fd},"
               "backing.file.locking=on")
    subprocess.run(["qemu-img", "convert", "-q", "-m", "1", "--image-opts", "-O", "raw", options, image.name],
                   cwd=work, pass_fds=(fd,), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                   check=True, timeout=120, preexec_fn=create.file_limit(size))
    image.chmod(0o600)
    require(image.stat().st_size == size, "incomplete read-only copy of trial overlay")
    verify_trial_filesystem(work, image, prepared.trial_receipt, layout, prepared.configuration)

    # Reuse the existing rescue kernel/init payload and pinned Core verifier.
    # No source repair or migration is allowed during this independent readback.
    rescue = work / "rescue"
    rescue.mkdir(mode=0o700)
    inspector_file = create.HERE.parent / "rescue" / "inspect.py"
    spec = importlib.util.spec_from_file_location("psios_rescue_inspector", inspector_file)
    inspector = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(inspector)
    inspector.prepare(rescue, SimpleNamespace(iso=prepared.kernel_iso, core=prepared.core, core_sha256=core_hash,
        state="/psios", witness=None, uid=1000, gid=None, partition=2, max_copy_mib=COPY_LIMIT_MIB,
        journal_overlay_mib=None, migration_config=None), False)
    with create.boot_media.regular_file(image) as raw:
        image_fd = raw.fileno()
        command = virtual_machine() + [
            "-machine", "pc", "-kernel", "kernel", "-initrd", "initrd", "-append", create.boot_media.CMDLINE,
            "-serial", "null", "-serial", "stdio", *readonly_disk(image_fd, "inspection"), *virtio_disk("inspection")]
        output = run_guest(rescue, command, (image_fd,), "trial authority inspection")
    marker = b"\n" + RESCUE_MARKER + b"\n"
    require(output.count(RESCUE_MARKER) == output.count(marker) == 1,
            "trial authority inspection did not emit one complete report")
    status, _, body = output.partition(marker)[2].partition(b"\n")
    if status != b"OK":
        failure = re.fullmatch(rb"ERROR ([a-zA-Z0-9-]{1,64})", status)
        raise ValueError("trial authority inspection failed" + (f" at {failure[1].decode()}" if failure else ""))
    report = json_report(body.strip())
    require(report.get("node_id") == node and type(report.get("database_format")) is int
            and report["database_format"] == target_format and report.get("migration_required") is False
            and report.get("sqlite_integrity") == report.get("foreign_keys") == "Verified"
            and report.get("witness") == "Confirmed" and report.get("owner_rotation") == "None"
            and type(report.get("copied_bytes")) is int and 0 < report["copied_bytes"] <= COPY_LIMIT_MIB * MIB,
            "post-trial authority does not match the expected migrated node and witness")
    return report


def prove_trial(work: Path, prepared, firmware_code: Path, firmware_vars: Path) -> dict:
    """Run a disposable UEFI trial and return proof bound to the staged update pair."""
    layout, config_hash, node, target_format = receipt_lineage(prepared)
    core_hash = create.sha256(prepared.core)
    with (create.boot_media.regular_file(prepared.candidate_disk) as candidate,
          create.boot_media.regular_file(prepared.trial_disk) as trial,
          create.boot_media.regular_file(firmware_code) as code,
          create.boot_media.regular_file(firmware_vars) as vars_file):
        verify_base_images(candidate, trial, prepared, layout)
        code_hash, vars_hash = digest(code), digest(vars_file)
        vars_file.seek(0)
        boot_trial(work, trial, code, vars_file, prepared.trial_receipt, layout.image_mib * MIB)
        authority = verify_overlay(work, trial, prepared, layout, core_hash, node, target_format)
        require(digest(trial) == prepared.trial_receipt["initial_image_sha256"]
                and digest(code) == code_hash and digest(vars_file) == vars_hash,
                "private trial backing or pinned firmware inputs changed")
    proof = {"status": "Verified", "boot_mode": create.TRIAL_BOOT_MODE,
             "configuration_sha256": config_hash, "trial_disk_uuid": prepared.trial_receipt["trial_disk_uuid"],
             "state_uuid": prepared.trial_receipt["state_uuid"], "home_uuid": prepared.trial_receipt["home_uuid"],
             "firmware_code_sha256": code_hash, "firmware_vars_sha256": vars_hash, "core_sha256": core_hash,
             "verified_authority": authority}
    if "secure_boot" in prepared.trial_receipt:
        proof["secure_boot"] = {"status": "Enforced", **prepared.trial_receipt["secure_boot"]}
    return proof
