use psios_api::{canonical, contract, digest, parse, read_frame};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/v1/examples")
}
fn file(path: &Path) -> Value {
    parse(&fs::read(path).unwrap()).unwrap()
}
#[test]
fn published_contracts_and_negative_schema_examples() {
    let manifest = file(&root().join("manifest.json"));
    for example in manifest["examples"].as_array().unwrap() {
        let name = example["schema"]
            .as_str()
            .unwrap()
            .trim_end_matches(".schema.json");
        let path = root().join(example["path"].as_str().unwrap());
        let result = parse(&fs::read(&path).unwrap()).and_then(|v| contract(name, &v));
        if example["expected"] == "Valid" {
            result.unwrap_or_else(|e| panic!("{name} {e}"))
        } else if example["validation"] == "Schema" {
            assert!(result.is_err(), "{}", path.display())
        }
    }
}
#[test]
fn all_published_jcs_digests_match() {
    let vectors = file(&root().join("digest-vectors.json"));
    for v in vectors["vectors"].as_array().unwrap() {
        assert_eq!(
            String::from_utf8(canonical(&v["payload"])).unwrap(),
            v["canonical_utf8"]
        );
        assert_eq!(
            digest(
                v["contract"].as_str().unwrap(),
                v["digest_field"].as_str().unwrap(),
                &v["payload"]
            )
            .unwrap(),
            v["expected_digest"]
        );
    }
}
#[test]
fn jcs_uses_utf16_order_and_ecmascript_numbers() {
    let v = parse(br#"[333333333.33333329,1E30,4.50,2e-3,0.000000000000000000000000001,-0.0]"#)
        .unwrap();
    assert_eq!(
        canonical(&v),
        b"[333333333.3333333,1e+30,4.5,0.002,1e-27,0]"
    );
    assert_eq!(
        String::from_utf8(canonical(&json!({"\u{e000}": 1, "\u{10000}": 2}))).unwrap(),
        "{\"\u{10000}\":2,\"\u{e000}\":1}"
    );
}
#[test]
fn strict_input_and_bounded_framing() {
    for s in [
        r#"{"a":[{"x":1,"x":2}]}"#,
        r#""\uD800""#,
        "1e999",
        "9007199254740993",
        "{}[]",
    ] {
        assert!(parse(s.as_bytes()).is_err(), "{s}")
    }
    for f in [&[0, 0, 0, 0][..], &[255, 255, 255, 255], &[0, 0, 0, 4, 123]] {
        assert!(read_frame(&mut &f[..]).is_err())
    }
}
