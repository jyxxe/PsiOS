use super::{CHUNK_BYTES, Manifest, files, retention, stored_digest};
use crate::{
    Result,
    db::{Db, Doc},
    decode, err, policy, require,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use psios_api::sha256;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repair {
    reference: Value,
    offset: usize,
    data_base64: String,
}

pub(super) fn supported(artifact: &Doc) -> bool {
    !files::managed(artifact) && stored_digest(artifact).is_some()
}

pub fn chunk(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let p: Repair = decode(params)?;
    let artifact = db.resolve(&p.reference)?;
    require(
        artifact.kind == "Artifact",
        "InvalidArgument",
        "expected result artifact",
    )?;
    require(
        !files::managed(&artifact),
        "UnsupportedCapability",
        "managed OCI archives do not support chunk repair",
    )?;
    retention::require_content(db.c, &artifact)?;
    let manifest = Manifest::read(&artifact)?.ok_or_else(|| {
        err(
            "UnsupportedCapability",
            "legacy inline artifacts have no separately repairable content",
        )
    })?;
    require(
        p.offset < manifest.size && p.offset.is_multiple_of(CHUNK_BYTES),
        "InvalidArgument",
        "repair offset must identify a stored chunk",
    )?;
    let expected = (manifest.size - p.offset).min(CHUNK_BYTES);
    require(
        p.data_base64.len() == 4 * expected.div_ceil(3),
        "InvalidArgument",
        "repair must supply one complete chunk",
    )?;
    let payload = STANDARD
        .decode(p.data_base64)
        .map_err(|_| err("InvalidArgument", "invalid artifact chunk encoding"))?;
    let index = p.offset / CHUNK_BYTES;
    require(
        payload.len() == expected && manifest.chunks[index] == sha256(&payload),
        "PreconditionFailed",
        "repair content differs from the original artifact chunk",
    )?;
    let replaced = db.c.execute(
        "INSERT INTO artifact_chunks(artifact_id,artifact_rev,chunk_index,payload)
         VALUES(?1,?2,?3,?4)
         ON CONFLICT(artifact_id,artifact_rev,chunk_index) DO UPDATE SET payload=excluded.payload
         WHERE artifact_chunks.payload != excluded.payload",
        params![artifact.id, artifact.rev, index as i64, payload],
    )? != 0;
    if replaced {
        db.audit(
            actor,
            "Artifact.RepairChunk",
            Some(&artifact.id),
            "Repaired",
        )?;
    }
    Ok(json!({"reference":p.reference,"offset":p.offset,"repaired":replaced}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store, artifact, storage::oci_archive::Depot};
    use std::io::{Read, Write};

    #[test]
    fn managed_archive_neither_advertises_nor_accepts_chunk_repair() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config::local(&temporary.path().join("state"));
        Store::init(&config).unwrap();
        let mut store = Store::open(config).unwrap();
        store
            .transaction("managed-repair-fixture", |db| {
                policy::grant(
                    db,
                    policy::OWNER,
                    &json!({"principal_id":policy::OWNER,"actions":[artifact::ingress::UPLOAD],
                        "tool_targets":[db.meta("authority")?],"units":1,"ttl_seconds":120}),
                )?;
                let bytes = b"immutable archive bytes";
                let depot = Depot::create(&db.cfg.state_dir)?;
                let mut stage = depot.stage(|_| Ok(()))?;
                stage.file().write_all(bytes)?;
                let sealed = stage.seal(bytes.len() as u64, &sha256(bytes))?;
                let artifact_id = psios_api::id("artifact");
                let publication = artifact::ingress::publish_managed_oci(
                    db,
                    policy::OWNER,
                    &artifact_id,
                    sealed,
                    "application/vnd.psios.docker-image.v1+tar",
                    json!({"kind":"LocalOciImageCapture"}),
                )?;
                let reference = &publication["artifact_ref"];
                let artifact = db.resolve(reference)?;
                let details = retention::describe(db, &artifact, false)?;
                assert!(details["repair_chunk_bytes"].is_null());
                for data in ["invalid base64".into(), STANDARD.encode(bytes)] {
                    let error = chunk(
                        db,
                        policy::OWNER,
                        &json!({"reference":reference,"offset":0,"data_base64":data}),
                    )
                    .unwrap_err();
                    assert_eq!(error.0.code, "UnsupportedCapability");
                }
                let chunks: u32 = db.c.query_row(
                    "SELECT count(*) FROM artifact_chunks WHERE artifact_id=?1",
                    [&artifact_id],
                    |row| row.get(0),
                )?;
                assert_eq!(chunks, 0);
                let mut current = Vec::new();
                depot
                    .open_payload(&artifact_id, bytes.len() as u64)?
                    .read_to_end(&mut current)?;
                assert_eq!(current, bytes);
                Ok(())
            })
            .unwrap();
    }
}
