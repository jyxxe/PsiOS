//! The bounded set of diagnostics available through local adapters.
use crate::{BlockInventoryQuery, ResourceRef};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_LOCAL_READ_BINDINGS: usize = 7;
pub const MAX_VOLUME_FILES_PAGE: usize = 32;
pub const VOLUME_FILES_RESULT_BYTES: usize = 12 * 1024;
pub const MAX_VOLUME_FILE_CHUNK: usize = 6 * 1024;
pub const MAX_VOLUME_FILE_OFFSET: u64 = (1_u64 << 53) - 1;

fn valid_volume_version(version: &str) -> bool {
    (1..=128).contains(&version.len()) && !version.chars().any(char::is_control)
}

pub fn valid_volume_entry_revision(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// A page of metadata from a volume selected by the adapter operator.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFilesQuery {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl VolumeFilesQuery {
    pub fn within_limits(&self) -> bool {
        self.path.len() <= 2048
            && self
                .limit
                .is_none_or(|limit| (1..=MAX_VOLUME_FILES_PAGE).contains(&limit))
            && self.directory_revision.as_deref().is_none_or(|revision| {
                self.volume_version.is_some() && valid_volume_entry_revision(revision)
            })
            && self.after.as_ref().is_none_or(|after| {
                self.volume_version.is_some()
                    && (1..=255).contains(&after.len())
                    && !matches!(after.as_str(), "." | "..")
                    && !after.contains(['/', '\0'])
            })
            && self
                .volume_version
                .as_deref()
                .is_none_or(valid_volume_version)
    }
}

/// A binary file chunk from a volume selected by the adapter operator.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeFileChunkQuery {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_revision: Option<String>,
}

impl VolumeFileChunkQuery {
    pub fn valid_file_revision(value: &str) -> bool {
        valid_volume_entry_revision(value)
    }

    pub fn within_limits(&self) -> bool {
        (1..=2048).contains(&self.path.len())
            && self.offset <= MAX_VOLUME_FILE_OFFSET
            && self
                .limit
                .is_none_or(|limit| (1..=MAX_VOLUME_FILE_CHUNK).contains(&limit))
            && self
                .volume_version
                .as_deref()
                .is_none_or(valid_volume_version)
            && (self.offset == 0 || self.file_revision.is_some())
            && self.file_revision.as_deref().is_none_or(|revision| {
                self.volume_version.is_some() && Self::valid_file_revision(revision)
            })
    }
}

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
    #[serde(rename = "node.resolver")]
    NodeResolver,
}

impl LocalReadTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::SystemStatus => "system.status",
            Self::NodePressure => "node.pressure",
            Self::NodeInventory => "node.inventory",
            Self::NodeDevices => "node.devices",
            Self::NodeResolver => "node.resolver",
        }
    }

    pub fn requires_node_scope(self) -> bool {
        matches!(
            self,
            Self::NodeInventory | Self::NodeDevices | Self::NodeResolver
        )
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
    NodeDevices(BlockInventoryQuery),
    #[serde(rename = "node.resolver")]
    NodeResolver {},
    #[serde(rename = "volume.usage")]
    VolumeUsage { target_id: String },
    #[serde(rename = "volume.files")]
    VolumeFiles {
        target_id: String,
        #[serde(default)]
        query: VolumeFilesQuery,
    },
    #[serde(rename = "volume.read_chunk")]
    VolumeReadChunk {
        target_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        query: Option<VolumeFileChunkQuery>,
    },
    #[serde(rename = "service.inspect")]
    ServiceInspect { target_id: String },
    #[serde(rename = "application.inspect_instance")]
    ApplicationInspect {
        target_id: String,
        #[serde(default)]
        include_usage: bool,
    },
}

impl From<LocalReadTool> for LocalReadBinding {
    fn from(tool: LocalReadTool) -> Self {
        match tool {
            LocalReadTool::SystemStatus => Self::SystemStatus {},
            LocalReadTool::NodePressure => Self::NodePressure {},
            LocalReadTool::NodeInventory => Self::NodeInventory {},
            LocalReadTool::NodeDevices => Self::NodeDevices(BlockInventoryQuery::default()),
            LocalReadTool::NodeResolver => Self::NodeResolver {},
        }
    }
}

