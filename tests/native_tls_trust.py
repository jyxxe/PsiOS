"""Exercise OpenSSL against the native TLS store and preserve a local override across an update."""
import argparse
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile

from native_disk import REPO, boot, check_clean_shutdown, check_state, extract_partition, password, replace_partition, sha
from native_upgrade import shutdown


STORE = "/etc/ssl/certs/ca-certificates.crt"
PUBLIC_PEM_SHA256 = "5961c0a3c65459931c023e0075c09733f09aea4ca439cb85bc4a08bb4f3f568b"


def generate_certificates(work):
    def private_run(*args):
        subprocess.run(args, cwd=work, check=True, capture_output=True, timeout=40)

    private_run("openssl", "req", "-new", "-x509", "-newkey", "rsa:2048", "-nodes", "-sha256", "-days", "2",
                "-subj", "/CN=Native Test CA", "-keyout", "ca.key", "-out", "local-ca.pem",
                "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    private_run("openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-sha256",
                "-subj", "/CN=node.test", "-keyout", "leaf.key", "-out", "leaf.csr")
    (work / "leaf.ext").write_text("basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n"
                                  "extendedKeyUsage=serverAuth\nsubjectAltName=DNS:node.test\n")
    private_run("openssl", "x509", "-req", "-in", "leaf.csr", "-CA", "local-ca.pem", "-CAkey", "ca.key",
                "-set_serial", "2", "-days", "2", "-sha256", "-extfile", "leaf.ext", "-out", "leaf.pem")


def install_probes(work, disk):
    generate_certificates(work)
    crypto_flags = shlex.split(subprocess.check_output(
        ["pkg-config", "--static", "--cflags", "--libs", "openssl"], text=True, timeout=15))
    subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-O2", "-static", "-s",
                    REPO / "tests/native_tls_probe.c", *crypto_flags, "-o", work / "tls-probe"],
                   cwd=work, check=True, timeout=90)
    home = work / "fixture-home.ext4"
    extract_partition(disk, home, 257, 64)
    edits = []
    for name, mode in (("tls-probe", "0100700"), ("local-ca.pem", "0100444"),
                       ("leaf.pem", "0100444"), ("leaf.key", "0100600")):
        target = f"/psios/{name}"
        edits.extend((f"write {name} {target}", f"set_inode_field {target} uid 1000",
                      f"set_inode_field {target} gid 1000", f"set_inode_field {target} mode {mode}"))
    subprocess.run(["debugfs", "-w", "-f", "-", home], cwd=work, input="\n".join(edits) + "\n",
                   text=True, capture_output=True, timeout=30, check=True)
    replace_partition(disk, home, 257)
    home.unlink()


def exercise(args, work):
    operator, operator_hash = password(work, "operator.hash")
    admin, admin_hash = password(work, "root.hash")
    source, prior = work / "source.raw", work / "source.json"
    code, variables = work / "code.fd", work / "vars.fd"
    shutil.copyfile(args.ovmf_dir / "OVMF_CODE_4M.fd", code)
    shutil.copyfile(args.ovmf_dir / "OVMF_VARS_4M.fd", variables)
    prior.write_bytes(subprocess.check_output([sys.executable, "-B", args.installer, source,
        "--kernel-iso", args.kernel_iso, "--core-archive", args.previous_archive,
        "--core-sha256", sha(args.previous_archive), "--console-hash", operator_hash,
        "--root-console-hash", admin_hash], timeout=180))
    prior.chmod(0o600)
    install_probes(work, source)
    local_digest = sha(work / "local-ca.pem")

    with boot(work, source, pace_commands=True) as console:
        console.login("psios", operator)
        manifest = json.loads(console.run("cat /psios-deployment.json"))
        assert manifest["tls_trust_bundle"]["pem_sha256"] == PUBLIC_PEM_SHA256
        assert console.run(f"sha256sum {STORE}").split()[0] == PUBLIC_PEM_SHA256
        assert console.run("sha256sum /etc/ssl/cert.pem").split()[0] == PUBLIC_PEM_SHA256
        assert console.run("test ! -w /etc/ssl/certs && test ! -w /psios-ca-certificates.crt") == ""
        assert console.run("./tls-probe") == "loaded 118 TLS roots"
        rejected = console.run("./tls-probe leaf.pem node.test; test $? -eq 2")
        assert "unable to get local issuer certificate" in rejected, rejected
        rejected = console.run("./tls-probe --handshake leaf.pem leaf.key node.test; test $? -eq 2")
        assert "unable to get local issuer certificate" in rejected, rejected
        baseline = check_state(console)
        console.send("exit")
        console.login("root", admin)
        console.run(f"cp /home/psios/local-ca.pem {STORE}.new && chmod 0644 {STORE}.new && "
                    f"mv -T {STORE}.new {STORE} && sync -f /etc")
        console.send("poweroff")
        assert console.vm.wait(35) == 0
    check_clean_shutdown(source)

    frozen = {path: sha(path) for path in (source, prior, code, variables)}
    output = work / "staged"
    report = json.loads(subprocess.check_output([sys.executable, "-B", args.controller, output,
        "--upgrade-from", source, "--previous-receipt", prior, "--previous-receipt-sha256", frozen[prior],
        "--kernel-iso", args.kernel_iso, "--core-archive", args.archive, "--core-sha256", sha(args.archive),
        "--ovmf-code", code, "--ovmf-code-sha256", frozen[code], "--ovmf-vars", variables,
        "--ovmf-vars-sha256", frozen[variables]], timeout=500))
    assert report["trial"]["status"] == "Verified"
    candidate = output / "candidate.raw"
    shutil.copyfile(variables, work / "candidate-vars.fd")
    with boot(work, candidate, vars_file="candidate-vars.fd", pace_commands=True) as console:
        console.login("psios", operator)
        assert console.run(f"sha256sum {STORE}").split()[0] == local_digest
        assert console.run("sha256sum /etc/ssl/cert.pem").split()[0] == local_digest
        assert console.run("sha256sum /psios-ca-certificates.crt").split()[0] == PUBLIC_PEM_SHA256
        assert console.run("./tls-probe leaf.pem node.test") == "verified HTTPS certificate"
        assert console.run("./tls-probe leaf.pem other.test; test $? -eq 2") == "hostname mismatch"
        assert console.run("./tls-probe --handshake leaf.pem leaf.key node.test") == "verified TLS 1.3 and HTTP response"
        assert console.run("./tls-probe --handshake leaf.pem leaf.key other.test; test $? -eq 2") == "hostname mismatch"
        node, task, result, boot_id = check_state(console)
        assert node == baseline[0] and task["task_id"] == baseline[1]["task_id"] and result == baseline[2]
        assert task["deduplicated"] and boot_id != baseline[3]
        shutdown(console, admin)
    check_clean_shutdown(candidate)
    assert {path: sha(path) for path in frozen} == frozen
    print("PASS native public trust, live TLS 1.3 and HTTP, hostname refusal and stopped-upgrade retention")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, default=REPO / "images/native/create.py")
    parser.add_argument("--controller", type=Path, default=REPO / "images/native/stage_update.py")
    parser.add_argument("--ovmf-dir", type=Path, default=Path("/usr/share/OVMF"))
    for flag in ("--kernel-iso", "--previous-archive", "--archive"):
        parser.add_argument(flag, type=Path, required=True)
    with tempfile.TemporaryDirectory(prefix="psios-native-tls-trust-") as directory:
        exercise(parser.parse_args(), Path(directory).resolve())
