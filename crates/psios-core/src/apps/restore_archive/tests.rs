use super::*;
use psios_api::sha256;
use std::io::Cursor;
use tar::{Builder, EntryType, Header};

fn validate(bytes: &[u8], image: &str, diff_ids: &[String]) -> Result<Compatibility> {
    validate_reader(Cursor::new(bytes), image, diff_ids, false)
}

fn canonical_load(bytes: &[u8], image: &str, diff_ids: &[String]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let digest = write_load(Cursor::new(bytes), &mut output, image, diff_ids, false)?;
    assert_eq!(digest, sha256(&output));
    Ok(output)
}

fn encode(members: &BTreeMap<String, Vec<u8>>) -> Vec<u8> {
    let mut out = Builder::new(Vec::new());
    for (path, content) in members {
        let mut header = Header::new_ustar();
        header.set_mode(0o644);
        header.set_size(content.len() as u64);
        out.append_data(&mut header, path, content.as_slice())
            .unwrap();
    }
    out.into_inner().unwrap()
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

fn blob(digest: &str) -> String {
    format!("blobs/sha256/{}", &digest[7..])
}

fn descriptor(payload: &[u8], media_type: &str) -> Value {
    json!({"size":payload.len(),"digest":sha256(payload),"mediaType":media_type})
}

fn gzip_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoded.write_all(bytes).unwrap();
    encoded.finish().unwrap()
}

#[test]
fn manifest_identity_keeps_original_blobs_in_an_oci_only_load() {
    let mut fixture = Fixture::new(true);
    let previous_layer = fixture.docker["Layers"][0].as_str().unwrap().to_owned();
    let plain = fixture.members.remove(&previous_layer).unwrap();
    let encoded = gzip_bytes(&plain);
    let layer = descriptor(&encoded, OCI_GZIP_LAYER);
    let layer_path = blob(layer["digest"].as_str().unwrap());
    fixture.members.insert(layer_path.clone(), encoded);
    let config_path = fixture.docker["Config"].as_str().unwrap().to_owned();
    fixture.docker["Layers"] = json!([layer_path]);
    fixture.docker["LayerSources"] = json!({(fixture.layers[0].clone()):layer});
    let manifest = json!({"schemaVersion":2,"mediaType":OCI_MANIFEST,
        "config":descriptor(&fixture.members[&config_path], OCI_CONFIG),"layers":[layer]});
    let description = fixture.index(manifest);
    let image = description["digest"].as_str().unwrap();
    let original = fixture.tar();
    assert_eq!(
        validate(&original, image, &fixture.layers).unwrap(),
        Compatibility::Unsupported("ManifestIdentity")
    );
    assert_eq!(
        validate_reader(Cursor::new(&original), image, &fixture.layers, true).unwrap(),
        Compatibility::VerifiedManifest(fixture.image.clone()),
    );
    let mut load = Vec::new();
    let normalized = write_load(
        Cursor::new(original),
        &mut load,
        image,
        &fixture.layers,
        true,
    )
    .unwrap();
    assert_eq!(normalized, sha256(&load));
    let mut archive = tar::Archive::new(Cursor::new(load));
    let mut entries = BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_str().unwrap().to_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        entries.insert(name, data);
    }
    assert_eq!(entries.len(), 5);
    for path in [
        "index.json",
        "oci-layout",
        &blob(image),
        &config_path,
        &layer_path,
    ] {
        assert_eq!(entries[path], fixture.members[path]);
    }
}

