//! The bounded set of no-argument diagnostics available through local adapters.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum LocalReadTool {
    #[serde(rename = "system.status")]
    SystemStatus,
    #[serde(rename = "node.pressure")]
    NodePressure,
}

impl LocalReadTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::SystemStatus => "system.status",
            Self::NodePressure => "node.pressure",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalReadToolsRequest {
    pub delegation_id: String,
    pub tools: Vec<LocalReadTool>,
}
