"""Exercise recipient-bound backups with real independent authority stores."""
import base64
from contextlib import closing
import hashlib
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import time
import unittest
import uuid

from support import Failure, Node, target


class SignedBackupTransfer(unittest.TestCase):
    def setUp(self):
        self.binaries = os.getenv("PSIOS_TEST_BIN_DIR", "target/debug")
        self.source = self.node()
        self.receiver = self.node()
        self.source_id = self.authority(self.source)
        self.receiver_id = self.authority(self.receiver)
        self.assertNotEqual(self.source_id, self.receiver_id)
        self.private, self.public = self.key("source")
        self.other_private, self.other_public = self.key("alternate")
        volume = self.source.perform("volume.create", {"name": "signed-source"})
        self.source_volume = self.source.perform("volume.write", {
            **target(volume), "name": "config", "content": "signed backup payload",
        })["volume_ref"]
        self.backup = self.source.perform("backup.create", target(self.source_volume))
        self.manifest = self.source.get(self.backup["resource_id"])["value"]["manifest_digest"]
        self.archive = self.receiver.base / "export"
        self.published = self.receiver.state / "backups" / self.backup["resource_id"]
        self.attestation = self.receiver.base / "transfer.json"
        self.export()
        self.receiver.stop()

    def node(self):
        node = Node(self.binaries)
        self.addCleanup(node.close)
        return node

    def authority(self, node):
        return node.call("Node.Get", {})["reference"]["resource_id"]

    def key(self, name):
        private = self.source.base / f"{name}.der"
        public = self.receiver.base / f"{name}.pub"
        subprocess.run(
            ["openssl", "genpkey", "-algorithm", "ED25519", "-outform", "DER", "-out", private],
            check=True, timeout=10,
        )
        private.chmod(0o600)
        encoded = subprocess.check_output(
            ["openssl", "pkey", "-inform", "DER", "-in", private, "-pubout", "-outform", "DER"],
            timeout=10,
        )
        self.assertEqual(encoded[:12].hex(), "302a300506032b6570032100")
        self.assertEqual(len(encoded), 44)
        public.write_bytes(encoded[12:])
        public.chmod(0o644)
        return private, public

    def export(self):
        agent = self.source.call("Agent.Register", {"name": "signed-exporter"})
        self.source.call("Grant.Issue", {
            "principal_id": agent["principal_id"], "actions": ["backup.export"], "units": 2,
            "ttl_seconds": 90, "tool_targets": [self.backup["resource_id"]],
        })
        token = self.source.base / "export.token"
        token.touch(mode=0o600)
        token.write_text(agent["token"])
        exported = subprocess.check_output([
            self.source.binaries / "psiosctl", "--socket", self.source.socket, "--token-file", token,
            "backup-export", "--output", self.archive,
        ], input=json.dumps(self.backup).encode(), timeout=30)
        self.assertEqual(json.loads(exported)["manifest_digest"], self.manifest)

    def command(self, node, *args, payload=None, error=None):
        result = subprocess.run(
            node.daemon + list(args), input=payload, capture_output=True, timeout=30,
        )
        if error:
            self.assertNotEqual(result.returncode, 0, result.stdout)
            self.assertEqual(result.stdout, b"")
            self.assertIn(error.encode(), result.stderr)
            return None
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def sign(self, *, node=None, private=None, recipient=None, reference=None, error=None):
        return self.command(
            node or self.source, "attest-backup-transfer",
            "--recipient-authority", recipient or self.receiver_id,
            "--signing-key-file", private or self.private,
            payload=json.dumps(reference or self.backup).encode(), error=error,
        )

    def import_arguments(self, archive=None, public=None, source=None, digest=None, signed=True):
        args = ["import-backup", "--directory", archive or self.archive,
                "--manifest-digest", digest or self.manifest]
        if signed:
            args += ["--attestation-file", self.attestation, "--trusted-source-key",
                     public or self.public, "--expected-source-authority", source or self.source_id]
        return args

    def register(self, *, node=None, error=None, **kwargs):
        return self.command(node or self.receiver, *self.import_arguments(**kwargs), error=error)

    def inspect_intake(self, *options, error=None):
        return self.command(self.receiver, "inspect-backup-intake",
                            "--manifest-digest", self.manifest, *options, error=error)

    def reconcile_intake(self, approval, *options, error=None):
        return self.command(self.receiver, "reconcile-backup-intake", *options,
                            payload=json.dumps(approval).encode(), error=error)

    def write_attestation(self, envelope):
        self.attestation.write_text(json.dumps(envelope))

    def test_exact_receiver_and_verifier_before_publication_then_restore(self):
        self.sign(error="Conflict")
        self.source.stop()
        self.private.chmod(0o644)
        self.sign(error="PermissionDenied")
        self.private.chmod(0o600)
        linked = self.source.base / "linked.der"
        linked.symlink_to(self.private)
        self.sign(private=linked, error="PermissionDenied")
        self.sign(recipient=self.source_id, error="InvalidArgument")
        self.sign(reference=dict(self.backup, digest="sha256:" + "0" * 64), error="PreconditionFailed")

        state_files = [file for file in self.source.state.iterdir()
                       if file.name.startswith("state.db")]
        state_files += list(self.source.witness.iterdir())
        before = {path: path.read_bytes() for path in state_files}
        envelope = self.sign()
        self.assertEqual(self.sign(), envelope)
        self.assertEqual({path: path.read_bytes() for path in state_files}, before)
        self.assertEqual(set(self.source.witness.iterdir()),
                         {path for path in state_files if path.parent == self.source.witness})
        statement = envelope["attestation"]
        self.assertEqual(statement["format"], 1)
        self.assertEqual(statement["backup_ref"], self.backup)
        self.assertEqual(statement["source_volume_ref"], self.source_volume)
        self.assertEqual(statement["source_authority_id"], self.source_id)
        self.assertEqual(statement["recipient_authority_id"], self.receiver_id)
        self.assertEqual(statement["manifest_digest"], self.manifest)
        self.assertEqual(statement["signer_fingerprint"],
                         "sha256:" + hashlib.sha256(self.public.read_bytes()).hexdigest())
        self.assertEqual(len(base64.b64decode(envelope["signature_base64"], validate=True)), 64)
        self.write_attestation(envelope)

        unrelated = self.node()
        other_id = self.authority(unrelated)
        unrelated.stop()
        self.register(node=unrelated, error="PermissionDenied")
        self.assertEqual(list((unrelated.state / "backups").iterdir()), [])
        self.public.chmod(0o664)
        self.register(error="PermissionDenied")
        self.public.chmod(0o644)
        self.register(public=self.other_public, error="PermissionDenied")
        self.register(source=other_id, error="PermissionDenied")
        self.register(digest="sha256:" + "0" * 64, error="PreconditionFailed")
        forged = dict(envelope, attestation=dict(statement, data_incarnation=f"data-op-{uuid.uuid4()}"))
        self.write_attestation(forged)
        self.register(error="PermissionDenied")
        self.write_attestation(envelope)
        damaged = self.receiver.base / "damaged"
        shutil.copytree(self.archive, damaged)
        manifest = damaged / "manifest.json"
        original = manifest.read_bytes()
        manifest.write_bytes(original + b"\n")
        self.register(archive=damaged, error="PreconditionFailed")
        manifest.write_bytes(original)
        blob, = damaged.glob("blob-*")
        blob.write_bytes(b"x" * blob.stat().st_size)
        self.register(archive=damaged, error="PreconditionFailed")
        self.assertEqual(list(self.published.parent.iterdir()), [])
        with closing(sqlite3.connect(self.receiver.state / "state.db")) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM current WHERE kind='BackupIntake'").fetchone(), (0,))

        imported = self.register()
        self.assertFalse(imported["reused"])
        self.assertEqual(self.register(), dict(imported, reused=True))
        self.register(signed=False, error="Conflict")
        self.write_attestation(self.sign(private=self.other_private))
        self.register(public=self.other_public, error="Conflict")
        self.write_attestation(envelope)
        self.assertEqual(self.command(self.receiver, "verify")["witness"], "Confirmed")
        self.sign(node=self.receiver, recipient=self.source_id, reference=imported["reference"],
                  error="PreconditionFailed")
        self.receiver.start()
        self.assertEqual(self.receiver.get(imported["reference"]["resource_id"])["reference"],
                         imported["reference"])
        restored = self.receiver.perform("backup.restore", target(imported["reference"]))
        local = self.receiver.get(restored["resource_id"])
        self.assertNotEqual(local["reference"]["resource_id"], self.source_volume["resource_id"])
        self.assertNotEqual(local["value"]["data_incarnation"], statement["data_incarnation"])
        self.assertEqual(local["resource"]["spec"]["storage_restore_phase"], "AwaitingActivation")
        activation = self.receiver.submit("volume.activate", target(restored))
        self.assertEqual(self.receiver.wait(activation)["lifecycle"], "WaitingApproval")
        self.receiver.approve(activation)
        self.assertEqual(self.receiver.wait(activation)["lifecycle"], "Succeeded")
        current = self.receiver.get(restored["resource_id"])["reference"]
        read = self.receiver.perform("volume.read", {**target(current), "name": "config"})
        self.assertEqual(read["content"], "signed backup payload")

    def test_settlement_recovers_exact_signed_custody_and_rejects_missing_verifier(self):
        self.source.stop(signal.SIGKILL)
        # Restart resolves any genuinely pending source witness; the signer itself is read-only.
        self.source.start()
        self.source.stop()
        envelope = self.sign()
        self.write_attestation(envelope)
        with closing(sqlite3.connect(self.receiver.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER fail_signed_catalog BEFORE INSERT ON versions
                WHEN NEW.kind='Backup' BEGIN
                    SELECT RAISE(ABORT, 'test signed intake settlement failure');
                END
            """)
        self.register(error="StorageUnavailable")
        inode = self.published.stat().st_ino
        self.assertEqual(self.command(self.receiver, "verify")["witness"], "Confirmed")
        self.receiver.start()
        with self.assertRaises(Failure) as missing:
            self.receiver.get(self.backup["resource_id"])
        self.assertEqual(missing.exception.code, "NotFound")
        self.receiver.stop(signal.SIGKILL)
        with closing(sqlite3.connect(self.receiver.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER fail_signed_catalog")
        self.archive.rename(self.receiver.base / "unplugged")
        self.register(signed=False, error="Conflict")
        self.write_attestation(self.sign(private=self.other_private))
        self.register(public=self.other_public, error="Conflict")
        self.write_attestation(envelope)
        receipt = self.register()
        self.assertTrue(receipt["reused"])
        self.assertEqual(self.register(), receipt)
        self.assertEqual(self.published.stat().st_ino, inode)
        self.assertEqual(self.command(self.receiver, "verify")["witness"], "Confirmed")
        self.receiver.start()
        audit = [event["decision"] for event in self.receiver.call("Audit.List", {"limit": 128})
                 if event["method"] == "Backup.ImportDetached"]
        self.assertEqual(audit, ["Prepared", "Imported"])
        self.receiver.stop()
        with closing(sqlite3.connect(self.receiver.state / "state.db")) as db, db:
            db.execute("DELETE FROM signed_backup_transfers")
        self.command(self.receiver, "verify", error="RecoveryRequired")
        self.inspect_intake(error="RecoveryRequired")
        self.register(error="RecoveryRequired")

    def test_existing_digest_only_custody_cannot_gain_signed_provenance(self):
        self.source.stop()
        self.write_attestation(self.sign())
        original = self.register(signed=False)
        self.assertFalse(original["reused"])
        self.register(error="Conflict")
        self.assertEqual(self.register(signed=False), dict(original, reused=True))
        self.assertEqual(self.command(self.receiver, "verify")["witness"], "Confirmed")

    def test_reconcile_exact_signed_publication_without_transport_or_source_keys(self):
        self.inspect_intake(error="NotFound")
        self.source.stop()
        self.write_attestation(self.sign())
        n = self.receiver
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER withhold_signed_catalog BEFORE INSERT ON versions
                WHEN NEW.kind='Backup' BEGIN
                    SELECT RAISE(ABORT, 'test missing intake catalog receipt');
                END
            """)
        self.register(error="StorageUnavailable")
        with closing(sqlite3.connect(n.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER withhold_signed_catalog")
        inode = self.published.stat().st_ino
        def authority_bytes():
            return {
                path: path.read_bytes()
                for path in list(n.witness.iterdir()) + list(n.state.glob("state.db*"))
            }

        before = authority_bytes()
        inspected = self.inspect_intake()
        self.assertEqual(authority_bytes(), before)
        self.assertEqual(inspected["authority_id"], self.receiver_id)
        self.assertEqual(inspected["mode"], "Signed")
        self.assertEqual(inspected["lifecycle"], "Prepared")
        self.assertEqual(inspected["publication"], "VerifiedPublished")
        self.assertEqual(inspected["signer_fingerprint"],
                         "sha256:" + hashlib.sha256(self.public.read_bytes()).hexdigest())
        self.assertNotIn("backup_ref", inspected)
        for private in ("signature_base64", "public_key", "directory"):
            self.assertNotIn(private, json.dumps(inspected))
        self.inspect_intake("--max-bytes", "1", error="QuotaExceeded")
        approval = inspected["recovery"]
        self.assertEqual(approval["intake_ref"], inspected["intake_ref"])
        self.reconcile_intake(dict(approval, authority_id=self.source_id), error="Conflict")
        self.reconcile_intake(dict(approval, recovery_incarnation="different"), error="Conflict")
        self.reconcile_intake(dict(approval, intake_ref=dict(approval["intake_ref"], version="r2")),
                              error="Conflict")

        twin = n.base / "witnessed-publication"
        self.published.rename(twin)
        shutil.copytree(twin, self.published)
        observed = self.inspect_intake()
        self.assertEqual(observed["publication"], "Conflict")
        self.assertNotIn("recovery", observed)
        self.reconcile_intake(approval, error="RecoveryRequired")
        shutil.rmtree(self.published)
        twin.rename(self.published)

        blob, = self.published.glob("blob-*")
        raw = blob.read_bytes()
        blob.write_bytes(b"\x00" * len(raw))
        observed = self.inspect_intake()
        self.assertEqual(observed["publication"], "Conflict")
        self.assertNotIn("recovery", observed)
        self.reconcile_intake(approval, error="PreconditionFailed")
        blob.write_bytes(raw)
        with blob.open("rb") as file:
            os.fsync(file.fileno())

        n.start()
        self.inspect_intake(error="Conflict")
        self.reconcile_intake(approval, error="Conflict")
        with self.assertRaises(Failure) as missing:
            n.get(self.backup["resource_id"])
        self.assertEqual(missing.exception.code, "NotFound")
        n.stop()
        self.archive.rename(n.base / "disconnected-transfer")
        self.attestation.unlink()
        self.public.unlink()
        self.private.unlink()
        receipt = self.reconcile_intake(approval)
        self.assertTrue(receipt["reused"])
        self.assertEqual(self.reconcile_intake(approval), receipt)
        self.assertEqual(self.published.stat().st_ino, inode)
        observed = self.inspect_intake()
        self.assertEqual((observed["lifecycle"], observed["publication"]), ("Settled", "VerifiedPublished"))
        self.assertEqual(observed["backup_ref"], receipt["reference"])
        self.assertNotIn("recovery", observed)
        n.start()
        self.assertEqual(n.get(self.backup["resource_id"])["reference"], receipt["reference"])
        audit = [event["decision"] for event in n.call("Audit.List", {"limit": 128})
                 if event["method"] == "Backup.ImportDetached"]
        self.assertEqual(audit, ["Prepared", "Imported"])
        n.perform("backup.delete", target(receipt["reference"]))
        n.stop()
        retired = self.inspect_intake()
        self.assertEqual((retired["lifecycle"], retired["publication"]), ("Settled", "Retired"))
        self.assertEqual(retired["backup_ref"], receipt["reference"])
        self.assertNotIn("recovery", retired)
        self.reconcile_intake(approval, error="PreconditionFailed")

    @unittest.skipUnless(os.getenv("PSIOS_TEST_BACKUP_INTAKE_CUT") == "1",
                         "requires syscall tracing and injection")
    def test_legacy_retry_syncs_publication_before_reconciling_real_rename_cut(self):
        n = self.receiver
        tracer = ["strace", "-D", "-f", "-yy", "-e", "trace=renameat2,fsync"]
        importer = [*n.daemon, *self.import_arguments(signed=False)]
        before_trace = n.base / "before-rename.strace"
        killed = subprocess.run([
            *tracer, "-o", before_trace,
            "-e", "inject=renameat2:error=EINTR:signal=SIGKILL:when=1", "--", *importer,
        ], capture_output=True, timeout=25)
        self.assertEqual(killed.returncode, -signal.SIGKILL, killed.stderr)
        self.assertIn(f'"{self.published.name}", RENAME_NOREPLACE', before_trace.read_text())
        self.assertFalse(self.published.exists())
        stages = list(self.published.parent.glob(".stage-*"))
        self.assertEqual(len(stages), 1)
        interrupted = self.inspect_intake()
        self.assertEqual((interrupted["lifecycle"], interrupted["publication"]),
                         ("Prepared", "NoPublishedDirectory"))
        self.assertEqual(interrupted["mode"], "DigestOnly")
        self.assertNotIn("recovery", interrupted)
        old_approval = {key: interrupted[key] for key in (
            "authority_id", "recovery_incarnation", "intake_ref",
        )}
        self.reconcile_intake(old_approval, error="PreconditionFailed")
        self.assertEqual(list(self.published.parent.glob(".stage-*")), stages)
        self.assertFalse(self.published.exists())
        n.start()
        n.stop()
        self.assertEqual(list(self.published.parent.glob(".stage-*")), [])

        after_trace = n.base / "after-rename.strace"
        with subprocess.Popen([
            *tracer, "-o", after_trace,
            "-e", "inject=renameat2:delay_exit=6000ms:when=1", "--", *importer,
        ], stdout=subprocess.PIPE, stderr=subprocess.PIPE) as process:
            try:
                deadline = time.monotonic() + 20
                while not self.published.is_dir() and process.poll() is None:
                    self.assertLess(time.monotonic(), deadline, "rename did not publish")
                    time.sleep(0.01)
                self.assertIsNone(process.poll())
                self.assertTrue(os.path.samefile(f"/proc/{process.pid}/exe", n.daemon[0]))
                process.kill()
                _, stderr = process.communicate(timeout=15)
                self.assertEqual(process.returncode, -signal.SIGKILL, stderr)
            finally:
                if process.poll() is None:
                    process.kill()
        output = after_trace.read_text()
        name = f'"{self.published.name}", RENAME_NOREPLACE'
        self.assertIn(name, output)
        self.assertFalse(any("fsync(" in line and f"<{self.published.parent}>" in line
                             for line in output.split(name, 1)[1].splitlines()), output)
        inode = self.published.stat().st_ino
        ready = self.inspect_intake()
        self.assertEqual((ready["lifecycle"], ready["publication"]), ("Prepared", "VerifiedPublished"))
        self.assertNotEqual(ready["intake_ref"], interrupted["intake_ref"])
        self.reconcile_intake(old_approval, error="Conflict")

        sync_trace = n.base / "retry-sync.strace"
        rejected = subprocess.run([
            "strace", "-yy", "-o", sync_trace, "-e", "trace=fsync", "-P", self.published.parent,
            "-e", "inject=fsync:error=EIO:when=1", "--", *importer,
        ], capture_output=True, timeout=25)
        self.assertNotEqual(rejected.returncode, 0, rejected.stdout)
        self.assertIn(b"StorageUnavailable", rejected.stderr)
        self.assertIn(f"<{self.published.parent}>) = -1 EIO", sync_trace.read_text())
        self.assertEqual(self.inspect_intake(), ready)
        n.start()
        with self.assertRaises(Failure) as missing:
            n.get(self.backup["resource_id"])
        self.assertEqual(missing.exception.code, "NotFound")
        n.stop()
        self.archive.rename(n.base / "transfer-no-longer-mounted")
        receipt = self.reconcile_intake(ready["recovery"])
        self.assertEqual(receipt, self.reconcile_intake(ready["recovery"]))
        self.assertEqual(self.published.stat().st_ino, inode)
        self.assertEqual(self.inspect_intake()["backup_ref"], receipt["reference"])

    @unittest.skipUnless(os.getenv("PSIOS_BACKUP_TRANSFER_PRIOR_BIN_DIR"),
                         "requires exact format 104 binaries")
    def test_old_writer_pending_digest_intake_migrates_without_rebinding(self):
        legacy = Node(os.environ["PSIOS_BACKUP_TRANSFER_PRIOR_BIN_DIR"])
        self.addCleanup(legacy.close)
        legacy_id = self.authority(legacy)
        legacy.stop()
        prior_command = legacy.daemon.copy()
        self.assertEqual(self.command(legacy, "verify")["database_format"], 104)
        with closing(sqlite3.connect(legacy.state / "state.db")) as db, db:
            db.execute("""
                CREATE TRIGGER legacy_fail_catalog BEFORE INSERT ON versions
                WHEN NEW.kind='Backup' BEGIN
                    SELECT RAISE(ABORT, 'test prior published intake settlement failure');
                END
            """)
        self.register(node=legacy, signed=False, error="StorageUnavailable")
        published = legacy.state / "backups" / self.backup["resource_id"]
        inode = published.stat().st_ino
        with closing(sqlite3.connect(legacy.state / "state.db")) as db, db:
            db.execute("DROP TRIGGER legacy_fail_catalog")
        self.source.stop()
        self.write_attestation(self.sign(recipient=legacy_id))

        legacy.daemon[0] = self.receiver.daemon[0]
        preview = self.command(legacy, "verify", "--rehearse-migration")
        self.assertEqual(preview["database_format"], 104)
        self.assertTrue(preview["migration_required"])
        target_format = preview["migration_rehearsal"]["target_format"]
        self.assertGreaterEqual(target_format, 105)
        self.register(node=legacy, error="Conflict")
        self.archive.rename(self.receiver.base / "unplugged-prior")
        receipt = self.register(node=legacy, signed=False)
        self.assertTrue(receipt["reused"])
        self.assertEqual(self.register(node=legacy, signed=False), receipt)
        self.assertEqual(published.stat().st_ino, inode)
        self.assertEqual(self.command(legacy, "verify")["database_format"], target_format)

        new_command = legacy.daemon
        legacy.daemon = prior_command
        try:
            self.command(legacy, "verify", error="SchemaVersionUnsupported")
        finally:
            legacy.daemon = new_command
        legacy.start()
        self.assertEqual(legacy.get(self.backup["resource_id"])["reference"], receipt["reference"])