#[test]
fn gzip_layers_require_single_members_and_a_matching_bounded_diffid() {
    let raw = encode(&BTreeMap::from([("file".into(), vec![1; 4096])]));
    let diff = sha256(&raw);
    let compressed = gzip_bytes(&raw);
    assert_eq!(
        gzip::verify_layer(&compressed[..], raw.len() as u64, &diff).unwrap(),
        raw.len() as u64
    );
    assert_eq!(
        gzip::verify_layer(&compressed[..], 512, &diff)
            .unwrap_err()
            .0
            .code,
        "QuotaExceeded"
    );
    assert_eq!(
        gzip::verify_layer(&compressed[..], BYTE_LIMIT, &sha256(b"unrelated"))
            .unwrap_err()
            .0
            .code,
        "PreconditionFailed"
    );
    let concatenated = [compressed.as_slice(), compressed.as_slice()].concat();
    assert_eq!(
        gzip::verify_layer(&concatenated[..], BYTE_LIMIT, &diff)
            .unwrap_err()
            .0
            .code,
        "PreconditionFailed"
    );
}

struct Fixture {
    members: BTreeMap<String, Vec<u8>>,
    image: String,
    layers: Vec<String>,
    docker: Value,
}

impl Fixture {
    fn new(modern: bool) -> Self {
        let layer = encode(&BTreeMap::from([(
            "fixture".into(),
            b"real layer data".to_vec(),
        )]));
        let diff = sha256(&layer);
        let config = json_bytes(&json!({"os":"linux","architecture":"amd64",
            "rootfs":{"type":"layers","diff_ids":[diff]},"config":{"Entrypoint":["/fixture"]}}));
        let image = sha256(&config);
        let config_path = if modern {
            blob(&image)
        } else {
            format!("{}.json", &image[7..])
        };
        let layer_path = if modern {
            blob(&diff)
        } else {
            format!("{}/layer.tar", &diff[7..])
        };
        let docker = json!({"Config": config_path,"Layers":[layer_path],"RepoTags":null});
        let mut fixture = Self {
            members: BTreeMap::from([(config_path, config), (layer_path, layer)]),
            image,
            layers: vec![diff],
            docker,
        };
        if modern {
            let config = &fixture.members[fixture.docker["Config"].as_str().unwrap()];
            let layer = &fixture.members[fixture.docker["Layers"][0].as_str().unwrap()];
            let layer = descriptor(layer, OCI_LAYER);
            fixture.docker["LayerSources"] = json!({(fixture.layers[0].clone()):layer});
            let manifest = json!({"schemaVersion":2,"mediaType":OCI_MANIFEST,
                "config":descriptor(config, OCI_CONFIG),"layers":[layer]});
            fixture.index(manifest);
            let inert = b"legacy layer config bytes".to_vec();
            fixture.members.insert(blob(&sha256(&inert)), inert);
        }
        fixture
    }

    fn index(&mut self, manifest: Value) -> Value {
        let data = json_bytes(&manifest);
        let desc = descriptor(&data, OCI_MANIFEST);
        self.members
            .insert(blob(desc["digest"].as_str().unwrap()), data);
        self.members.insert(
            "oci-layout".into(),
            json_bytes(&json!({"imageLayoutVersion":"1.0.0"})),
        );
        self.members.insert(
            "index.json".into(),
            json_bytes(&json!({"schemaVersion":2,"mediaType":OCI_INDEX,"manifests":[desc]})),
        );
        desc
    }

    fn tar(&self) -> Vec<u8> {
        let mut members = self.members.clone();
        members
            .entry("manifest.json".into())
            .or_insert_with(|| json_bytes(&json!([self.docker])));
        encode(&members)
    }

    fn validate(&self) -> Result<Compatibility> {
        validate(&self.tar(), &self.image, &self.layers)
    }
}

#[test]
fn validates_tagless_legacy_tar_and_realistic_dual_manifest_archives() {
    assert_eq!(
        Fixture::new(false).validate().unwrap(),
        Compatibility::Verified
    );
    assert_eq!(
        Fixture::new(true).validate().unwrap(),
        Compatibility::Verified
    );
}

