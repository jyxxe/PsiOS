//! Page queries for the verified local block inventory Task.
use serde::{Deserialize, Serialize};

pub const MAX_BLOCK_INVENTORY_PAGE: u16 = 32;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockInventoryCursor {
    pub snapshot_digest: String,
    pub offset: u16,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockInventoryQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<BlockInventoryCursor>,
}
