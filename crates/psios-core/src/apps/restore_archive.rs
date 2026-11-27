//! Classify exact Docker save bytes without extracting files or executing the loader.
mod compression;
use super::valid_image_id;
use crate::{Result, decode, err, require, storage::oci_archive::MAX_BYTES};
use psios_api::parse;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{OnceCell, RefCell, RefMut},
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Seek, SeekFrom, Write},
};

pub(crate) const FORMAT: u32 = super::release_archive::FORMAT + 1;
pub(crate) const SCHEMA: &str = include_str!("restore_archive.sql");
pub(crate) const MANIFEST_IDENTITY_FORMAT: u32 = super::capture::CAPACITY_PROFILE_FORMAT + 1;
pub(crate) const ZSTD_FORMAT: u32 = crate::artifact::ingress::windows::FORMAT + 1;
const PROFILE: &str = "DockerClassicTaglessSaveV1";
const OCI_PROFILE: &str = "DockerOciTaglessSaveV1";
const MAX_ENTRIES: usize = 512;
const MAX_METADATA: usize = 128 * 1024;
const BLOCK: usize = 512;
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
const OCI_LAYER: &str = "application/vnd.oci.image.layer.v1.tar";
const OCI_GZIP_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
const OCI_ZSTD_LAYER: &str = "application/vnd.oci.image.layer.v1.tar+zstd";
const DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const DOCKER_CONFIG: &str = "application/vnd.docker.container.image.v1+json";
const DOCKER_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar";
const DOCKER_GZIP_LAYER: &str = "application/vnd.docker.image.rootfs.diff.tar.gzip";

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Compatibility {
    Verified,
    VerifiedManifest(String),
    Unsupported(&'static str),
}

#[derive(Clone, Copy)]
pub(super) enum ManifestLayers {
    TarAndGzip,
    TarGzipAndZstd,
}

impl ManifestLayers {
    pub(super) fn supported_at(format: u32) -> Self {
        if format >= ZSTD_FORMAT {
            Self::TarGzipAndZstd
        } else {
            Self::TarAndGzip
        }
    }
}

impl Compatibility {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Verified | Self::VerifiedManifest(_) => "Verified",
            Self::Unsupported(_) => "Unsupported",
        }
    }

    pub fn evidence(&self) -> Value {
        match self {
            Self::Verified => json!({"status":"Verified","profile":PROFILE}),
            Self::VerifiedManifest(configuration_digest) => {
                json!({"status":"Verified","profile":OCI_PROFILE,"config_digest":configuration_digest})
            }
            Self::Unsupported(reason) => json!({"status":"Unsupported","reason":reason}),
        }
    }

    pub fn valid_evidence(status: &str, value: &Value) -> bool {
        match status {
            "Unverified" => value.is_null(),
            "Verified" => {
                value == &Self::Verified.evidence()
                    || value["config_digest"]
                        .as_str()
                        .filter(|digest| valid_image_id(digest))
                        .is_some_and(|digest| {
                            value == &Self::VerifiedManifest(digest.to_owned()).evidence()
                        })
            }
            "Unsupported" => ["ArchiveLayout", "ManifestIdentity", "CompressedLayer"]
                .iter()
                .any(|reason| value == &Self::Unsupported(reason).evidence()),
            _ => false,
        }
    }
}

pub(super) fn manifest_profile(evidence: &Value) -> bool {
    evidence["profile"] == OCI_PROFILE
}

fn invalid(message: &str) -> crate::Error {
    err("PreconditionFailed", message)
}

struct Member {
    offset: u64,
    size: u64,
    digest: OnceCell<String>,
}

// Each view has an independent, member-relative offset. tar's inner reader must
// never seek or read past the outer member into a different file.
struct MemberInput<'a, R> {
    reader: RefMut<'a, R>,
    member: &'a Member,
    position: u64,
}

impl<R: Read> Read for MemberInput<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let n = buffer
            .len()
            .min((self.member.size - self.position) as usize);
        let read = self.reader.read(&mut buffer[..n])?;
        self.position += read as u64;
        Ok(read)
    }
}

