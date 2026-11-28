use crate::{
    Result,
    db::{Db, Doc},
    decode, err,
    host::Action,
    label, policy, require,
};
use psios_api::{
    LocalReadBinding, LocalReadTool, LocalReadToolsRequest, MAX_LOCAL_READ_BINDINGS,
    ResolveLocalReadToolsRequest, ResolvedLocalReadTool, ResourceRef,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;

pub(crate) const NODE_INVENTORY_FORMAT: u32 = crate::storage::xattrs::FORMAT + 1;
pub(crate) const VOLUME_FILES_FORMAT: u32 = crate::apps::mount_observation::READ_ONLY_FORMAT + 1;
pub(crate) const VOLUME_FILE_CHUNK_FORMAT: u32 = VOLUME_FILES_FORMAT + 1;
pub(crate) const APPLICATION_INSPECT_FORMAT: u32 = crate::apps::restart::USAGE_FORMAT + 1;
pub(crate) const VOLUME_DIRECTORY_REVISION_FORMAT: u32 =
    crate::policy::secrets::receipts::FORMAT + 1;

pub(crate) fn agent(db: &Db<'_>, actor: &str) -> Result<()> {
    let principal = db.of(actor, "Principal")?;
    require(
        principal.spec["principal_type"] == "Agent" && principal.body["lifecycle"] == "Active",
        "PermissionDenied",
        "local read tool discovery requires an active Agent principal",
    )
}

pub(crate) fn task_authority(db: &Db<'_>, actor: &str, delegation_id: &str) -> Result<bool> {
    for action in ["task.submit", "task.read"] {
        match policy::delegation(db, actor, delegation_id, action) {
            Err(error) if error.0.code == "PermissionDenied" => return Ok(false),
            other => {
                other?;
            }
        }
    }
    Ok(true)
}

fn read_only(db: &Db<'_>, name: &str) -> Result<()> {
    let registered = db.of(&format!("tool-{name}"), "Capability")?;
    require(
        policy::readonly(name)
            && !policy::approval(name)
            && registered.body["capability_id"] == name
            && registered.spec["effect_class"] == "ReadOnly"
            && registered.spec["requires_approval"] == false,
        "RecoveryRequired",
        "local read tool disagrees with its capability registration",
    )
}

fn available(db: &Db<'_>, actor: &str, delegation_id: &str, tool: LocalReadTool) -> Result<bool> {
    let name = tool.name();
    let checked = (|| -> Result<()> {
        let delegation = policy::delegation(db, actor, delegation_id, name)?;
        read_only(db, name)?;
        let action = Action::parse(name, &json!({}))?;
        let target = action.target(db, actor)?;
        require(
            !tool.requires_node_scope() || explicitly_scoped(db, &delegation, &target.id)?,
            "PermissionDenied",
            "this diagnostic requires an explicit Node scope",
        )?;
        action.audience(db, actor, &delegation.reference(), &target)?;
        Ok(())
    })();
    match checked {
        Ok(()) => Ok(true),
        Err(error) if error.0.code == "PermissionDenied" => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckLocalReadTool {
    delegation_id: String,
    tool: LocalReadTool,
}

pub(crate) fn check(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: CheckLocalReadTool = decode(params)?;
    label(&request.delegation_id)?;
    let allowed = task_authority(db, actor, &request.delegation_id)?
        && available(db, actor, &request.delegation_id, request.tool)?;
    Ok(json!({"allowed":allowed}))
}

pub(crate) fn discover(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: LocalReadToolsRequest = decode(params)?;
    agent(db, actor)?;
    label(&request.delegation_id)?;
    require(
        matches!(request.tools.as_slice(), [_] | [_, _])
            && (request.tools.len() == 1 || request.tools[0] != request.tools[1]),
        "InvalidArgument",
        "one or two distinct local read tools required",
    )?;
    if !task_authority(db, actor, &request.delegation_id)? {
        return Ok(json!({"tools":[]}));
    }
    let mut visible = Vec::<LocalReadTool>::new();
    for tool in request.tools {
        if available(db, actor, &request.delegation_id, tool)? {
            visible.push(tool);
        }
    }
    Ok(json!({"tools":visible}))
}

pub(crate) fn explicitly_scoped(db: &Db<'_>, delegation: &Doc, target: &str) -> Result<bool> {
    let grant_id = delegation.body["grant_id"]
        .as_str()
        .ok_or_else(|| err("RecoveryRequired", "delegation has no root grant"))?;
    let grant = db.of(grant_id, "Grant")?;
    Ok([&grant.spec, &delegation.body].into_iter().any(|scope| {
        scope["tool_targets"]
            .as_array()
            .is_some_and(|targets| targets.iter().any(|id| id == target))
    }))
}

pub(crate) fn resolve_binding(
    db: &Db<'_>,
    actor: &str,
    delegation: &Doc,
    binding: &LocalReadBinding,
) -> Result<Option<ResolvedLocalReadTool>> {
    let name = binding.name();
    read_only(db, name)?;
    let exact: Option<ResourceRef> = if let Some((id, kind)) = binding.target() {
        if !explicitly_scoped(db, delegation, id)? {
            return Ok(None);
        }
        policy::tool_scope(db, &delegation.reference(), id)?;
        let Some(target) = db.find(id)? else {
            return Ok(None);
        };
        let lifecycle = match kind {
            "Volume" => "Available",
            "Application" => "Running",
            _ => "Registered",
        };
        if target.kind != kind || target.body["lifecycle"] != lifecycle {
            return Ok(None);
        }
        if matches!(
            binding,
            LocalReadBinding::VolumeReadChunk { query: None, .. }
        ) {
            policy::owned(actor, &target)?;
            return Ok(Some(ResolvedLocalReadTool {
                tool: name.to_owned(),
                target_ref: decode(&target.reference())?,
            }));
        }
        Some(decode(&target.reference())?)
    } else {
        None
    };
    require(
        match binding {
            LocalReadBinding::VolumeFiles { query, .. } => query.within_limits(),
            LocalReadBinding::VolumeReadChunk {
                query: Some(query), ..
            } => query.within_limits(),
            _ => true,
        },
        "InvalidArgument",
        "volume read parameters are outside the local profile",
    )?;
    let input = binding
        .task_input(exact.as_ref())
        .ok_or_else(|| err("RecoveryRequired", "local read tool target is inconsistent"))?;
    let action = Action::parse(name, &input)?;
    let target = action.target(db, actor)?;
    if binding
        .unbound_tool()
        .is_some_and(LocalReadTool::requires_node_scope)
        && !explicitly_scoped(db, delegation, &target.id)?
    {
        return Ok(None);
    }
    let audience = action.audience(db, actor, &delegation.reference(), &target)?;
    if matches!(binding, LocalReadBinding::ApplicationInspect { .. }) {
        for reference in audience.iter().skip(1) {
            let reference: ResourceRef = decode(reference)?;
            if !explicitly_scoped(db, delegation, &reference.resource_id)? {
                return Ok(None);
            }
        }
    }
    let target_ref = decode(&target.reference())?;
    require(
        exact.is_none_or(|reference| reference == target_ref),
        "RecoveryRequired",
        "local read tool resolved a different target",
    )?;
    Ok(Some(ResolvedLocalReadTool {
        tool: name.to_owned(),
        target_ref,
    }))
}

pub(crate) fn resolve(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: ResolveLocalReadToolsRequest = decode(params)?;
    agent(db, actor)?;
    label(&request.delegation_id)?;
    let mut tools = HashSet::new();
    require(
        (1..=MAX_LOCAL_READ_BINDINGS).contains(&request.bindings.len())
            && request
                .bindings
                .iter()
                .all(|binding| tools.insert(binding.name())),
        "InvalidArgument",
        "one to seven distinct local read tools required",
    )?;
    for binding in &request.bindings {
        if let Some((id, _)) = binding.target() {
            label(id)?;
        }
    }
    if !task_authority(db, actor, &request.delegation_id)? {
        return Ok(json!({"tools":[]}));
    }
    let mut visible = Vec::new();
    for binding in request.bindings {
        let resolution = policy::delegation(db, actor, &request.delegation_id, binding.name())
            .and_then(|delegation| resolve_binding(db, actor, &delegation, &binding));
        match resolution {
            Ok(Some(tool)) => visible.push(tool),
            Ok(None) => {}
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(json!({"tools":visible}))
}
