//! Page queries shared by the verified local device inventory Tasks.
use serde::{Deserialize, Serialize};

pub const MAX_DEVICE_INVENTORY_PAGE: u16 = 32;
pub const MAX_BLOCK_INVENTORY_PAGE: u16 = MAX_DEVICE_INVENTORY_PAGE;
pub const MAX_PCI_INVENTORY_ROWS: u16 = 256;
pub const MAX_USB_INVENTORY_ROWS: u16 = 256;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInventoryCursor {
    pub snapshot_digest: String,
    pub offset: u16,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceInventoryQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<DeviceInventoryCursor>,
}

pub type BlockInventoryCursor = DeviceInventoryCursor;
pub type BlockInventoryQuery = DeviceInventoryQuery;