impl LocalReadBinding {
    pub fn name(&self) -> &'static str {
        match self {
            Self::SystemStatus {} => LocalReadTool::SystemStatus.name(),
            Self::NodePressure {} => LocalReadTool::NodePressure.name(),
            Self::NodeInventory {} => LocalReadTool::NodeInventory.name(),
            Self::NodeDevices(_) => LocalReadTool::NodeDevices.name(),
            Self::NodeResolver {} => LocalReadTool::NodeResolver.name(),
            Self::VolumeUsage { .. } => "volume.usage",
            Self::VolumeFiles { .. } => "volume.files",
            Self::VolumeReadChunk { .. } => "volume.read_chunk",
            Self::ServiceInspect { .. } => "service.inspect",
            Self::ApplicationInspect { .. } => "application.inspect_instance",
        }
    }

    pub fn unbound_tool(&self) -> Option<LocalReadTool> {
        match self {
            Self::SystemStatus {} => Some(LocalReadTool::SystemStatus),
            Self::NodePressure {} => Some(LocalReadTool::NodePressure),
            Self::NodeInventory {} => Some(LocalReadTool::NodeInventory),
            Self::NodeDevices(_) => Some(LocalReadTool::NodeDevices),
            Self::NodeResolver {} => Some(LocalReadTool::NodeResolver),
            Self::VolumeUsage { .. }
            | Self::VolumeFiles { .. }
            | Self::VolumeReadChunk { .. }
            | Self::ServiceInspect { .. }
            | Self::ApplicationInspect { .. } => None,
        }
    }

    /// Resource ID and kind selected by deployment configuration.
    pub fn target(&self) -> Option<(&str, &'static str)> {
        match self {
            Self::VolumeUsage { target_id }
            | Self::VolumeFiles { target_id, .. }
            | Self::VolumeReadChunk { target_id, .. } => Some((target_id, "Volume")),
            Self::ServiceInspect { target_id } => Some((target_id, "HostService")),
            Self::ApplicationInspect { target_id, .. } => Some((target_id, "Application")),
            Self::SystemStatus {}
            | Self::NodePressure {}
            | Self::NodeInventory {}
            | Self::NodeResolver {}
            | Self::NodeDevices(_) => None,
        }
    }

    /// Build the native Task input from an authority-resolved revision.
    pub fn task_input(&self, reference: Option<&ResourceRef>) -> Option<Value> {
        let Some((id, _)) = self.target() else {
            return Some(match self {
                Self::NodeDevices(query) => json!(query),
                _ => json!({}),
            });
        };
        let reference = reference.filter(|reference| reference.resource_id == id)?;
        let version = match self {
            Self::VolumeFiles { query, .. } => query.volume_version.as_deref(),
            Self::VolumeReadChunk { query, .. } => query.as_ref()?.volume_version.as_deref(),
            _ => None,
        }
        .unwrap_or(&reference.version);
        let target = json!({"id":id,"expected_resource_version":version});
        match self {
            Self::VolumeUsage { .. } => Some(json!({"target":target})),
            Self::VolumeFiles { query, .. } => {
                let mut input = json!({
                    "target":target,"path":query.path,"after":query.after,
                    "limit":query.limit.unwrap_or(MAX_VOLUME_FILES_PAGE),
                    "max_result_bytes":VOLUME_FILES_RESULT_BYTES
                });
                if let Some(revision) = &query.directory_revision {
                    input["expected_directory_revision"] = json!(revision);
                }
                Some(input)
            }
            Self::VolumeReadChunk { query, .. } => {
                let query = query.as_ref()?;
                Some(json!({
                    "target":target,"path":query.path,"offset":query.offset,
                    "limit":query.limit.unwrap_or(MAX_VOLUME_FILE_CHUNK),
                    "expected_file_revision":query.file_revision
                }))
            }
            Self::ServiceInspect { .. } => Some(target),
            Self::ApplicationInspect { include_usage, .. } => Some(json!({
                "id": id, "expected_resource_version": version, "include_usage": include_usage
            })),
            Self::SystemStatus {}
            | Self::NodePressure {}
            | Self::NodeInventory {}
            | Self::NodeResolver {}
            | Self::NodeDevices(_) => unreachable!(),
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