#[test]
fn normalized_load_has_only_one_tagless_manifest_and_unmodified_image_members() {
    let fixture = Fixture::new(true);
    let bytes = fixture.tar();
    let normalized = canonical_load(&bytes, &fixture.image, &fixture.layers).unwrap();
    assert_ne!(normalized, bytes);
    let config = blob(&fixture.image);
    let layer = blob(&fixture.layers[0]);
    let manifest = json_bytes(&json!([{"Config":config,"RepoTags":[],"Layers":[layer]}]));
    // The persisted checkpoint digest uses the original classic tar emitter.
    let mut original = Builder::new(Vec::new());
    for (path, bytes) in [
        (config.as_str(), &fixture.members[&config]),
        (layer.as_str(), &fixture.members[&layer]),
        ("manifest.json", &manifest),
    ] {
        let mut header = Header::new_ustar();
        header.set_mode(0o600);
        header.set_entry_type(EntryType::Regular);
        header.set_size(bytes.len() as u64);
        original.append_data(&mut header, path, &bytes[..]).unwrap();
    }
    assert_eq!(normalized, original.into_inner().unwrap());
    let legacy = Fixture::new(false);
    assert_eq!(
        canonical_load(&legacy.tar(), &legacy.image, &legacy.layers).unwrap(),
        normalized
    );
    let members = Members::read(Cursor::new(&normalized), normalized.len() as u64).unwrap();
    assert_eq!(members.files.len(), 3);
    assert!(members.directories.is_empty());
    assert_eq!(
        members.metadata("manifest.json").unwrap(),
        json!([{"Config":blob(&fixture.image),"RepoTags":[],"Layers":[blob(&fixture.layers[0])]}])
    );
    for digest in [&fixture.image, &fixture.layers[0]] {
        let path = blob(digest);
        let actual = members.file(&path).unwrap();
        assert_eq!(members.digest(actual).unwrap(), digest);
        let mut bytes = Vec::new();
        members
            .reader(actual)
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, fixture.members[&path]);
    }
    assert_eq!(
        validate(&normalized, &fixture.image, &fixture.layers).unwrap(),
        Compatibility::Verified,
    );
}

#[test]
fn seek_window_and_normalizer_reject_truncated_member_bytes() {
    let bytes = b"prefixpayloadnext";
    let members = Members {
        input: RefCell::new(Cursor::new(bytes)),
        files: BTreeMap::new(),
        directories: BTreeSet::new(),
    };
    let member = Member {
        offset: 6,
        size: 7,
        digest: OnceCell::new(),
    };
    let mut reader = members.reader(&member).unwrap();
    assert!(reader.seek(SeekFrom::Start(8)).is_err());
    assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 5);
    let mut remaining = Vec::new();
    reader.read_to_end(&mut remaining).unwrap();
    assert_eq!(remaining, b"ad");
    let mut output = Builder::new(Vec::new());
    assert_eq!(
        append_canonical(&mut output, "fixture", 8, &b"payload"[..])
            .unwrap_err()
            .0
            .code,
        "PreconditionFailed",
    );
}

#[test]
fn normalized_load_rejects_nested_extension_records() {
    let mut tar = Builder::new(Vec::new());
    let data = b"26 GNU.sparse.size=999999\n";
    let mut header = Header::new_ustar();
    header.set_entry_type(EntryType::XHeader);
    header.set_size(data.len() as u64);
    tar.append_data(&mut header, "PaxHeaders/sparse", data.as_slice())
        .unwrap();
    let layer = tar.into_inner().unwrap();
    assert_eq!(
        check_inner_layer(Cursor::new(&layer), layer.len() as u64)
            .unwrap_err()
            .0
            .code,
        "UnsupportedCapability",
    );
}

