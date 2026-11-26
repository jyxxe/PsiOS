//! A restore publication keeps its historical Backup wire shape or binds an exact Clone source.
use super::super::cloning;
use crate::{
    Result,
    broker::dependencies,
    db::{self, Doc},
    decode, err, require,
};
use rusqlite::Connection;
use serde_json::{Value, json};

pub(super) enum Source {
    Backup,
    Clone(Value),
    Create(Value),
}

impl Source {
    pub(super) fn load(conn: &Connection, op: &Doc) -> Result<(Self, Doc)> {
        let plan = db::resolve(conn, &op.body["plan_ref"])?;
        let effect = &plan.body["allowed_effects"][0];
        let target = db::resolve(conn, &effect["resource_ref"])?;
        require(
            op.kind == "Operation" && plan.kind == "OperationPlan" && plan.owner == op.owner,
            "RecoveryRequired",
            "volume publication does not have its original plan",
        )?;
        match effect["action"].as_str() {
            Some("backup.restore") if target.kind == "Backup" => Ok((Self::Backup, target)),
            Some(super::CREATE) => {
                let binding = super::creation::binding(conn, op, &target)?;
                Ok((Self::Create(binding), target))
            }
            Some(cloning::TOOL) if target.kind == "Volume" => {
                let task_id = op.spec["task_id"]
                    .as_str()
                    .ok_or_else(|| err("RecoveryRequired", "clone has no original task"))?;
                let task = super::verification::needed(conn, task_id)?;
                let input = db::resolve(conn, &task.spec["input_ref"])?;
                let input: cloning::Input = decode(dependencies::tool_input(&input)?)?;
                input.validate()?;
                let pool_ref = plan.body["preconditions"]
                    .as_array()
                    .and_then(|conditions| {
                        conditions
                            .iter()
                            .map(|item| &item["resource_ref"])
                            .find(|reference| reference["resource_id"] == "local")
                    })
                    .ok_or_else(|| err("RecoveryRequired", "clone plan has no target pool"))?;
                let pool = db::resolve(conn, pool_ref)?;
                require(
                    task.kind == "Task"
                        && task.owner == op.owner
                        && task.spec["task_type"] == cloning::TOOL
                        && input.target.id == target.id
                        && input.target.expected_resource_version == target.version()
                        && target.body["lifecycle"] == "Available"
                        && target.body["data_incarnation"].is_string()
                        && pool.kind == "StoragePool"
                        && pool.id == "local"
                        && pool.body["allocation_enabled"] == true,
                    "RecoveryRequired",
                    "clone source and pool do not match the original scoped task",
                )?;
                let bound = json!({
                    "volume_ref":target.reference(),"data_incarnation":target.body["data_incarnation"],
                    "pool_ref":pool_ref,"name":input.name,
                });
                Ok((Self::Clone(bound), target))
            }
            _ => Err(err(
                "RecoveryRequired",
                "volume publication has an unsupported source or effect",
            )),
        }
    }

    pub(super) fn tool(&self) -> &'static str {
        match self {
            Self::Backup => "backup.restore",
            Self::Clone(_) => cloning::TOOL,
            Self::Create(_) => super::CREATE,
        }
    }

    pub(super) fn evidence(&self, body: &mut Value, target: &Doc, checkpoint: bool) {
        match self {
            Self::Backup => {
                body["backup_ref"] = target.reference();
                if checkpoint {
                    body["manifest_digest"] = target.body["manifest_digest"].clone();
                }
            }
            Self::Clone(binding) => body["clone_source"] = binding.clone(),
            Self::Create(binding) => body["create_source"] = binding.clone(),
        }
    }

    pub(super) fn matches(&self, record: &Doc, target: &Doc, checkpoint: bool) -> bool {
        match self {
            Self::Backup => {
                record.body.get("clone_source").is_none()
                    && record.body.get("create_source").is_none()
                    && record.body["backup_ref"] == target.reference()
                    && (!checkpoint
                        || record.body["manifest_digest"] == target.body["manifest_digest"])
            }
            Self::Clone(binding) => {
                record.body.get("backup_ref").is_none()
                    && record.body.get("manifest_digest").is_none()
                    && record.body.get("create_source").is_none()
                    && record.body["clone_source"] == *binding
            }
            Self::Create(binding) => {
                record.body.get("backup_ref").is_none()
                    && record.body.get("manifest_digest").is_none()
                    && record.body.get("clone_source").is_none()
                    && record.body["create_source"] == *binding
            }
        }
    }

    pub(super) fn catalog(&self, target: &Doc, proof: &Value) -> Value {
        let mut spec = json!({"storage_restore_phase":"AwaitingActivation","restore_proof":proof});
        match self {
            Self::Backup => spec["restored_from"] = target.reference(),
            Self::Clone(binding) => {
                spec["name"] = binding["name"].clone();
                spec["cloned_from"] = target.reference();
            }
            Self::Create(binding) => return json!({"name":binding["name"]}),
        }
        spec
    }

    pub(super) fn check_receipt(&self, operation: &Doc, receipt: &Value) -> Result<()> {
        if matches!(self, Self::Clone(_) | Self::Create(_)) {
            let number = |name: &str| {
                receipt[name]
                    .as_str()
                    .and_then(|value| value.parse::<u64>().ok())
            };
            require(
                receipt["id"] == format!("volume-{}", operation.id)
                    && receipt["incarnation"] == format!("data-{}", operation.id)
                    && number("device").is_some()
                    && number("inode").is_some_and(|value| value != 0)
                    && number("capacity").is_some_and(|value| value != 0),
                "RecoveryRequired",
                "volume publication receipt does not bind a distinct local target",
            )?;
            if matches!(self, Self::Clone(_)) {
                super::restore_proof::check_clone_proof(&receipt["restore_proof"])?;
            } else {
                require(
                    receipt.as_object().is_some_and(|body| body.len() == 6)
                        && receipt.get("restore_proof").is_some_and(Value::is_null),
                    "RecoveryRequired",
                    "created volume receipt must describe an empty tree",
                )?;
            }
        }
        Ok(())
    }
}

pub(super) fn same_binding(first: &Doc, second: &Doc) -> bool {
    first.body.get("clone_source") == second.body.get("clone_source")
        && first.body.get("backup_ref") == second.body.get("backup_ref")
        && first.body.get("create_source") == second.body.get("create_source")
}
