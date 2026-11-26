"""Keep a real Backup-backed binding readable across the typed clone proof cutover."""

from contextlib import closing
import json
from pathlib import Path
import sqlite3
import subprocess

from oci_direct_clone_rebind import verify_direct_clone_rebind
from oci_volume_rebind import verify_rebind
from support import Node


def deployment_proofs(node):
    uri = (node.state / "state.db").as_uri() + "?mode=ro"
    with closing(sqlite3.connect(uri, uri=True)) as db:
        return dict(db.execute("""SELECT id,spec FROM versions
            WHERE kind='Deployment' AND rev=1 AND spec->>'restore_rebind' IS NOT NULL ORDER BY seq"""))


def verify_direct_clone_rebind_upgrade(previous_binaries, binaries, docker, spec, app_ids):
    node = Node(previous_binaries)
    try:
        verify_rebind(node, docker, spec, app_ids)
        node.stop()
        previous_daemon = node.daemon.copy()
        previous = json.loads(subprocess.check_output(previous_daemon + ["verify"], text=True, timeout=25))
        assert previous["database_format"] in (114, 115), previous
        legacy = deployment_proofs(node)
        assert len(legacy) == 1, legacy
        old_proof = json.loads(next(iter(legacy.values())))["restore_rebind"]
        assert old_proof["backup_ref"] and "clone_source" not in old_proof, old_proof

        node.daemon[0] = str(Path(binaries).absolute() / "psiosd")
        preview = json.loads(subprocess.check_output(node.daemon + ["verify", "--rehearse-migration"],
                                                     text=True, timeout=25))
        assert preview["database_format"] == previous["database_format"] and preview["migration_required"], preview
        assert preview["migration_rehearsal"]["target_format"] >= 116, preview
        node.start()
        verify_direct_clone_rebind(node, docker, spec, app_ids)
        node.stop()

        verified = json.loads(subprocess.check_output(node.daemon + ["verify"], text=True, timeout=25))
        assert verified["database_format"] >= 116 and not verified["migration_required"], verified
        settled = deployment_proofs(node)
        assert {key: settled[key] for key in legacy} == legacy, settled
        new_id, = settled.keys() - legacy.keys()
        current = json.loads(settled[new_id])["restore_rebind"]
        assert current["clone_source"]["publication_ref"] and "backup_ref" not in current, current

        # Exact clone history may not be interpreted as a legacy Backup binding.
        corrupted = json.loads(settled[new_id])
        corrupted["restore_rebind"].pop("clone_source")
        corrupted["restore_rebind"]["backup_ref"] = old_proof["backup_ref"]
        with closing(sqlite3.connect(node.state / "state.db")) as db, db:
            db.execute("UPDATE versions SET spec=? WHERE id=? AND rev=1", (json.dumps(corrupted), new_id))
        try:
            rejected = subprocess.run(node.daemon + ["verify"], capture_output=True, text=True, timeout=25)
            assert rejected.returncode != 0 and "RecoveryRequired" in rejected.stderr + rejected.stdout, rejected
        finally:
            with closing(sqlite3.connect(node.state / "state.db")) as db, db:
                db.execute("UPDATE versions SET spec=? WHERE id=? AND rev=1", (settled[new_id], new_id))
        assert deployment_proofs(node) == settled
        refused = subprocess.run(previous_daemon + ["serve"], capture_output=True, text=True, timeout=20)
        assert refused.returncode != 0 and "SchemaVersionUnsupported" in refused.stdout + refused.stderr, refused
        print("PASS exact historical Backup rebind proof survives migration and new direct-clone proof fences old Core")
    finally:
        node.close()
