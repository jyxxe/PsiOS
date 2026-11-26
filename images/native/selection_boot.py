"""Boot the exact selected native deployment and return a new verified generation state."""

from activate_update import CODE, DISK, VARS, secret_at
from activation_guest import boot_candidate
from activation_storage import BUILD_RECEIPT, BootStorage
from install import checked_digest, require


def boot(work, entry, args, issue):
    """Run under the selector lock. Issue the durable boot record before starting QEMU."""
    selection = entry["deployment"]
    current = entry["current"]
    signing = selection["secure_boot"]
    digest_fields = ("image_sha256", "efi_code_sha256", "efi_vars_sha256")
    expected_artifacts = {field: checked_digest(current[field]) for field in digest_fields}
    storage = BootStorage.from_receipt(work / BUILD_RECEIPT, selection["candidate_receipt_sha256"],
                                        selection["size_bytes"])
    with boot_candidate(
        work,
        disk=work / DISK,
        firmware_code=work / CODE,
        firmware_vars=work / VARS,
        credential=secret_at(args.root_console_password_file),
        expected_node=selection["node_id"],
        manifest_sha256=selection["deployment_manifest_sha256"],
        expected_artifacts=expected_artifacts,
        before_launch=issue,
        storage=storage,
        secure_boot_required=signing is not None,
    ) as result:
        proof = result["proof"]
        require(proof["boot_id"] != current["boot_id"], "selected boot did not produce a fresh Linux boot ID")
        require(proof.get("secure_boot") == ("Enforced" if signing else None),
                "selected boot did not prove the approved Secure Boot policy")
        if signing:
            proof["secure_boot"] = {"status": "Enforced", **signing}
        return {
            "current": {**result["artifacts"], "boot_id": proof["boot_id"]},
            "proof": proof,
        }
