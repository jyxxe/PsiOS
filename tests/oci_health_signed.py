"""Verify signed OCI health opt-in and omission with an existing real fixture image."""
import json
import subprocess
import time

from oci_capture import denied
from support import key, target
from test_artifact_install import TOOL as INSTALL, grant, publish_install
from test_oci_publisher import local_signature


STAGE = "application.stage_artifact_release"


def resolve(node, reference):
    return node.call("Resource.Resolve", {"reference": reference})["value"]


def approved_release(node, preview, source, signature, publisher):
    assert preview["publisher"]["source_trust"] == "PublisherVerified", preview
    assert preview["publisher"]["key_ref"] == signature["key_ref"], preview
    task_id = node.call("Task.Submit", preview["request"])["task_id"]
    waiting = node.wait(task_id)
    assert waiting["lifecycle"] == "WaitingApproval", waiting
    approval = node.get(waiting["conditions"][0]["message"])["value"]
    plan = resolve(node, approval["plan_ref"])
    purpose = resolve(node, plan["purpose_ref"])
    assert (purpose["reference"], purpose["signature"]) == (source, signature), purpose
    assert [condition["resource_ref"] for condition in plan["preconditions"]] == preview["resource_refs"], plan

    completed = node.wait(task_id, autoapprove=True)
    assert completed["lifecycle"] == "Succeeded", completed
    artifact, = completed["result"]["artifact_refs"]
    receipt = node.call("Artifact.Read", {"reference": artifact})
    assert receipt["source_artifact_ref"] == source, receipt
    manifest = resolve(node, receipt["release_ref"])
    assert manifest["publisher_identity_ref"]["resource_id"] == publisher["principal_id"], manifest
    release = resolve(node, manifest["release_spec_ref"])
    provenance = release["provenance"]
    assert provenance["source_trust"] == "PublisherVerified" and source in provenance["source_refs"], provenance
    evidence = resolve(node, provenance["trust_evidence_or_admission_policy_ref"])
    assert (evidence["artifact_ref"], evidence["signature"]) == (source, signature), evidence
    return receipt, manifest


def verify_signed_health(node, docker, specification, probe):
    owner = "local-owner"
    node_id = node.call("Node.Get", {})["reference"]["resource_id"]
    grant(node, owner, ["artifact.upload", "artifact.read"], [node_id])
    volume_id = specification["volume"]["id"]
    volume = node.get(volume_id)["reference"]
    spec = {**specification, "name": "signed-image-health", "volume": target(volume), "health_check": probe}
    source = publish_install(node, spec)
    changed = publish_install(node, {**spec, "health_check": {**probe, "timeout_ms": probe["timeout_ms"] - 1}})
    signature, _, publisher = local_signature(node, source)
    permit = grant(node, owner, ["task.submit", "task.read", INSTALL],
                   [volume_id, source["resource_id"], changed["resource_id"], signature["key_ref"]["resource_id"]])
    install = {"reference": source, "signature": signature, "key": key(),
               "delegation_id": permit["delegation_ref"]["resource_id"]}
    denied(node, "PreconditionFailed", "Application.PrepareArtifactInstall", {**install, "reference": changed})
    preview = node.call("Application.PrepareArtifactInstall", install)
    assert preview["deployment"]["health_check"] == probe, preview
    assert preview["resource_refs"] == [volume, source, signature["key_ref"]], preview
    receipt, manifest = approved_release(node, preview, source, signature, publisher)
    app = receipt["application_ref"]
    app_id = app["resource_id"]
    ready = node.state / "volumes" / volume_id / "data" / "ready"

    try:
        profile = resolve(node, manifest["health_check_ref"])
        assert profile["probe"] == probe and profile["config_digest"] == spec["image_id"], profile
        ready.write_text("signed probe ready\n")
        app = node.perform("application.start", target(app))
        deadline = time.monotonic() + 12
        while True:
            health = node.perform("application.health", target(app))
            if health["health"] == "Healthy":
                assert health["readiness"] == "Ready" and health["process_running"], health
                break
            assert time.monotonic() < deadline, health
            time.sleep(0.2)
        app = node.perform("application.stop", target(app))

        stage = {field: value for field, value in spec.items() if field not in ("health_check", "volume")}
        stage["release_version"] = "2.0.0"
        omitted = publish_install(node, stage)
        signature, _, publisher = local_signature(node, omitted)
        permit = grant(node, owner, ["task.submit", "task.read", STAGE],
                       [app_id, omitted["resource_id"], signature["key_ref"]["resource_id"]])
        preview = node.call("Application.PrepareArtifactRelease", {
            "target": target(app), "reference": omitted, "signature": signature, "key": key(),
            "delegation_id": permit["delegation_ref"]["resource_id"],
        })
        assert "health_check" not in preview["release"], preview
        assert preview["resource_refs"] == [app, omitted, signature["key_ref"]], preview
        staged, manifest = approved_release(node, preview, omitted, signature, publisher)
        assert manifest["health_check_ref"] == node.get("oci-health")["reference"], manifest
        app = node.perform("application.select_release", {"target": target(app), "release_ref": staged["release_ref"]})
        app = node.perform("application.start", target(app))
        health = node.perform("application.health", target(app))
        assert (health["health"], health["readiness"]) == ("Unknown", "Unknown"), health
        runtime, = json.loads(subprocess.check_output(docker + ["inspect", health["container_id"]], timeout=15))
        assert runtime["Image"] == spec["image_id"], runtime["Image"]
        assert runtime["Config"]["Healthcheck"]["Test"] == ["NONE"], runtime["Config"]["Healthcheck"]
        assert not runtime["State"].get("Health"), "disabled health check has engine state"
        app = node.perform("application.stop", target(app))
        node.perform("application.uninstall", target(app))
        print("PASS signed OCI probe opt-in, altered-declaration refusal and signed disabled release", flush=True)
    finally:
        ready.unlink(missing_ok=True)
        containers = subprocess.check_output(docker + ["ps", "-aq", "--filter", f"label=io.psios.application={app_id}"],
                                             text=True, timeout=15).split()
        if containers:
            subprocess.run(docker + ["rm", "-f", *containers], capture_output=True, timeout=20)
