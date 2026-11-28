"""Exercise authenticated native login on a real OVMF VGA virtual terminal."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

from native_dhcp import wait_for
from native_disk import REPO, boot, call, check_clean_shutdown, password, sha
from native_usb_inventory import monitor


KEYS = {
    " ": ("spc",), "\n": ("ret",), "/": ("slash",), "-": ("minus",),
    "_": ("shift", "minus"), ">": ("shift", "dot"),
}
# Keep the boot disk at the same PCI address when VGA is removed.
DISPLAY = ("-device", "VGA,addr=0x2", "-qmp", "unix:console-control.sock,server=on,wait=off")


def type_text(qmp, text):
    for character in text:
        keys = KEYS.get(character)
        if keys is None:
            assert character.isascii() and character.isalnum()
            keys = ("shift", character.lower()) if character.isupper() else (character,)
        qmp("send-key", keys=[{"type": "qcode", "data": key} for key in keys], **{"hold-time": 25})
        time.sleep(0.045)


def verify_console(work, console, operator):
    wait_for(console, "grep -Eq '^[0-9]+ simpledrmdrmfb$' /proc/fb && "
                      'test "$(cat /sys/class/tty/tty0/active)" = tty1 && '
                      "pidof getty >/dev/null")
    assert console.run("cat /etc/securetty") == "ttyS0"
    screen = work / "console.ppm"
    with monitor(work / "console-control.sock") as qmp:
        qmp("screendump", filename=str(screen))
        assert screen.read_bytes().startswith(b"P6\n")
        original = sha(screen)
        type_text(qmp, "psios\n")
        time.sleep(0.5)
        type_text(qmp, operator + "\n")
        time.sleep(0.5)
        type_text(qmp, "tty > /home/psios/vt-terminal\n")
        type_text(qmp, "psiosctl whoami > /home/psios/vt-identity\n")
        wait_for(console, "test -s /home/psios/vt-terminal && test -s /home/psios/vt-identity")
        qmp("screendump", filename=str(screen))
        assert sha(screen) != original, "the visible framebuffer did not change during local login"
    assert console.run("cat /home/psios/vt-terminal") == "/dev/tty1"
    local = json.loads(console.run("cat /home/psios/vt-identity"))
    serial = json.loads(console.run("psiosctl whoami"))
    assert local["authority_id"] == serial["authority_id"] and local["principal_id"] == "local-owner"
    console.run("rm /home/psios/vt-terminal /home/psios/vt-identity")
    return local["authority_id"]


def login_root(console, admin):
    console.send("exit")
    console.login("root", admin)


def stop(console):
    console.send("cd /; exec poweroff")
    assert console.vm.wait(35) == 0


def exercise(args, work):
    for name, source in (("code.fd", "OVMF_CODE_4M.fd"), ("vars.fd", "OVMF_VARS_4M.fd")):
        shutil.copyfile(args.ovmf_dir / source, work / name)
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "admin.hash")
    common = ["--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive)]
    image = work / "console.raw"
    previous = work / "console.json"
    previous.write_bytes(subprocess.check_output([sys.executable, args.installer, image, *common,
        "--console-hash", operator_hash, "--root-console-hash", admin_hash, "--local-vt"], timeout=180))
    receipt = json.loads(previous.read_bytes())
    assert (receipt["format"], receipt["local_vt"]) == (14, True), receipt
    with tempfile.TemporaryDirectory(dir=work) as validation:
        verify = ("from pathlib import Path; import sys; from install_source import stage_image\n"
                  "work, image, receipt = map(Path, sys.argv[1:4])\n"
                  "_, facts = stage_image(work, image, receipt, sys.argv[4])\n"
                  "assert (facts['receipt_format'], facts['local_vt']) == (14, True)\n")
        subprocess.run([sys.executable, "-B", "-c", verify, validation, image, previous, sha(previous)],
                       cwd=args.installer.resolve().parent, check=True, timeout=100)

    with boot(work, image, extra_qemu_args=DISPLAY, pace_commands=True) as console:
        console.login("psios", operator)
        identity = verify_console(work, console, operator)
        task = json.loads(console.run("printf '{}' | psiosctl submit system.status --key native-virtual-terminal"))["task_id"]
        result = call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})
        assert result["lifecycle"] == "Succeeded"
        login_root(console, admin)
        stop(console)
    check_clean_shutdown(image)
    print("PASS framebuffer displayed an authenticated local operator running Core CLI on tty1", flush=True)

    source_digest = sha(image)
    staged = work / "staged"
    code, variables = work / "code.fd", work / "vars.fd"
    original_firmware = sha(variables)
    def stage(output, source, receipt, firmware):
        return subprocess.run([sys.executable, args.installer.with_name("stage_update.py"), output, *common,
            "--upgrade-from", source, "--previous-receipt", receipt, "--previous-receipt-sha256", sha(receipt),
            "--ovmf-code", code, "--ovmf-code-sha256", sha(code), "--ovmf-vars", variables,
            "--ovmf-vars-sha256", firmware], capture_output=True, timeout=480)

    success = stage(staged, image, previous, original_firmware)
    assert success.returncode == 0, success.stderr[-4096:]
    staged_report = json.loads(success.stdout)
    assert staged_report["status"] == "Staged"
    assert (sha(image), sha(variables)) == (source_digest, original_firmware)
    updated = json.loads((staged / "candidate-build.json").read_bytes())
    assert (updated["format"], updated["local_vt"]) == (14, True), updated
    candidate = staged / "candidate.raw"
    with boot(work, candidate, extra_qemu_args=DISPLAY, pace_commands=True) as console:
        console.login("psios", operator)
        assert verify_console(work, console, operator) == identity
        assert call(console, "Task.Wait", {"id": task, "timeout_ms": 5000})["result"] == result["result"]
        login_root(console, admin)
        stop(console)
    check_clean_shutdown(candidate)
    with boot(work, candidate, pace_commands=True) as console:
        console.login("psios", operator)
        console.run('test -z "$(cat /proc/fb)"')
        assert json.loads(console.run("psiosctl whoami"))["authority_id"] == identity
        login_root(console, admin)
        console.run("printf '%s\\n' 'tty1 # local root' >> /etc/securetty")
        stop(console)
    check_clean_shutdown(candidate)
    print("PASS local text console and original Task survived staged upgrade, with headless serial fallback", flush=True)

    refusal_output = work / "refused"
    candidate_digest, firmware_digest = sha(candidate), sha(variables)
    refusal = stage(refusal_output, candidate, staged / "candidate-build.json", firmware_digest)
    assert refusal.returncode and b"invalid persisted local console configuration" in refusal.stderr, refusal.stderr[-4096:]
    assert not refusal_output.exists() and (sha(candidate), sha(variables)) == (candidate_digest, firmware_digest)
    print("PASS an upgrade refused a restored local root terminal without modifying its source or firmware")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-iso", type=Path, required=True)
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="psios-native-local-vt-") as directory:
        exercise(args, Path(directory))
