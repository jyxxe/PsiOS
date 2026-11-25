//! Local trust for detached signatures on OCI installation declarations.
use crate::{
    Result,
    db::{Db, Doc, find, resolve, row},
    decode, err, now, policy, require,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::{OciPublisherSignature, canonical, sha256};
use ring::signature::{ED25519, UnparsedPublicKey};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const FORMAT: u32 = crate::artifact::session_output::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("publisher.sql");
const DOMAIN: &str = "psios/local-oci-declaration-signature/1";
const KEY: &str = "ArtifactPublisherKey";
const EVIDENCE: &str = "OciPublisherVerification";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    principal_id: String,
    public_key_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoke {
    id: String,
    expected_resource_version: String,
    reason: String,
}

fn base64<const N: usize>(value: &str) -> Result<[u8; N]> {
    let invalid = || {
        err(
            "InvalidArgument",
            "expected fixed-length canonical Ed25519 Base64",
        )
    };
    require(
        value.len() == 4 * N.div_ceil(3),
        "InvalidArgument",
        "incorrect Ed25519 Base64 length",
    )?;
    let bytes = STANDARD.decode(value).map_err(|_| invalid())?;
    require(
        STANDARD.encode(&bytes) == value,
        "InvalidArgument",
        "invalid Ed25519 Base64 encoding",
    )?;
    bytes.try_into().map_err(|_| invalid())
}

pub(crate) fn register(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let input: Register = decode(input)?;
    let public_key = base64::<32>(&input.public_key_base64)?;
    policy::active(db, &input.principal_id)?;
    let publisher = db.of(&input.principal_id, "Principal")?;
    let fingerprint = sha256(&public_key);
    let id = format!("oci-key-{}", &fingerprint[7..]);
    let spec = json!({"principal_id":publisher.id,"algorithm":"Ed25519",
        "public_key_base64":input.public_key_base64,"fingerprint":fingerprint,"signature_domain":DOMAIN});
    let key = if let Some(prior) = db.find(&id)? {
        require(
            prior.kind == KEY && prior.spec == spec,
            "IdempotencyConflict",
            "publisher key is already bound",
        )?;
        prior
    } else {
        let key = db.create(
            KEY,
            &id,
            &publisher.id,
            spec,
            json!({"lifecycle":"Active","publisher_ref":publisher.reference(),
                  "fingerprint":fingerprint}),
        )?;
        db.audit(
            actor,
            "Artifact.PublisherKey.Register",
            Some(&key.id),
            "Allowed",
        )?;
        key
    };
    Ok(
        json!({"reference":key.reference(),"lifecycle":key.body["lifecycle"],"fingerprint":fingerprint}),
    )
}

pub(crate) fn revoke(db: &Db<'_>, actor: &str, input: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let input: Revoke = decode(input)?;
    require(
        !input.reason.trim().is_empty() && input.reason.len() <= 256,
        "InvalidArgument",
        "bounded revocation reason required",
    )?;
    let key = db.of(&input.id, KEY)?;
    key.cas(&input.expected_resource_version)?;
    require(
        key.body["lifecycle"] == "Active",
        "PreconditionFailed",
        "publisher key already revoked",
    )?;
    let key = db.status(&key,json!({"lifecycle":"Revoked","publisher_ref":key.body["publisher_ref"],
        "fingerprint":key.body["fingerprint"],"reason":input.reason,"revoked_at":now(),"revoked_by":actor}))?;
    db.audit(
        actor,
        "Artifact.PublisherKey.Revoke",
        Some(&key.id),
        "Allowed",
    )?;
    Ok(key.reference())
}

pub(super) fn message(artifact: &Doc) -> Vec<u8> {
    canonical(
        &json!({"domain":DOMAIN,"artifact_digest":artifact.spec["digest"],
        "declared_media_type":artifact.spec["declared_media_type"],"size_bytes":artifact.spec["size_bytes"]}),
    )
}

fn signature(
    conn: &Connection,
    artifact: &Doc,
    proof: &OciPublisherSignature,
) -> Result<(Doc, Doc)> {
    let key = resolve(conn, &json!(proof.key_ref))?;
    require(
        key.kind == KEY && key.rev == 1,
        "PreconditionFailed",
        "expected original registered publisher key",
    )?;
    let publisher = resolve(conn, &key.body["publisher_ref"])?;
    require(
        crate::artifact::is_raw(artifact)
            && artifact.spec["declared_media_type"] == super::artifact_install::MEDIA
            && key.body["lifecycle"] == "Active"
            && key.spec["signature_domain"] == DOMAIN
            && key.spec["algorithm"] == "Ed25519"
            && publisher.kind == "Principal"
            && key.owner == publisher.id
            && key.spec["principal_id"] == publisher.id,
        "RecoveryRequired",
        "publisher key disagrees with its registered identity",
    )?;
    let public_key = base64::<32>(key.spec["public_key_base64"].as_str().unwrap_or_default())?;
    let fingerprint = sha256(&public_key);
    require(
        key.spec["fingerprint"] == fingerprint
            && key.body["fingerprint"] == fingerprint
            && key.id == format!("oci-key-{}", &fingerprint[7..]),
        "RecoveryRequired",
        "publisher key fingerprint mismatch",
    )?;
    let bytes = base64::<64>(&proof.signature_base64)?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(&message(artifact), &bytes)
        .map_err(|_| {
            err(
                "PreconditionFailed",
                "publisher signature does not match the OCI declaration",
            )
        })?;
    Ok((key, publisher))
}

fn require_active(db: &Db<'_>, key: &Doc) -> Result<()> {
    let active = db.of(&key.id, KEY)?;
    require(
        active.body["lifecycle"] == "Active" && active.reference() == key.reference(),
        "PermissionDenied",
        "publisher signing key is revoked or superseded",
    )?;
    policy::active(db, &key.owner)
}

pub(super) fn check(
    db: &Db<'_>,
    artifact: &Doc,
    proof: &OciPublisherSignature,
) -> Result<(Doc, Doc)> {
    let (key, publisher) = signature(db.c, artifact, proof)?;
    require_active(db, &key)?;
    Ok((key, publisher))
}

pub(super) struct Verification {
    pub evidence_ref: Value,
    pub publisher_ref: Value,
    pub artifact_ref: Value,
}

pub(super) fn record(
    db: &Db<'_>,
    actor: &str,
    operation: &str,
    artifact: &Doc,
    proof: &OciPublisherSignature,
) -> Result<Verification> {
    let (key, publisher) = check(db, artifact, proof)?;
    let evidence = db.create(EVIDENCE,&format!("oci-publisher-{operation}"),actor,json!({"operation_id":operation}),
        json!({"artifact_ref":artifact.reference(),"publisher_ref":publisher.reference(),"signature":proof,
               "key_fingerprint":key.spec["fingerprint"],"verified_at":now()}))?;
    Ok(Verification {
        evidence_ref: evidence.reference(),
        publisher_ref: publisher.reference(),
        artifact_ref: artifact.reference(),
    })
}

pub(super) fn check_launch(db: &Db<'_>, release: &Doc, spec: &Doc) -> Result<()> {
    let provenance = &spec.body["provenance"];
    if provenance["source_trust"] == "PublisherVerified" {
        let evidence = db.resolve(&provenance["trust_evidence_or_admission_policy_ref"])?;
        require(
            evidence.kind == EVIDENCE,
            "RecoveryRequired",
            "signed release is missing publisher evidence",
        )?;
        let artifact = db.resolve(&evidence.body["artifact_ref"])?;
        let proof: OciPublisherSignature = decode(&evidence.body["signature"])?;
        let (key, publisher) = check(db, &artifact, &proof)?;
        check_attribution(release, spec, &evidence, &artifact, &key, &publisher)?;
    }
    Ok(())
}

fn check_attribution(
    release: &Doc,
    spec: &Doc,
    evidence: &Doc,
    artifact: &Doc,
    key: &Doc,
    publisher: &Doc,
) -> Result<()> {
    let provenance = &spec.body["provenance"];
    require(
        release.body["release_spec_ref"] == spec.reference()
            && release.body["publisher_identity_ref"] == publisher.reference()
            && spec.kind == "ApplicationReleaseSpec"
            && spec.owner == release.owner
            && provenance["source_trust"] == "PublisherVerified"
            && provenance["trust_evidence_or_admission_policy_ref"] == evidence.reference()
            && provenance["source_refs"]
                .as_array()
                .is_some_and(|sources| sources.contains(&artifact.reference()))
            && evidence.body["publisher_ref"] == publisher.reference()
            && evidence.body["key_fingerprint"] == key.spec["fingerprint"],
        "RecoveryRequired",
        "release provenance disagrees with the signed declaration",
    )
}

pub(crate) fn verify(conn: &Connection) -> Result<()> {
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions s WHERE s.kind='ApplicationReleaseSpec' AND s.rev=1
           AND s.body->'provenance'->>'source_trust'='PublisherVerified'
           AND NOT EXISTS(SELECT 1 FROM versions e WHERE e.kind=?1 AND e.rev=1
               AND e.id=s.body->'provenance'->'trust_evidence_or_admission_policy_ref'->>'resource_id'))
         OR EXISTS(SELECT 1 FROM current o JOIN versions p
              ON p.id=o.body->'plan_ref'->>'resource_id' AND p.rev=1 AND p.kind='OperationPlan'
            WHERE o.kind='Operation' AND o.body->>'lifecycle'='Succeeded'
              AND p.spec->>'action' IN ('application.install_artifact','application.stage_artifact_release')
              AND json_array_length(p.body->'preconditions') >= 3
              AND NOT EXISTS(SELECT 1 FROM versions e WHERE e.kind=?1 AND e.rev=1
                  AND e.id='oci-publisher-'||o.id))",
        [EVIDENCE], |r| r.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "signed release lacks its publisher verification",
    )?;
    let mut all = conn.prepare("SELECT * FROM versions WHERE kind=?1")?;
    for evidence in all.query_map([EVIDENCE], row)? {
        let evidence = evidence?;
        let artifact = resolve(conn, &evidence.body["artifact_ref"])?;
        let proof: OciPublisherSignature = decode(&evidence.body["signature"])?;
        let (key, publisher) = signature(conn, &artifact, &proof)?;
        let operation = evidence.spec["operation_id"].as_str().ok_or_else(|| {
            err(
                "RecoveryRequired",
                "publisher verification has no operation",
            )
        })?;
        let op = find(conn, operation)?.ok_or_else(|| {
            err(
                "RecoveryRequired",
                "publisher verification operation missing",
            )
        })?;
        let plan = resolve(conn, &op.body["plan_ref"])?;
        let input = resolve(conn, &plan.body["purpose_ref"])?;
        let input = crate::broker::dependencies::tool_input(&input)?;
        let release = super::artifact_install::recorded_release(
            conn,
            &op,
            plan.spec["action"].as_str().unwrap_or_default(),
            &artifact,
        )?;
        let spec = conn
            .query_row(
                "SELECT * FROM versions WHERE kind='ApplicationReleaseSpec' AND rev=1
               AND body->'provenance'->>'source_trust'='PublisherVerified'
               AND body->'provenance'->'trust_evidence_or_admission_policy_ref'->>'resource_id'=?1",
                [&evidence.id],
                row,
            )
            .optional()?
            .ok_or_else(|| {
                err(
                    "RecoveryRequired",
                    "publisher verification is not bound to a release",
                )
            })?;
        check_attribution(&release, &spec, &evidence, &artifact, &key, &publisher)?;
        require(
            evidence.id == format!("oci-publisher-{operation}")
                && evidence.rev == 1
                && evidence.owner == op.owner
                && op.kind == "Operation"
                && op.body["lifecycle"] == "Succeeded"
                && op.body["outcome_knowledge"] == "Known"
                && plan.kind == "OperationPlan"
                && plan.owner == op.owner
                && (spec.owner == op.owner || op.owner == policy::OWNER)
                && input["reference"] == artifact.reference()
                && input["signature"] == evidence.body["signature"]
                && plan.body["preconditions"][1]["resource_ref"] == artifact.reference()
                && plan.body["preconditions"][2]["resource_ref"] == key.reference(),
            "RecoveryRequired",
            "publisher verification disagrees with its approved artifact operation",
        )?;
    }
    Ok(())
}
