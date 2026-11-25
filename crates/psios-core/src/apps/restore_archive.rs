//! Classify exact Docker save bytes without extracting files or executing the loader.
use super::valid_image_id;
use crate::{Result, artifact, decode, err, require};
use psios_api::{parse, sha256};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    cell::OnceCell,
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

pub(crate) const FORMAT: u32 = super::release_archive::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("restore_archive.sql");
const PROFILE: &str = "DockerClassicTaglessSaveV1";
const MAX_ENTRIES: usize = 512;
const MAX_METADATA: usize = 128 * 1024;
const BLOCK: usize = 512;
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Compatibility {
    Verified,
    Unsupported(&'static str),
}

impl Compatibility {
    pub fn status(self) -> &'static str {
        match self {
            Self::Verified => "Verified",
            Self::Unsupported(_) => "Unsupported",
        }
    }

    pub fn evidence(self) -> Value {
        match self {
            Self::Verified => json!({"status":"Verified","profile":PROFILE}),
            Self::Unsupported(reason) => json!({"status":"Unsupported","reason":reason}),
        }
    }

    pub fn valid_evidence(status: &str, value: &Value) -> bool {
        match status {
            "Unverified" => value.is_null(),
            "Verified" => value == &Self::Verified.evidence(),
            "Unsupported" => ["ArchiveLayout", "ManifestIdentity", "CompressedLayer"]
                .iter()
                .any(|reason| value == &Self::Unsupported(reason).evidence()),
            _ => false,
        }
    }
}

fn invalid(message: &str) -> crate::Error {
    err("PreconditionFailed", message)
}

struct Member<'a> {
    bytes: &'a [u8],
    digest: OnceCell<String>,
}

impl Member<'_> {
    fn digest(&self) -> &str {
        self.digest.get_or_init(|| sha256(self.bytes))
    }
}

struct Members<'a> {
    files: BTreeMap<String, Member<'a>>,
    directories: BTreeSet<String>,
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 192
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn octal_field(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .copied()
        .skip_while(|byte| *byte == b' ')
        .skip_while(|byte| (b'0'..=b'7').contains(byte))
        .all(|byte| matches!(byte, 0 | b' '))
}

fn zero_fill(bytes: &[u8]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}

fn unambiguous_header(header: &tar::Header) -> bool {
    let common = header.as_old();
    if ![
        common.mode.as_slice(),
        &common.uid,
        &common.gid,
        &common.size,
        &common.mtime,
        &common.cksum,
    ]
    .into_iter()
    .all(octal_field)
    {
        return false;
    }
    // Docker's Go tar reader recognizes USTAR magic without checking the version,
    // recognizes STAR by its trailer and may read malformed GNU times as a path prefix.
    // Admit only layouts whose path and size agree in both readers.
    if let Some(ustar) = header.as_ustar() {
        return octal_field(&ustar.dev_major)
            && octal_field(&ustar.dev_minor)
            && zero_fill(&ustar.pad);
    }
    if let Some(gnu) = header.as_gnu() {
        // The old GNU extension fields follow the two timestamps at byte 369.
        return octal_field(&gnu.dev_major)
            && octal_field(&gnu.dev_minor)
            && octal_field(&gnu.atime)
            && octal_field(&gnu.ctime)
            && zero_fill(&header.as_bytes()[369..]);
    }
    zero_fill(&common.pad)
}

