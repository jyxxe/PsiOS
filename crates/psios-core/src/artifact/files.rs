//! Custody of immutable generated OCI payloads selected by the SQLite authority.
use crate::storage::oci_archive::{MAX_BYTES, MAX_RETAINED_BYTES};
use crate::{
    Result,
    db::{Db, Doc},
    err, require,
    storage::oci_archive::Depot,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Seek,
    path::Path,
};

pub(crate) const FORMAT: u32 = crate::agent::tool_context::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("files.sql");
pub(super) const LAYOUT: &str = "ManagedOciArchiveV1";
const MAX_FILES: usize = 128;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Member {
    pub size_bytes: u64,
    pub digest: String,
}

pub(crate) fn managed(artifact: &Doc) -> bool {
    artifact.kind == "Artifact" && artifact.spec["content_layout"] == LAYOUT
}

pub(super) fn open_payload(
    conn: &Connection,
    state: Option<&Path>,
    artifact: &Doc,
    size: usize,
) -> Result<Option<File>> {
    if !managed(artifact) {
        return Ok(None);
    }
    let size = u32::try_from(size)
        .ok()
        .filter(|size| (1..=MAX_BYTES).contains(size))
        .ok_or_else(|| err("RecoveryRequired", "managed OCI archive has invalid length"))?;
    let valid: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM oci_archive_blobs b
           JOIN artifact_uploads u ON u.artifact_id=b.artifact
           WHERE b.artifact=?1 AND b.disposed=0 AND u.state='Published' AND u.owner=?2
             AND u.digest=?3 AND u.size_bytes=?4 AND u.source_kind='LocalOciImageCapture')",
        params![
            artifact.id,
            artifact.owner,
            artifact.spec["digest"].as_str(),
            size
        ],
        |r| r.get(0),
    )?;
    require(
        artifact.rev == 1 && valid,
        "RecoveryRequired",
        "managed OCI archive has no live authority record",
    )?;
    let state = state.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "managed OCI archive lacks its payload root",
        )
    })?;
    let depot = Depot::open(state)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "managed OCI archive directory is missing",
        )
    })?;
    depot.open_payload(&artifact.id, size as u64).map(Some)
}

pub(super) fn check_capacity(db: &Db<'_>) -> Result<()> {
    let count: u32 = db.c.query_row(
        "SELECT count(*) FROM oci_archive_blobs WHERE disposed=0",
        [],
        |row| row.get(0),
    )?;
    require(
        count < MAX_FILES as u32,
        "QuotaExceeded",
        "managed OCI archive slots are full",
    )
}

pub(crate) fn verify_registry(conn: &Connection) -> Result<()> {
    let missing: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM versions v WHERE v.kind='Artifact' AND v.spec->>'content_layout'=?1
            AND NOT EXISTS(SELECT 1 FROM oci_archive_blobs WHERE artifact=v.id))
         OR EXISTS(SELECT 1 FROM oci_archive_blobs b
            LEFT JOIN artifact_uploads u ON u.artifact_id=b.artifact
            LEFT JOIN versions v ON v.id=b.artifact AND v.rev=1
            WHERE u.artifact_id IS NULL OR v.id IS NULL OR v.kind<>'Artifact'
               OR v.spec->>'content_layout' IS NOT ?1 OR u.source_kind IS NOT 'LocalOciImageCapture'
               OR (b.disposed=1 AND u.state<>'Withdrawn'))",
        [LAYOUT],
        |row| row.get(0),
    )?;
    require(
        !missing,
        "RecoveryRequired",
        "managed OCI archive registry disagrees with its publication",
    )
}

