use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Skill {
    name: String,
    version: String,
    instructions: String,
    tools: Vec<String>,
    license: String,
    source: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Model {
    name: String,
    revision: String,
    provider_principal_id: String,
    max_context_bytes: u32,
    max_output_bytes: u32,
    lease_seconds: u32,
}
pub(super) fn stable(kind: &str, parts: Value) -> String {
    let digest = sha256(&canonical(&parts));
    format!("{kind}-{}", &digest[7..])
}
fn publish(db: &Db<'_>, kind: &str, id: String, manifest: Value) -> Result<Value> {
    match db.get(&id) {
        Ok(d) => {
            require(
                d.spec == manifest,
                "IdempotencyConflict",
                "registered revision already binds different contents",
            )?;
            require(
                d.body["lifecycle"] == "Active",
                "PreconditionFailed",
                "revision is retired",
            )?;
            Ok(d.reference())
        }
        Err(e) if e.0.code == "NotFound" => Ok(db
            .create(
                kind,
                &id,
                policy::SYSTEM,
                manifest.clone(),
                json!({"manifest":manifest,"lifecycle":"Active"}),
            )?
            .reference()),
        Err(e) => Err(e),
    }
}
pub fn skill(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let v: Skill = decode(p)?;
    label(&v.name)?;
    label(&v.version)?;
    label(&v.source)?;
    label(&v.license)?;
    require(
        (1..=16384).contains(&v.instructions.len()) && (1..=16).contains(&v.tools.len()),
        "QuotaExceeded",
        "skill manifest limits",
    )?;
    let mut tools = std::collections::BTreeSet::new();
    let mut refs = serde_json::Map::new();
    for tool in &v.tools {
        require(
            policy::TOOLS.contains(&tool.as_str()) && tools.insert(tool),
            "UnsupportedCapability",
            "unregistered or repeated tool dependency",
        )?;
        refs.insert(
            tool.clone(),
            db.of(&format!("tool-{tool}"), "Capability")?.reference(),
        );
    }
    let mut manifest = p.clone();
    manifest["tool_refs"] = json!(refs);
    manifest["executable_dependencies"] = json!([]);
    manifest["source_trust"] = json!("OperatorRegistered");
    publish(
        db,
        "Skill",
        stable("skill", json!([v.name, v.version])),
        manifest,
    )
}
pub fn model(db: &Db<'_>, actor: &str, p: &Value) -> Result<Value> {
    policy::admin(actor)?;
    let m: Model = decode(p)?;
    label(&m.name)?;
    label(&m.revision)?;
    policy::active(db, &m.provider_principal_id)?;
    require(
        (1024..=131072).contains(&m.max_context_bytes)
            && (128..=65536).contains(&m.max_output_bytes)
            && (1..=300).contains(&m.lease_seconds),
        "InvalidArgument",
        "invalid local provider limits",
    )?;
    let mut manifest = p.clone();
    manifest["interface"] = json!("psios.local.model-turn.v1");
    manifest["transport"] = json!("authenticated-local-peer");
    manifest["revision_assurance"] = json!("ProviderReported");
    manifest["modalities"] = json!(["structured-text"]);
    publish(
        db,
        "Model",
        stable(
            "model",
            json!([m.provider_principal_id, m.name, m.revision]),
        ),
        manifest,
    )
}
pub fn retire(db: &Db<'_>, actor: &str, p: &Value, kind: &str) -> Result<Value> {
    policy::admin(actor)?;
    let c: Conditional = decode(p)?;
    let d = db.of(&c.id, kind)?;
    d.cas(&c.expected_resource_version)?;
    let mut b = d.body.clone();
    b["lifecycle"] = json!("Retired");
    Ok(db.status(&d, b)?.reference())
}
pub(super) fn pinned(db: &Db<'_>, rf: &Value, kind: &str) -> Result<Doc> {
    let d = db.resolve(rf)?;
    require(
        d.kind == kind && db.get(&d.id)?.reference() == *rf && d.body["lifecycle"] == "Active",
        "PreconditionFailed",
        "execution dependency changed or retired",
    )?;
    Ok(d)
}