#[test]
fn repeated_layers_share_one_cached_member_digest() {
    let layer = encode(&BTreeMap::from([(
        "fixture".into(),
        vec![1; 5 * 1024 * 1024],
    )]));
    let diff = sha256(&layer);
    let layer_path = format!("{}/layer.tar", &diff[7..]);
    let diff_ids = vec![diff.clone(); 128];
    let config = json_bytes(&json!({"os":"linux","architecture":"amd64",
        "rootfs":{"type":"layers","diff_ids":diff_ids}}));
    let id = sha256(&config);
    let config_path = format!("{}.json", &id[7..]);
    let bytes = encode(&BTreeMap::from([
        (
            "manifest.json".into(),
            json_bytes(&json!([{"Config":config_path,"Layers":vec![layer_path.clone();128]}])),
        ),
        (config_path, config),
        (layer_path.clone(), layer),
    ]));
    let members = Members::read(Cursor::new(&bytes), bytes.len() as u64).unwrap();
    assert_eq!(
        canonical_load(&bytes, &id, &diff_ids).unwrap_err().0.code,
        "QuotaExceeded",
    );
    let member = members.file(&layer_path).unwrap();
    assert!(member.digest.get().is_none());
    assert_eq!(
        validate_members(&members, &id, &diff_ids, false).unwrap(),
        Compatibility::Verified
    );
    let cached = member.digest.get().unwrap();
    assert_eq!(cached, &diff);
    for _ in 0..128 {
        assert!(std::ptr::eq(
            cached.as_str(),
            members.digest(member).unwrap()
        ));
    }
}

#[test]
fn distinguishes_classic_config_from_manifest_and_compressed_identity() {
    let mut modern = Fixture::new(true);
    let index = parse(&modern.members["index.json"]).unwrap();
    modern.image = index["manifests"][0]["digest"].as_str().unwrap().into();
    assert_eq!(
        modern.validate().unwrap(),
        Compatibility::Unsupported("ManifestIdentity")
    );
    let mut legacy = Fixture::new(false);
    legacy.members.insert(
        legacy.docker["Layers"][0].as_str().unwrap().into(),
        b"\x1f\x8bcompressed".to_vec(),
    );
    assert_eq!(
        legacy.validate().unwrap(),
        Compatibility::Unsupported("CompressedLayer")
    );
}

#[test]
fn rejects_tagged_multi_image_and_conflicting_index_views() {
    let mut tagged = Fixture::new(false);
    tagged.docker["RepoTags"] = json!(["fixture:one"]);
    assert!(tagged.validate().is_err());
    let mut multiple = Fixture::new(false);
    multiple.members.insert(
        "manifest.json".into(),
        json_bytes(&json!([multiple.docker, multiple.docker])),
    );
    assert!(multiple.validate().is_err());
    let mut conflicting = Fixture::new(true);
    let mut other_config =
        conflicting.members[conflicting.docker["Config"].as_str().unwrap()].clone();
    other_config.push(b' ');
    let other = descriptor(&other_config, OCI_CONFIG);
    conflicting
        .members
        .insert(blob(other["digest"].as_str().unwrap()), other_config);
    let layer = descriptor(
        &conflicting.members[conflicting.docker["Layers"][0].as_str().unwrap()],
        OCI_LAYER,
    );
    let mut index = conflicting.index(json!({"schemaVersion":2,"config":other,"layers":[layer]}));
    assert!(conflicting.validate().is_err());
    index["annotations"] = json!({"io.containerd.image.name":"fixture:one"});
    conflicting.members.insert(
        "index.json".into(),
        json_bytes(&json!({"schemaVersion":2,"manifests":[index]})),
    );
    assert!(
        conflicting
            .validate()
            .unwrap_err()
            .0
            .message
            .contains("tag")
    );
}

#[test]
fn rejects_unrecognized_oci_fields_and_annotations() {
    let mut fixture = Fixture::new(true);
    let mut index = parse(&fixture.members["index.json"]).unwrap();
    index["extra"] = json!({});
    fixture
        .members
        .insert("index.json".into(), json_bytes(&index));
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .0
            .message
            .contains("unsupported fields")
    );
    let mut fixture = Fixture::new(true);
    let mut index = parse(&fixture.members["index.json"]).unwrap();
    index["manifests"][0]["annotations"] = json!({"example.vendor.label":"ordinary note"});
    fixture
        .members
        .insert("index.json".into(), json_bytes(&index));
    assert!(
        fixture
            .validate()
            .unwrap_err()
            .0
            .message
            .contains("annotation")
    );
}