impl<R: Seek> Seek for MemberInput<'_, R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::End(offset) => self.member.size.checked_add_signed(offset),
            SeekFrom::Current(offset) => self.position.checked_add_signed(offset),
        }
        .filter(|position| *position <= self.member.size)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "seek outside OCI tar member")
        })?;
        self.reader
            .seek(SeekFrom::Start(self.member.offset + position))?;
        self.position = position;
        Ok(position)
    }
}

struct Members<R> {
    input: RefCell<R>,
    files: BTreeMap<String, Member>,
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

fn zeros(input: &mut (impl Read + Seek), offset: u64, mut size: u64) -> Result<bool> {
    input.seek(SeekFrom::Start(offset))?;
    let mut buffer = [0u8; 8192];
    while size > 0 {
        let length = size.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..length])?;
        if !zero_fill(&buffer[..length]) {
            return Ok(false);
        }
        size -= length as u64;
    }
    Ok(true)
}

impl<R: Read + Seek> Members<R> {
    fn read(mut input: R, length: u64) -> Result<Self> {
        require(
            length.is_multiple_of(BLOCK as u64),
            "PreconditionFailed",
            "OCI archive has incomplete tar blocks",
        )?;
        input.rewind()?;
        let mut tar = tar::Archive::new(&mut input);
        let mut files = BTreeMap::new();
        let mut directories = BTreeSet::new();
        let mut padding = Vec::new();
        let mut next = 0u64;
        for (ordinal, entry) in tar
            .entries_with_seek()
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
            let start = next + BLOCK as u64;
            require(
                entry.raw_header_position() == next && entry.raw_file_position() == start,
                "PreconditionFailed",
                "OCI tar member offsets disagree",
            )?;
            let size = entry.size();
            let end = start
                .checked_add(size)
                .filter(|end| *end <= length)
                .ok_or_else(|| invalid("OCI tar member is truncated"))?;
            next = end.next_multiple_of(BLOCK as u64);
            if next > end {
                padding.push((end, next - end));
            }
            if directory && ordinal == 0 && path == "." {
                continue;
            }
            require(
                safe_path(path),
                "PreconditionFailed",
                "OCI tar member has an invalid path",
            )?;
            let unique = if directory {
                !files.contains_key(path) && directories.insert(path.to_owned())
            } else {
                !directories.contains(path)
                    && files
                        .insert(
                            path.to_owned(),
                            Member {
                                offset: start,
                                size,
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
            !files.is_empty()
                && length.saturating_sub(next) >= (2 * BLOCK) as u64
                && zeros(&mut input, next, length - next)?,
            "PreconditionFailed",
            "OCI tar has no unambiguous end marker",
        )?;
        for (start, size) in padding {
            require(
                zeros(&mut input, start, size)?,
                "PreconditionFailed",
                "OCI tar padding is invalid",
            )?;
        }
        for path in files.keys().chain(&directories) {
            for (separator, _) in path.match_indices('/') {
                require(
                    !files.contains_key(&path[..separator]),
                    "PreconditionFailed",
                    "OCI tar file conflicts with a directory",
                )?;
            }
        }
        require(
            !files.contains_key("repositories") && !directories.contains("repositories"),
            "PreconditionFailed",
            "OCI archive contains repository tag metadata",
        )?;
        Ok(Self {
            input: RefCell::new(input),
            files,
            directories,
        })
    }

    fn file(&self, path: &str) -> Result<&Member> {
        self.files
            .get(path)
            .ok_or_else(|| invalid("OCI archive metadata references a missing tar member"))
    }

    fn reader<'a>(&'a self, member: &'a Member) -> Result<MemberInput<'a, R>> {
        let mut reader = self.input.borrow_mut();
        reader.seek(SeekFrom::Start(member.offset))?;
        Ok(MemberInput {
            reader,
            member,
            position: 0,
        })
    }

    fn digest<'a>(&self, member: &'a Member) -> Result<&'a str> {
        if let Some(digest) = member.digest.get() {
            return Ok(digest);
        }
        let mut reader = self.reader(member)?;
        let mut digest = Sha256::new();
        require(
            io::copy(&mut reader, &mut digest)? == member.size,
            "PreconditionFailed",
            "OCI tar member is truncated",
        )?;
        Ok(member
            .digest
            .get_or_init(|| format!("sha256:{:x}", digest.finalize())))
    }