impl<'a> Members<'a> {
    fn read(bytes: &'a [u8]) -> Result<Self> {
        require(
            bytes.len().is_multiple_of(BLOCK),
            "PreconditionFailed",
            "OCI archive has incomplete tar blocks",
        )?;
        let mut tar = tar::Archive::new(Cursor::new(bytes));
        let mut members = Self {
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        let mut next = 0usize;
        for (ordinal, entry) in tar
            .entries()
            .map_err(|_| invalid("unreadable OCI tar archive"))?
            .raw(true)
            .enumerate()
        {
            require(
                ordinal < MAX_ENTRIES,
                "QuotaExceeded",
                "OCI archive has too many tar entries",
            )?;
            let entry = entry.map_err(|_| invalid("unreadable OCI tar entry"))?;
            let header = entry.header();
            require(
                unambiguous_header(header),
                "PreconditionFailed",
                "OCI tar header has ambiguous format or numeric fields",
            )?;
            let directory = header.entry_type().is_dir();
            require(
                directory || header.entry_type().is_file(),
                "PreconditionFailed",
                "OCI tar has unsupported metadata or entry type",
            )?;
            require(
                header.link_name_bytes().is_none() && (!directory || entry.size() == 0),
                "PreconditionFailed",
                "OCI tar has invalid member metadata",
            )?;
            let raw = header.path_bytes();
            let raw = std::str::from_utf8(&raw)
                .map_err(|_| invalid("OCI tar member has an invalid path"))?;
            let path = if directory {
                raw.strip_suffix('/').unwrap_or(raw)
            } else {
                raw
            };
            let start = next + BLOCK;
            require(
                entry.raw_header_position() == next as u64
                    && entry.raw_file_position() == start as u64,
                "PreconditionFailed",
                "OCI tar member offsets disagree",
            )?;
            let size = usize::try_from(entry.size())
                .map_err(|_| invalid("OCI tar member size overflows"))?;
            let end = start
                .checked_add(size)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| invalid("OCI tar member is truncated"))?;
            next = end.next_multiple_of(BLOCK);
            require(
                next <= bytes.len() && bytes[end..next].iter().all(|b| *b == 0),
                "PreconditionFailed",
                "OCI tar padding is invalid",
            )?;
            if directory && ordinal == 0 && path == "." {
                continue;
            }
            require(
                safe_path(path),
                "PreconditionFailed",
                "OCI tar member has an invalid path",
            )?;
            let unique = if directory {
                !members.files.contains_key(path) && members.directories.insert(path.into())
            } else {
                !members.directories.contains(path)
                    && members
                        .files
                        .insert(
                            path.into(),
                            Member {
                                bytes: &bytes[start..end],
                                digest: OnceCell::new(),
                            },
                        )
                        .is_none()
            };
            require(
                unique,
                "PreconditionFailed",
                "OCI tar member path is duplicated",
            )?;
        }
        require(
            !members.files.is_empty()
                && bytes.len().saturating_sub(next) >= 2 * BLOCK
                && bytes[next..].iter().all(|b| *b == 0),
            "PreconditionFailed",
            "OCI tar has no unambiguous end marker",
        )?;
        for path in members.files.keys().chain(&members.directories) {
            for (separator, _) in path.match_indices('/') {
                require(
                    !members.files.contains_key(&path[..separator]),
                    "PreconditionFailed",
                    "OCI tar file conflicts with a directory",
                )?;
            }
        }
        require(
            !members.files.contains_key("repositories")
                && !members.directories.contains("repositories"),
            "PreconditionFailed",
            "OCI archive contains repository tag metadata",
        )?;
        Ok(members)
    }

    fn file(&self, path: &str) -> Result<&Member<'a>> {
        self.files
            .get(path)
            .ok_or_else(|| invalid("OCI archive metadata references a missing tar member"))
    }

    fn metadata(&self, path: &str) -> Result<Value> {
        let bytes = self.file(path)?.bytes;
        require(
            bytes.len() <= MAX_METADATA,
            "QuotaExceeded",
            "OCI archive metadata exceeds byte limit",
        )?;
        parse(bytes).map_err(|_| invalid("OCI archive metadata is malformed or ambiguous"))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "PascalCase")]
struct DockerManifest {
    config: String,
    layers: Vec<String>,
    repo_tags: Option<Vec<String>>,
    parent: Option<String>,
    layer_sources: Option<BTreeMap<String, Descriptor>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
    size: usize,
    urls: Option<Vec<String>>,
    annotations: Option<BTreeMap<String, String>>,
    platform: Option<Value>,
    data: Option<String>,
    artifact_type: Option<String>,
}

fn allowed_fields(value: &Value, fields: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.keys().all(|key| fields.contains(&key.as_str())))
}

fn no_annotations(annotations: &Value) -> bool {
    annotations.is_null()
        || annotations
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
}

impl Descriptor {
    fn member(&self, members: &Members<'_>) -> Result<String> {
        require(
            valid_image_id(&self.digest)
                && self.urls.as_ref().is_none_or(Vec::is_empty)
                && self.data.is_none()
                && self.artifact_type.is_none()
                && self.annotations.as_ref().is_none_or(BTreeMap::is_empty)
                && self.platform.as_ref().is_none_or(|p| {
                    allowed_fields(p, &["os", "architecture", "variant"])
                        && p["os"] == "linux"
                        && p["architecture"] == "amd64"
                        && matches!(p["variant"].as_str(), None | Some("" | "v1"))
                }),
            "PreconditionFailed",
            "OCI descriptor has a tag, annotation, external source or unsupported identity",
        )?;
        let path = format!("blobs/sha256/{}", &self.digest[7..]);
        let data = members.file(&path)?;
        require(
            data.bytes.len() == self.size && data.digest() == self.digest,
            "PreconditionFailed",
            "OCI descriptor size or digest disagrees with the archive",
        )?;
        Ok(path)
    }
}

