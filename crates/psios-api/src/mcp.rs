//! The bounded set of no-argument diagnostics available through local adapters.
use crate::ResourceRef;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum LocalReadTool {
    #[serde(rename = "system.status")]
    SystemStatus,
    #[serde(rename = "node.pressure")]
    NodePressure,
    #[serde(rename = "node.inventory")]
    NodeInventory,
    #[serde(rename = "node.devices")]
    NodeDevices,
}

impl LocalReadTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::SystemStatus => "system.status",
            Self::NodePressure => "node.pressure",
            Self::NodeInventory => "node.inventory",
            Self::NodeDevices => "node.devices",
        }
    }

    pub fn requires_node_scope(self) -> bool {
        matches!(self, Self::NodeInventory | Self::NodeDevices)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalReadToolsRequest {
    pub delegation_id: String,
    pub tools: Vec<LocalReadTool>,
}

/// An adapter's fixed tool and, where applicable, operator-configured target.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "tool", deny_unknown_fields)]
pub enum LocalReadBinding {
    #[serde(rename = "system.status")]
    SystemStatus {},
    #[serde(rename = "node.pressure")]
    NodePressure {},
    #[serde(rename = "node.inventory")]
    NodeInventory {},
    #[serde(rename = "node.devices")]
    NodeDevices {},
    #[serde(rename = "volume.usage")]
    VolumeUsage { target_id: String },
    #[serde(rename = "service.inspect")]
    ServiceInspect { target_id: String },
}

impl From<LocalReadTool> for LocalReadBinding {
    fn from(tool: LocalReadTool) -> Self {
        match tool {
            LocalReadTool::SystemStatus => Self::SystemStatus {},
            LocalReadTool::NodePressure => Self::NodePressure {},
            LocalReadTool::NodeInventory => Self::NodeInventory {},
            LocalReadTool::NodeDevices => Self::NodeDevices {},
        }
    }
}

impl LocalReadBinding {
    pub fn name(&self) -> &'static str {
        match self {
            Self::SystemStatus {} => LocalReadTool::SystemStatus.name(),
            Self::NodePressure {} => LocalReadTool::NodePressure.name(),
            Self::NodeInventory {} => LocalReadTool::NodeInventory.name(),
            Self::NodeDevices {} => LocalReadTool::NodeDevices.name(),
            Self::VolumeUsage { .. } => "volume.usage",
            Self::ServiceInspect { .. } => "service.inspect",
        }
    }

    pub fn unbound_tool(&self) -> Option<LocalReadTool> {
        match self {
            Self::SystemStatus {} => Some(LocalReadTool::SystemStatus),
            Self::NodePressure {} => Some(LocalReadTool::NodePressure),
            Self::NodeInventory {} => Some(LocalReadTool::NodeInventory),
            Self::NodeDevices {} => Some(LocalReadTool::NodeDevices),
            Self::VolumeUsage { .. } | Self::ServiceInspect { .. } => None,
        }
    }

    /// Resource ID and kind selected by deployment configuration.
    pub fn target(&self) -> Option<(&str, &'static str)> {
        match self {
            Self::VolumeUsage { target_id } => Some((target_id, "Volume")),
            Self::ServiceInspect { target_id } => Some((target_id, "HostService")),
            Self::SystemStatus {}
            | Self::NodePressure {}
            | Self::NodeInventory {}
            | Self::NodeDevices {} => None,
        }
    }

    /// Build the native Task input from an authority-resolved revision.
    pub fn task_input(&self, reference: Option<&ResourceRef>) -> Option<Value> {
        let Some((id, _)) = self.target() else {
            return Some(json!({}));
        };
        let reference = reference.filter(|reference| reference.resource_id == id)?;
        let target = json!({"id":id,"expected_resource_version":reference.version});
        match self {
            Self::VolumeUsage { .. } => Some(json!({"target":target})),
            Self::ServiceInspect { .. } => Some(target),
            Self::SystemStatus {}
            | Self::NodePressure {}
            | Self::NodeInventory {}
            | Self::NodeDevices {} => unreachable!(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveLocalReadToolsRequest {
    pub delegation_id: String,
    pub bindings: Vec<LocalReadBinding>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedLocalReadTool {
    pub tool: String,
    pub target_ref: ResourceRef,
}