    fn metadata(&self, path: &str) -> Result<Value> {
        let member = self.file(path)?;
        require(
            member.size <= MAX_METADATA as u64,
            "QuotaExceeded",
            "OCI archive metadata exceeds byte limit",
        )?;
        let mut bytes = vec![0; member.size as usize];
        self.reader(member)?.read_exact(&mut bytes)?;
        parse(&bytes).map_err(|_| invalid("OCI archive metadata is malformed or ambiguous"))
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
    fn member(&self, members: &Members<impl Read + Seek>) -> Result<String> {
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
            data.size == self.size as u64 && members.digest(data)? == self.digest,
            "PreconditionFailed",
            "OCI descriptor size or digest disagrees with the archive",
        )?;
        Ok(path)
    }
}

fn layer_media(media: &str) -> Result<bool> {
    match media {
        OCI_LAYER | DOCKER_LAYER => Ok(false),
        OCI_GZIP_LAYER | OCI_ZSTD_LAYER | DOCKER_GZIP_LAYER => Ok(true),
        _ => Err(invalid(
            "OCI archive contains an unsupported or foreign layer descriptor",
        )),
    }
}

struct OciGraph {
    manifest_digest: String,
    layers: Vec<Descriptor>,
    compressed: bool,
}

fn validate_oci(
    members: &Members<impl Read + Seek>,
    docker: &DockerManifest,
) -> Result<Option<OciGraph>> {
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
    Ok(Some(OciGraph {
        manifest_digest: descriptor.digest,
        layers,
        compressed,
    }))
}

fn compressed(mut input: impl Read, length: u64) -> Result<bool> {
    let mut bytes = [0; 6];
    let bytes = &mut bytes[..length.min(6) as usize];
    input.read_exact(bytes)?;
    Ok([
        b"\x1f\x8b".as_slice(),
        b"\x28\xb5\x2f\xfd",
        b"BZh",
        b"\xfd7zXZ\0",
    ]
    .into_iter()
    .any(|magic| bytes.starts_with(magic)))
}