fn layer_media(media: &str) -> Result<bool> {
    match media {
        OCI_LAYER | DOCKER_LAYER => Ok(false),
        "application/vnd.oci.image.layer.v1.tar+gzip"
        | "application/vnd.oci.image.layer.v1.tar+zstd"
        | "application/vnd.docker.image.rootfs.diff.tar.gzip" => Ok(true),
        _ => Err(invalid(
            "OCI archive contains an unsupported or foreign layer descriptor",
        )),
    }
}

fn validate_oci(members: &Members<'_>, docker: &DockerManifest) -> Result<Option<(String, bool)>> {
    let layout = members.files.contains_key("oci-layout");
    let index = members.files.contains_key("index.json");
    require(
        layout == index,
        "PreconditionFailed",
        "OCI index and layout marker must appear together",
    )?;
    if !layout {
        return Ok(None);
    }
    let layout = members.metadata("oci-layout")?;
    let index = members.metadata("index.json")?;
    require(
        allowed_fields(&layout, &["imageLayoutVersion"])
            && layout["imageLayoutVersion"] == "1.0.0"
            && allowed_fields(
                &index,
                &["schemaVersion", "mediaType", "manifests", "annotations"],
            )
            && index["schemaVersion"] == 2
            && (index["mediaType"].is_null() || index["mediaType"] == OCI_INDEX)
            && no_annotations(&index["annotations"]),
        "PreconditionFailed",
        "OCI archive index has unsupported fields or annotations",
    )?;
    let references = index["manifests"]
        .as_array()
        .filter(|refs| refs.len() == 1)
        .ok_or_else(|| invalid("OCI archive index must contain exactly one image"))?;
    let descriptor: Descriptor =
        decode(&references[0]).map_err(|_| invalid("OCI image descriptor is malformed"))?;
    require(
        matches!(
            descriptor.media_type.as_str(),
            OCI_MANIFEST | DOCKER_MANIFEST
        ),
        "PreconditionFailed",
        "OCI archive index does not identify a direct image",
    )?;
    let manifest = members.metadata(&descriptor.member(members)?)?;
    require(
        allowed_fields(
            &manifest,
            &[
                "schemaVersion",
                "mediaType",
                "config",
                "layers",
                "annotations",
            ],
        ) && manifest["schemaVersion"] == 2
            && (manifest["mediaType"].is_null() || manifest["mediaType"] == descriptor.media_type)
            && no_annotations(&manifest["annotations"]),
        "PreconditionFailed",
        "OCI image manifest has unsupported fields or annotations",
    )?;
    let config: Descriptor = decode(&manifest["config"])
        .map_err(|_| invalid("OCI configuration descriptor is malformed"))?;
    require(
        matches!(config.media_type.as_str(), OCI_CONFIG | DOCKER_CONFIG)
            && config.member(members)? == docker.config,
        "PreconditionFailed",
        "Docker and OCI manifests identify different image configurations",
    )?;
    let layers: Vec<Descriptor> =
        decode(&manifest["layers"]).map_err(|_| invalid("OCI layer descriptors are malformed"))?;
    require(
        layers.len() == docker.layers.len(),
        "PreconditionFailed",
        "Docker and OCI manifests contain different layer counts",
    )?;
    let mut compressed = false;
    for (layer, path) in layers.iter().zip(&docker.layers) {
        require(
            layer.member(members)? == *path,
            "PreconditionFailed",
            "Docker and OCI manifests identify different layer bytes",
        )?;
        compressed |= layer_media(&layer.media_type)?;
    }
    Ok(Some((descriptor.digest, compressed)))
}

fn compressed(bytes: &[u8]) -> bool {
    [
        b"\x1f\x8b".as_slice(),
        b"\x28\xb5\x2f\xfd",
        b"BZh",
        b"\xfd7zXZ\0",
    ]
    .into_iter()
    .any(|magic| bytes.starts_with(magic))
}

