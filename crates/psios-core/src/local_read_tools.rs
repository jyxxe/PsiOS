use crate::{Result, db::Db, decode, host::Action, label, policy, require};
use psios_api::{LocalReadTool, LocalReadToolsRequest};
use serde_json::{Value, json};

pub(crate) fn discover(db: &Db<'_>, actor: &str, params: &Value) -> Result<Value> {
    let request: LocalReadToolsRequest = decode(params)?;
    let principal = db.of(actor, "Principal")?;
    require(
        principal.spec["principal_type"] == "Agent" && principal.body["lifecycle"] == "Active",
        "PermissionDenied",
        "local read tool discovery requires an active Agent principal",
    )?;
    label(&request.delegation_id)?;
    require(
        matches!(request.tools.as_slice(), [_] | [_, _])
            && (request.tools.len() == 1 || request.tools[0] != request.tools[1]),
        "InvalidArgument",
        "one or two distinct local read tools required",
    )?;
    let delegation = |action| policy::delegation(db, actor, &request.delegation_id, action);
    for action in ["task.submit", "task.read"] {
        match delegation(action) {
            Err(error) if error.0.code == "PermissionDenied" => return Ok(json!({"tools":[]})),
            other => {
                other?;
            }
        }
    }
    let mut visible = Vec::<LocalReadTool>::new();
    for tool in request.tools {
        let name = tool.name();
        let checked = (|| -> Result<()> {
            let delegation = delegation(name)?;
            let registered = db.of(&format!("tool-{name}"), "Capability")?;
            require(
                policy::readonly(name)
                    && !policy::approval(name)
                    && registered.body["capability_id"] == name
                    && registered.spec["effect_class"] == "ReadOnly"
                    && registered.spec["requires_approval"] == false,
                "RecoveryRequired",
                "local read tool disagrees with its capability registration",
            )?;
            let action = Action::parse(name, &json!({}))?;
            let target = action.target(db, actor)?;
            action.audience(db, actor, &delegation.reference(), &target)?;
            Ok(())
        })();
        match checked {
            Ok(()) => visible.push(tool),
            Err(error) if error.0.code == "PermissionDenied" => {}
            Err(error) => return Err(error),
        }
    }
    Ok(json!({"tools":visible}))
}