fn hex_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn compatible_paths(
    members: &Members<impl Read + Seek>,
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
                    hex_name(name) && members.digest(member)?.strip_prefix("sha256:") == Some(name),
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

const BYTE_LIMIT: u64 = MAX_BYTES as u64;

fn input_length(input: &mut impl Seek) -> Result<u64> {
    let length = input.seek(SeekFrom::End(0))?;
    require(
        length <= BYTE_LIMIT,
        "QuotaExceeded",
        "OCI archive exceeds byte limit",
    )?;
    input.rewind()?;
    Ok(length)
}

pub(super) fn validate_reader(
    mut input: impl Read + Seek,
    image: &str,
    diff_ids: &[String],
    manifest_layers: Option<ManifestLayers>,
) -> Result<Compatibility> {
    let length = input_length(&mut input)?;
    if compressed(&mut input, length)? {
        return Ok(Compatibility::Unsupported("ArchiveLayout"));
    }
    let members = Members::read(input, length)?;
    validate_members(&members, image, diff_ids, manifest_layers)
}

// Supply canonical tar headers and only the manifest format selected at capture.
pub(super) fn write_load(
    mut input: impl Read + Seek,
    output: impl Write,
    image: &str,
    diff_ids: &[String],
    expected_config: Option<&str>,
) -> Result<String> {
    let length = input_length(&mut input)?;
    require(
        !compressed(&mut input, length)?,
        "UnsupportedCapability",
        "restore requires an uncompressed outer tar",
    )?;
    let members = Members::read(input, length)?;
    let manifest_layers = expected_config.map(|_| ManifestLayers::TarGzipAndZstd);
    let compatibility = validate_members(&members, image, diff_ids, manifest_layers)?;
    let manifest_identity = match (expected_config, compatibility) {
        (None, Compatibility::Verified) => false,
        (Some(expected), Compatibility::VerifiedManifest(actual)) => {
            require(
                expected == actual,
                "PreconditionFailed",
                "OCI archive configuration disagrees with the captured config digest",
            )?;
            true
        }
        _ => {
            return Err(err(
                "UnsupportedCapability",
                "captured archive does not meet its recorded Docker restore profile",
            ));
        }
    };
    let manifests: Vec<DockerManifest> = decode(&members.metadata("manifest.json")?)?;
    let docker = &manifests[0];
    if manifest_identity {
        return write_oci_load(&members, docker, image, output);
    }
    let config = members.file(&docker.config)?;
    let config_path = format!("blobs/sha256/{}", &image[7..]);
    let mut layers = Vec::with_capacity(diff_ids.len());
    let mut entries = vec![(config_path.clone(), config)];
    let mut seen = BTreeSet::new();
    let mut remaining = BYTE_LIMIT;
    for (path, diff_id) in docker.layers.iter().zip(diff_ids) {
        let layer = members.file(path)?;
        remaining = remaining.checked_sub(layer.size).ok_or_else(|| {
            err(
                "QuotaExceeded",
                "OCI restore layer chain exceeds the cumulative byte limit",
            )
        })?;
        let path = format!("blobs/sha256/{}", &diff_id[7..]);
        if seen.insert(diff_id) {
            // Skip PAX, GNU sparse and other extension records that could change
            // the effective member size in Docker's inner layer reader.
            check_inner_layer(members.reader(layer)?, layer.size)?;
            entries.push((path.clone(), layer));
        }
        layers.push(path);
    }
    let manifest =
        serde_json::to_vec(&json!([{"Config":config_path,"RepoTags":[],"Layers":layers}]))?;
    let member_space = |size: u64| BLOCK as u64 + size.next_multiple_of(BLOCK as u64);
    let length = entries
        .iter()
        .map(|(_, member)| member_space(member.size))
        .sum::<u64>()
        + member_space(manifest.len() as u64)
        + (2 * BLOCK) as u64;
    require(
        length <= BYTE_LIMIT,
        "QuotaExceeded",
        "normalized OCI archive exceeds load input limit",
    )?;
    let mut canonical = tar::Builder::new(HashOutput::new(output));
    for (path, member) in entries {
        append_canonical(&mut canonical, &path, member.size, members.reader(member)?)?;
    }
    append_canonical(
        &mut canonical,
        "manifest.json",
        manifest.len() as u64,
        &manifest[..],
    )?;
    let mut output = canonical.into_inner()?;
    output.flush()?;
    Ok(output.finish())
}

fn write_oci_load(
    members: &Members<impl Read + Seek>,
    docker: &DockerManifest,
    image: &str,
    output: impl Write,
) -> Result<String> {
    let manifest_blob = format!("blobs/sha256/{}", &image[7..]);
    let paths = ["index.json", "oci-layout", &manifest_blob, &docker.config]
        .into_iter()
        .chain(docker.layers.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    let mut archive = tar::Builder::new(HashOutput::new(output));
    for path in paths {
        let member = members.file(path)?;
        append_canonical(&mut archive, path, member.size, members.reader(member)?)?;
    }
    let mut output = archive.into_inner()?;
    output.flush()?;
    Ok(output.finish())
}

pub(super) struct HashOutput<W> {
    output: W,
    digest: Sha256,
}

impl<W> HashOutput<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            digest: Sha256::new(),
        }
    }

    pub fn finish(self) -> String {
        format!("sha256:{:x}", self.digest.finalize())
    }
}