fn hex_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn compatible_paths(
    members: &Members<'_>,
    docker: &DockerManifest,
    config: &str,
    has_index: bool,
) -> Result<bool> {
    let config_hex = &config[7..];
    let modern = docker.config == format!("blobs/sha256/{config_hex}")
        && docker
            .layers
            .iter()
            .all(|path| path.strip_prefix("blobs/sha256/").is_some_and(hex_name));
    if modern {
        for (path, member) in &members.files {
            if let Some(name) = path.strip_prefix("blobs/sha256/") {
                require(
                    hex_name(name) && member.digest().strip_prefix("sha256:") == Some(name),
                    "PreconditionFailed",
                    "OCI blob filename disagrees with its digest",
                )?;
            } else if !matches!(path.as_str(), "manifest.json" | "index.json" | "oci-layout") {
                return Ok(false);
            }
        }
        return Ok(members
            .directories
            .iter()
            .all(|dir| matches!(dir.as_str(), "blobs" | "blobs/sha256")));
    }
    if has_index || docker.config != format!("{config_hex}.json") {
        return Ok(false);
    }
    let dirs: Option<BTreeSet<&str>> = docker
        .layers
        .iter()
        .map(|path| path.strip_suffix("/layer.tar").filter(|dir| hex_name(dir)))
        .collect();
    let Some(dirs) = dirs else {
        return Ok(false);
    };
    Ok(members
        .directories
        .iter()
        .all(|dir| dirs.contains(dir.as_str()))
        && members.files.keys().all(|path| {
            path == "manifest.json"
                || path == &docker.config
                || path.split_once('/').is_some_and(|(dir, name)| {
                    dirs.contains(dir) && matches!(name, "layer.tar" | "VERSION" | "json")
                })
        }))
}

pub(super) fn validate(bytes: &[u8], image: &str, diff_ids: &[String]) -> Result<Compatibility> {
    require(
        bytes.len() <= artifact::ingress::MAX_BYTES as usize,
        "QuotaExceeded",
        "OCI archive exceeds byte limit",
    )?;
    if compressed(bytes) {
        return Ok(Compatibility::Unsupported("ArchiveLayout"));
    }
    let members = Members::read(bytes)?;
    validate_members(&members, image, diff_ids)
}

fn validate_members(
    members: &Members<'_>,
    image: &str,
    diff_ids: &[String],
) -> Result<Compatibility> {
    if !members.files.contains_key("manifest.json") {
        return Ok(Compatibility::Unsupported("ArchiveLayout"));
    }
    let manifests: Vec<DockerManifest> = decode(&members.metadata("manifest.json")?)
        .map_err(|_| invalid("Docker archive manifest is malformed"))?;
    let [docker] = manifests.as_slice() else {
        return Err(invalid("Docker archive must contain exactly one image"));
    };
    require(
        docker.repo_tags.as_ref().is_none_or(Vec::is_empty)
            && docker.parent.as_ref().is_none_or(String::is_empty),
        "PreconditionFailed",
        "Docker archive contains tags or a parent image",
    )?;
    let config_digest = members.file(&docker.config)?.digest();
    let config = members.metadata(&docker.config)?;
    require(
        config["os"] == "linux"
            && config["architecture"] == "amd64"
            && config["rootfs"]["type"] == "layers"
            && config["rootfs"]["diff_ids"] == json!(diff_ids)
            && (1..=128).contains(&docker.layers.len())
            && docker.layers.len() == diff_ids.len(),
        "PreconditionFailed",
        "OCI archive config disagrees with the inspected image platform or layers",
    )?;
    let mut compressed_layer = false;
    for (path, diff_id) in docker.layers.iter().zip(diff_ids) {
        let payload = members.file(path)?;
        if compressed(payload.bytes) {
            compressed_layer = true;
        } else {
            require(
                payload.digest() == *diff_id,
                "PreconditionFailed",
                "OCI archive layer disagrees with the inspected DiffID",
            )?;
        }
    }

    let oci = validate_oci(members, docker)?;
    if let Some(sources) = &docker.layer_sources {
        for (diff_id, descriptor) in sources {
            let position = diff_ids
                .iter()
                .position(|diff| diff == diff_id)
                .ok_or_else(|| invalid("Docker layer source has no matching inspected DiffID"))?;
            require(
                descriptor.member(members)? == docker.layers[position],
                "PreconditionFailed",
                "Docker layer source disagrees with its archive path",
            )?;
            compressed_layer |= layer_media(&descriptor.media_type)?;
        }
    }
    if config_digest != image {
        if oci.as_ref().is_some_and(|(manifest, _)| manifest == image) {
            return Ok(Compatibility::Unsupported("ManifestIdentity"));
        }
        return Err(invalid(
            "OCI archive configuration digest differs from the inspected image ID",
        ));
    }
    if compressed_layer || oci.as_ref().is_some_and(|(_, compressed)| *compressed) {
        return Ok(Compatibility::Unsupported("CompressedLayer"));
    }
    if !compatible_paths(members, docker, config_digest, oci.is_some())? {
        return Ok(Compatibility::Unsupported("ArchiveLayout"));
    }
    Ok(Compatibility::Verified)
}

#[cfg(test)]
mod tests;