#[test]
fn outer_header_formats_must_have_one_interpretation() {
    let archive = |mut header: Header| {
        header.set_path("fixture").unwrap();
        header.set_mode(0o644);
        header.set_size(1);
        header.set_cksum();
        let mut out = Builder::new(Vec::new());
        out.append(&header, &b"x"[..]).unwrap();
        out.into_inner().unwrap()
    };
    for header in [Header::new_old(), Header::new_ustar(), Header::new_gnu()] {
        let bytes = archive(header);
        assert!(Members::read(Cursor::new(&bytes), bytes.len() as u64).is_ok());
    }

    let mut other_version = Header::new_ustar();
    other_version.as_ustar_mut().unwrap().version = *b"01";
    let mut star = Header::new_ustar();
    star.as_mut_bytes()[508..].copy_from_slice(b"tar\0");
    let mut malformed_gnu = Header::new_gnu();
    malformed_gnu.as_gnu_mut().unwrap().atime = *b"not numeric\0";
    let mut extended_numeric = Header::new_gnu();
    extended_numeric.as_old_mut().uid[0] = 0x80;
    for header in [other_version, star, malformed_gnu, extended_numeric] {
        let bytes = archive(header);
        let error = validate(&bytes, "", &[]).unwrap_err();
        assert!(error.0.message.contains("ambiguous format"), "{error}");
    }
}

#[test]
fn rejects_invalid_paths_duplicate_entries_extra_archive_data_and_limits() {
    let invalid_paths = [
        "/absolute",
        "nested/../outside",
        "nested\\file",
        "path//duplicate",
    ];
    for path in invalid_paths {
        let mut header = Header::new_ustar();
        header.set_size(0);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        let mut out = Builder::new(Vec::new());
        out.append(&header, &[][..]).unwrap();
        assert!(
            validate(&out.into_inner().unwrap(), "", &[])
                .unwrap_err()
                .0
                .message
                .contains("path")
        );
    }
    let mut out = Builder::new(Vec::new());
    let mut link = Header::new_ustar();
    link.set_entry_type(EntryType::Symlink);
    link.set_size(0);
    link.set_link_name("fixture").unwrap();
    out.append_data(&mut link, "fixture-link", &[][..]).unwrap();
    assert!(
        validate(&out.into_inner().unwrap(), "", &[])
            .unwrap_err()
            .0
            .message
            .contains("entry type")
    );

    let mut out = Builder::new(Vec::new());
    for _ in 0..2 {
        let mut header = Header::new_ustar();
        header.set_size(1);
        out.append_data(&mut header, "duplicate", &b"x"[..])
            .unwrap();
    }
    assert!(
        validate(&out.into_inner().unwrap(), "", &[])
            .unwrap_err()
            .0
            .message
            .contains("duplicated")
    );
    let fixture = Fixture::new(false);
    let mut append = fixture.tar();
    append.extend_from_slice(&[1; BLOCK]);
    assert!(
        validate(&append, &fixture.image, &fixture.layers)
            .unwrap_err()
            .0
            .message
            .contains("end marker")
    );
    let oversized = vec![0; MAX_BYTES as usize + 1];
    assert_eq!(
        validate(&oversized, "", &[]).unwrap_err().0.code,
        "QuotaExceeded"
    );
}

#[test]
fn rejects_wrong_config_or_layer_digest() {
    let fixture = Fixture::new(false);
    let mismatch = validate(
        &fixture.tar(),
        &format!("sha256:{}", "f".repeat(64)),
        &fixture.layers,
    );
    assert!(
        mismatch
            .unwrap_err()
            .0
            .message
            .contains("configuration digest")
    );
    let mut fixture = Fixture::new(false);
    fixture.members.insert(
        fixture.docker["Layers"][0].as_str().unwrap().into(),
        b"changed layer".to_vec(),
    );
    assert!(fixture.validate().unwrap_err().0.message.contains("DiffID"));
}