impl<W: Write> Write for HashOutput<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.output.write(bytes)?;
        self.digest.update(&bytes[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn append_canonical(
    tar: &mut tar::Builder<impl Write>,
    path: &str,
    size: u64,
    data: impl Read,
) -> Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_path(path)?;
    header.set_size(size);
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    let mut data = data.take(size);
    tar.append(&header, &mut data)?;
    require(
        data.limit() == 0,
        "PreconditionFailed",
        "OCI tar member is truncated",
    )
}

fn check_inner_layer(mut input: impl Read + Seek, length: u64) -> Result<()> {
    let mut layer = tar::Archive::new(&mut input);
    let mut position = 0u64;
    for entry in layer.entries_with_seek()?.raw(true) {
        let entry = entry.map_err(|_| invalid("malformed OCI layer tar"))?;
        let header = entry.header();
        let old = header.as_old();
        let kind = old.linkflag[0];
        let ordinary_file = kind == b'0'
            || kind == 0
                && old
                    .name
                    .split(|v| *v == 0)
                    .next()
                    .is_some_and(|name| !name.is_empty() && !name.ends_with(b"/"));
        require(
            (ordinary_file || matches!(kind, b'1' | b'2' | b'5') && entry.size() == 0)
                && old.size[11] == 0
                && old.size[..11].iter().all(|v| matches!(v, b'0'..=b'7'))
                && unambiguous_header(header),
            "UnsupportedCapability",
            "OCI restore layer uses unsupported tar extension or filesystem entry type",
        )?;
        let start = position + BLOCK as u64;
        require(
            entry.raw_header_position() == position && entry.raw_file_position() == start,
            "PreconditionFailed",
            "OCI layer has ambiguous tar member offsets",
        )?;
        position = start
            .checked_add(entry.size())
            .filter(|end| *end <= length)
            .ok_or_else(|| invalid("OCI layer tar member is truncated"))?
            .next_multiple_of(BLOCK as u64);
    }
    require(
        length.is_multiple_of(BLOCK as u64)
            && length.saturating_sub(position) >= (2 * BLOCK) as u64
            && zeros(&mut input, position, length - position)?,
        "PreconditionFailed",
        "OCI layer has no unambiguous tar end marker",
    )
}

fn validate_members(
    members: &Members<impl Read + Seek>,
    image: &str,
    diff_ids: &[String],
    manifest_layers: Option<ManifestLayers>,
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
    let config_digest = members.digest(members.file(&docker.config)?)?;
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
        if compressed(members.reader(payload)?, payload.size)? {
            compressed_layer = true;
        } else {
            require(
                members.digest(payload)? == *diff_id,
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
        if let Some(graph) = oci.as_ref().filter(|graph| graph.manifest_digest == image) {
            if let Some(profile) = manifest_layers
                && compatible_paths(members, docker, config_digest, true)?
            {
                match verify_manifest_layers(members, graph, docker, diff_ids, profile) {
                    Ok(true) => {
                        return Ok(Compatibility::VerifiedManifest(config_digest.to_owned()));
                    }
                    Err(error) if error.0.code != "UnsupportedCapability" => return Err(error),
                    Ok(false) | Err(_) => {}
                }
            }
            return Ok(Compatibility::Unsupported("ManifestIdentity"));
        }
        return Err(invalid(
            "OCI archive configuration digest differs from the inspected image ID",
        ));
    }
    if compressed_layer || oci.as_ref().is_some_and(|graph| graph.compressed) {
        return Ok(Compatibility::Unsupported("CompressedLayer"));
    }
    if !compatible_paths(members, docker, config_digest, oci.is_some())? {
        return Ok(Compatibility::Unsupported("ArchiveLayout"));
    }
    Ok(Compatibility::Verified)
}

fn verify_manifest_layers(
    members: &Members<impl Read + Seek>,
    graph: &OciGraph,
    docker: &DockerManifest,
    diff_ids: &[String],
    profile: ManifestLayers,
) -> Result<bool> {
    let mut remaining = BYTE_LIMIT;
    for ((descriptor, path), diff_id) in graph.layers.iter().zip(&docker.layers).zip(diff_ids) {
        let payload = members.file(path)?;
        let size = match descriptor.media_type.as_str() {
            OCI_GZIP_LAYER | DOCKER_GZIP_LAYER => {
                compression::gzip(members.reader(payload)?, remaining, diff_id)?
            }
            OCI_ZSTD_LAYER if matches!(profile, ManifestLayers::TarGzipAndZstd) => {
                compression::zstd(members.reader(payload)?, remaining, diff_id)?
            }
            OCI_LAYER | DOCKER_LAYER => {
                require(
                    payload.size <= remaining,
                    "QuotaExceeded",
                    "OCI layer chain exceeds the byte limit",
                )?;
                require(
                    members.digest(payload)? == *diff_id,
                    "PreconditionFailed",
                    "OCI layer disagrees with its inspected DiffID",
                )?;
                check_inner_layer(members.reader(payload)?, payload.size)?;
                payload.size
            }
            _ => return Ok(false),
        };
        remaining -= size;
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
