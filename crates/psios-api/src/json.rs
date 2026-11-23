use crate::{ApiError, ApiResult, invalid};
use jsonschema::{Draft, JSONSchema};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::LazyLock,
};

const RAW: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/contracts.rs"));
// The design schema expresses decimal syntax; the authority also enforces the u64 ceiling.
fn counter_formats(schema: &mut Value) {
    match schema {
        Value::Object(o) => {
            if matches!(
                o.get("pattern").and_then(Value::as_str),
                Some("^(0|[1-9][0-9]{0,19})$" | "^[1-9][0-9]{0,19}$")
            ) {
                o.insert("format".into(), Value::String("psios-u64".into()));
            }
            for child in o.values_mut() {
                counter_formats(child)
            }
        }
        Value::Array(a) => a.iter_mut().for_each(counter_formats),
        _ => {}
    }
}
static CONTRACTS: LazyLock<HashMap<&str, JSONSchema>> = LazyLock::new(|| {
    let mut docs: Vec<_> = RAW
        .iter()
        .map(|(n, s)| (*n, serde_json::from_str::<Value>(s).unwrap()))
        .collect();
    for (_, doc) in &mut docs {
        counter_formats(doc)
    }
    let mut opts = JSONSchema::options();
    opts.with_format("psios-u64", |s| s.parse::<u64>().is_ok());
    opts.with_draft(Draft::Draft202012)
        .should_validate_formats(true);
    for (_, doc) in &docs {
        opts.with_document(doc["$id"].as_str().unwrap().into(), doc.clone());
    }
    docs.iter()
        .map(|(name, doc)| (*name, opts.compile(doc).expect("embedded v1 contract")))
        .collect()
});

pub fn contract(name: &str, value: &Value) -> ApiResult<()> {
    let schema = CONTRACTS
        .get(name)
        .ok_or_else(|| ApiError::new("UnsupportedCapability", "unknown contract"))?;
    if let Err(mut issues) = schema.validate(value) {
        // Error displays can contain the complete input, including credentials.
        let path = issues
            .next()
            .map(|e| e.instance_path.to_string())
            .unwrap_or_default();
        return Err(invalid(format!(
            "invalid {name} at /{}",
            path.trim_start_matches('/')
        )));
    }
    let digest_field = match name {
        "task-submit" => Some("request_digest"),
        "operation-plan" => Some("plan_digest"),
        "chat-send" => Some("content_digest"),
        "workflow-definition" => Some("definition_digest"),
        "recovery-set-manifest" => Some("manifest_digest"),
        _ => None,
    };
    if let Some(field) = digest_field
        && value[field] != digest(name, field, value)?
    {
        return Err(invalid("contract digest mismatch"));
    }
    if name == "resource-envelope" {
        let count = |v: &Value| v.as_str().and_then(|s| s.parse::<u64>().ok());
        if count(&value["status"]["observed_generation"])
            > count(&value["metadata"]["spec_generation"])
        {
            return Err(invalid("observed generation is ahead of specification"));
        }
    }
    if let Some(conditions) = value
        .get("conditions")
        .or_else(|| value.pointer("/status/conditions"))
        .and_then(Value::as_array)
    {
        let mut seen = HashSet::new();
        if conditions.iter().any(|c| !seen.insert(c["type"].as_str())) {
            return Err(invalid("duplicate condition type"));
        }
    }
    if name == "task-lookup"
        && value["status"] == "Found"
        && ["origin_ref", "idempotency_key", "request_digest"]
            .iter()
            .any(|k| value[*k] != value["accepted"][*k])
    {
        return Err(invalid("submission receipt does not match lookup"));
    }
    if name == "revocation-result" {
        for t in value["targets"].as_array().unwrap() {
            if (t["enforcement_status"] == "Enforced"
                && t["evidence_refs"].as_array().unwrap().is_empty())
                || (value["status"] == "Complete"
                    && t["required"] == true
                    && t["enforcement_status"] != "Enforced")
            {
                return Err(invalid("revocation assertion lacks enforcement evidence"));
            }
        }
    }
    if name == "execution-permit" {
        let time = |f: &str| {
            chrono::DateTime::parse_from_rfc3339(value[f].as_str().unwrap())
                .map_err(|_| invalid("invalid permit time"))
        };
        if time("not_after")? <= time("issued_at")? {
            return Err(invalid("empty execution permit validity interval"));
        }
    }
    Ok(())
}

struct Unique(Value);
struct JsonVisitor;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(JsonVisitor).map(Unique)
    }
}
impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("I-JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        if v.unsigned_abs() > 9_007_199_254_740_991 {
            return Err(E::custom("use decimal string for wide integer"));
        }
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        if v > 9_007_199_254_740_991 {
            return Err(E::custom("use decimal string for wide integer"));
        }
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Value, A::Error> {
        let mut a = Vec::new();
        while let Some(Unique(v)) = s.next_element()? {
            a.push(v)
        }
        Ok(Value::Array(a))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Value, A::Error> {
        let mut a = Map::new();
        while let Some(k) = m.next_key::<String>()? {
            if a.contains_key(&k) {
                return Err(de::Error::custom("duplicate object key"));
            }
            a.insert(k, m.next_value::<Unique>()?.0);
        }
        Ok(Value::Object(a))
    }
}
pub fn parse(data: &[u8]) -> ApiResult<Value> {
    serde_json::from_slice::<Unique>(data)
        .map(|u| u.0)
        .map_err(|_| invalid("malformed or ambiguous JSON"))
}

pub fn canonical(value: &Value) -> Vec<u8> {
    fn write(v: &Value, b: &mut Vec<u8>) {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<_> = o.keys().collect();
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                b.push(b'{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        b.push(b',')
                    }
                    serde_json::to_writer(&mut *b, k).unwrap();
                    b.push(b':');
                    write(&o[*k], b);
                }
                b.push(b'}');
            }
            Value::Array(a) => {
                b.push(b'[');
                for (i, item) in a.iter().enumerate() {
                    if i > 0 {
                        b.push(b',')
                    }
                    write(item, b)
                }
                b.push(b']');
            }
            Value::Number(n) => {
                b.extend(
                    ryu_js::Buffer::new()
                        .format_finite(n.as_f64().expect("finite JSON number"))
                        .as_bytes(),
                );
            }
            _ => serde_json::to_writer(b, v).unwrap(),
        }
    }
    let mut out = Vec::new();
    write(value, &mut out);
    out
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub fn digest(name: &str, field: &str, value: &Value) -> ApiResult<String> {
    let mut payload = value.clone();
    payload
        .as_object_mut()
        .ok_or_else(|| invalid("contract must be an object"))?
        .remove(field);
    let mut bytes = format!("psios/{name}/1\n").into_bytes();
    bytes.extend(canonical(&payload));
    Ok(sha256(&bytes))
}