pub(crate) fn selected(conn: &Connection, format: u32) -> Result<BTreeMap<String, Member>> {
    if format < FORMAT {
        return Ok(BTreeMap::new());
    }
    let mut statement = conn.prepare(
        "SELECT b.artifact,u.size_bytes,u.digest FROM oci_archive_blobs b
         JOIN artifact_uploads u ON u.artifact_id=b.artifact WHERE u.state='Published' LIMIT ?1",
    )?;
    let members = statement
        .query_map([MAX_FILES as u32 + 1], |row| {
            Ok((
                row.get(0)?,
                Member {
                    size_bytes: u64::from(row.get::<_, u32>(1)?),
                    digest: row.get(2)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    let (member_limit, aggregate_limit) = if format >= crate::apps::capture::CAPACITY_PROFILE_FORMAT
    {
        (MAX_BYTES, MAX_RETAINED_BYTES)
    } else {
        (32 * 1024 * 1024, super::ingress::TOTAL_BYTES)
    };
    require(
        members.len() <= MAX_FILES
            && members
                .values()
                .all(|member| (1..=u64::from(member_limit)).contains(&member.size_bytes))
            && members
                .values()
                .map(|member| member.size_bytes)
                .sum::<u64>()
                <= u64::from(aggregate_limit),
        "RecoveryRequired",
        "managed archive set exceeds its authority bounds",
    )?;
    Ok(members)
}

pub(crate) fn copy_checkpoint_payloads(
    state: &Path,
    destination: &Path,
    members: &BTreeMap<String, Member>,
) -> Result<()> {
    if members.is_empty() {
        return Ok(());
    }
    let source = Depot::open(state)?.ok_or_else(|| {
        err(
            "RecoveryRequired",
            "checkpoint source lacks managed OCI payloads",
        )
    })?;
    let target = Depot::create(destination)?;
    for (id, member) in members {
        let mut staged = target.stage(|dir| {
            crate::db::check_restore_capacity(dir, std::iter::once(member.size_bytes), 2, 0)
        })?;
        source.copy_payload(id, member.size_bytes, &member.digest, staged.file())?;
        staged.file().rewind()?;
        staged
            .seal(member.size_bytes, &member.digest)?
            .publish(id)?;
    }
    Ok(())
}

pub(crate) fn check_checkpoint_payloads(
    state: &Path,
    members: &BTreeMap<String, Member>,
) -> Result<()> {
    match Depot::open(state)? {
        None => require(
            members.is_empty(),
            "RecoveryRequired",
            "checkpoint managed archive directory is missing",
        ),
        Some(depot) => {
            require(
                !members.is_empty(),
                "PreconditionFailed",
                "checkpoint has no selected managed archive files",
            )?;
            depot.check_exact(&members.keys().cloned().collect())
        }
    }
}

pub(crate) fn reclaim(conn: &Connection, state: &Path) -> Result<()> {
    verify_registry(conn)?;
    let live: BTreeSet<_> = selected(conn, crate::db::CURRENT_FORMAT)?
        .into_keys()
        .collect();
    let depot = Depot::open(state)?;
    require(
        depot.is_some() || live.is_empty(),
        "RecoveryRequired",
        "managed archives are missing from authority storage",
    )?;
    if let Some(depot) = depot {
        depot.reclaim(&live)?;
    }
    Ok(())
}

pub(crate) fn next_disposal(db: &Db<'_>, after: &str) -> Result<Option<String>> {
    Ok(db
        .c
        .query_row(
            "SELECT b.artifact FROM oci_archive_blobs b
         JOIN artifact_uploads u ON u.artifact_id=b.artifact
         WHERE b.disposed=0 AND u.state='Withdrawn'
         ORDER BY b.artifact<=?1,b.artifact LIMIT 1",
            [after],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn reclaim_uncommitted(db: &Db<'_>, id: &str) -> Result<()> {
    if db.find(id)?.is_none()
        && let Some(depot) = Depot::open(&db.cfg.state_dir)?
    {
        depot.retire(id)?;
    }
    Ok(())
}

pub(crate) fn dispose(db: &Db<'_>, id: &str) -> Result<()> {
    let changed = db.c.execute(
        "UPDATE oci_archive_blobs SET disposed=1 WHERE artifact=?1
         AND EXISTS(SELECT 1 FROM artifact_uploads WHERE artifact_id=?1 AND state='Withdrawn')",
        [id],
    )?;
    require(
        changed == 1,
        "RecoveryRequired",
        "managed archive lacks a committed withdrawal",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Store, policy, storage::oci_archive::Sealed};
    use psios_api::{id, sha256};
    use serde_json::json;
    use std::io::Write;

    fn staged(config: &Config, bytes: &[u8]) -> Result<Sealed> {
        let mut staged = Depot::create(&config.state_dir)?.stage(|_| Ok(()))?;
        staged.file().write_all(bytes)?;
        staged.seal(bytes.len() as u64, &sha256(bytes))
    }

    #[test]
    fn published_payload_is_kept_and_an_aborted_publication_is_reclaimed() {
        let temporary = tempfile::tempdir().unwrap();
        let config = Config::local(&temporary.path().join("state"));
        Store::init(&config).unwrap();
        let mut store = Store::open(config.clone()).unwrap();
        store
            .transaction("upload-grant", |db| {
                policy::grant(
                    db,
                    policy::OWNER,
                    &json!({"principal_id":policy::OWNER,
                "actions":[super::super::ingress::UPLOAD],"units":2,"ttl_seconds":60,
                "tool_targets":[db.meta("authority")?]}),
                )?;
                Ok(())
            })
            .unwrap();
        let bytes = vec![0x35; super::super::CHUNK_BYTES + 5];
        let committed = id("artifact");
        let aborted = id("artifact");
        let source = json!({"kind":"LocalOciImageCapture"});
        let media = "application/vnd.psios.docker-image.v1+tar";
        let first = staged(&config, &bytes).unwrap();
        let second = staged(&config, &bytes).unwrap();
        let receipt = store
            .transaction("capture-publish", |db| {
                super::super::ingress::publish_managed_oci(
                    db,
                    policy::OWNER,
                    &committed,
                    first,
                    media,
                    source.clone(),
                )
            })
            .unwrap();
        let failure = store
            .transaction::<()>("capture-abort", |db| {
                super::super::ingress::publish_managed_oci(
                    db,
                    policy::OWNER,
                    &aborted,
                    second,
                    media,
                    source,
                )?;
                Err(err("StorageUnavailable", "checkpoint failed after publish"))
            })
            .unwrap_err();
        assert_eq!(failure.0.code, "StorageUnavailable");
        let depot_path = config.state_dir.join(crate::storage::oci_archive::ROOT);
        assert!(depot_path.join(&aborted).is_dir());
        store
            .transaction("capture-reclaim", |db| {
                reclaim_uncommitted(db, &committed)?;
                reclaim_uncommitted(db, &aborted)?;
                verify_registry(db.c)?;
                super::super::verify(db.c, Some(&db.cfg.state_dir))?;
                let artifact = db.resolve(&receipt["artifact_ref"])?;
                let mut output = Vec::new();
                super::super::copy_verified_raw(
                    db.c,
                    Some(&db.cfg.state_dir),
                    &artifact,
                    bytes.len(),
                    &mut output,
                )?;
                assert_eq!(output, bytes);
                Ok(())
            })
            .unwrap();
        assert!(depot_path.join(&committed).is_dir());
        assert!(!depot_path.join(&aborted).exists());
    }
}
