use psios_api::{
    ApiError, ApiResult, Client, DeviceInventoryCursor, DeviceInventoryQuery, LocalReadTool,
    ResourceRef, id, parse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{net::IpAddr, time::Duration};

const RESOLVER_RESULT_BYTES: usize = 8 * 1024;
const INVENTORY_RESULT_BYTES: usize = 16 * 1024;
const INVENTORY_PAGE_SIZE: u16 = 6;

#[derive(Clone, Copy)]
pub(super) enum DeviceBus {
    Pci,
    Usb,
}

#[derive(Deserialize, Serialize)]
struct NameServer {
    address: IpAddr,
    zone: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Resolver {
    nameservers: Vec<NameServer>,
    boot_id: String,
    observed_at: String,
}

#[derive(Deserialize, Serialize)]
struct PciDevice {
    address: String,
    parent_address: Option<String>,
    vendor_id: String,
    device_id: String,
    subsystem_vendor_id: String,
    subsystem_device_id: String,
    class_code: String,
    revision: String,
    driver: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct UsbDevice {
    location: String,
    parent_location: Option<String>,
    bus_number: u16,
    device_address: u16,
    vendor_id: String,
    product_id: String,
    class_code: String,
    speed_mbps: Option<String>,
    authorized: bool,
}

#[derive(Deserialize, Serialize)]
struct InventoryPage<Device> {
    devices: Vec<Device>,
    next_cursor: Option<DeviceInventoryCursor>,
    boot_id: String,
    observed_at: String,
}

fn check_scope(client: &Client, delegation: &str, tool: LocalReadTool) -> ApiResult<()> {
    let response = client.call(
        "Capability.CheckLocalReadTool",
        json!({"delegation_id":delegation,"tool":tool}),
    )?;
    match response["allowed"].as_bool() {
        Some(true) => Ok(()),
        Some(false) => Err(ApiError::new(
            "PermissionDenied",
            "local observation delegation is unavailable",
        )
        .into()),
        None => Err(ApiError::new("RecoveryRequired", "invalid local observation scope").into()),
    }
}

fn read<T: DeserializeOwned>(
    client: &Client,
    delegation: &str,
    tool: LocalReadTool,
    input: Value,
    maximum: usize,
    check: &impl Fn() -> ApiResult<()>,
) -> ApiResult<T> {
    check_scope(client, delegation, tool)?;
    check()?;
    let prepared = client.call(
        "Task.Prepare",
        json!({"tool":tool.name(),"input":input,"key":id("shell-observation"),"delegation_id":delegation}),
    )?;
    check()?;
    let submission = client.submit_prepared_while(prepared, || check().is_ok())?;
    let task = submission["task_id"].as_str().ok_or_else(|| {
        ApiError::new("RecoveryRequired", "invalid local observation Task receipt")
    })?;
    check()?;
    let settled = client.wait(task, Duration::from_secs(6))?;
    if settled["lifecycle"] != "Succeeded" || settled["outcome_knowledge"] != "Known" {
        let code = settled["conditions"][0]["type"]
            .as_str()
            .unwrap_or("ResultUnavailable");
        return Err(ApiError::new(code, "local observation Task did not complete").into());
    }
    let Some([reference]) = settled["result"]["artifact_refs"]
        .as_array()
        .map(Vec::as_slice)
    else {
        return Err(ApiError::new(
            "RecoveryRequired",
            "invalid local observation artifact reference",
        )
        .into());
    };
    let reference: ResourceRef = serde_json::from_value(reference.clone()).map_err(|_| {
        ApiError::new(
            "RecoveryRequired",
            "invalid local observation artifact reference",
        )
    })?;
    let bytes = client.artifact_bytes_checked(&reference, maximum, check)?;
    let report = serde_json::from_value(parse(&bytes)?)
        .map_err(|_| ApiError::new("RecoveryRequired", "invalid local observation result"))?;
    check()?;
    check_scope(client, delegation, tool)?;
    Ok(report)
}

pub(super) fn resolver(
    client: &Client,
    delegation: &str,
    check: &impl Fn() -> ApiResult<()>,
) -> ApiResult<Value> {
    let report: Resolver = read(
        client,
        delegation,
        LocalReadTool::NodeResolver,
        json!({}),
        RESOLVER_RESULT_BYTES,
        check,
    )?;
    Ok(json!(report))
}

pub(super) fn inventory(
    client: &Client,
    delegation: &str,
    bus: DeviceBus,
    cursor: Option<DeviceInventoryCursor>,
    check: &impl Fn() -> ApiResult<()>,
) -> ApiResult<Value> {
    let query = DeviceInventoryQuery {
        limit: Some(INVENTORY_PAGE_SIZE),
        cursor,
    };
    let query = json!(query);
    match bus {
        DeviceBus::Pci => {
            let report: InventoryPage<PciDevice> = read(
                client,
                delegation,
                LocalReadTool::NodePci,
                query,
                INVENTORY_RESULT_BYTES,
                check,
            )?;
            Ok(json!(report))
        }
        DeviceBus::Usb => {
            let report: InventoryPage<UsbDevice> = read(
                client,
                delegation,
                LocalReadTool::NodeUsb,
                query,
                INVENTORY_RESULT_BYTES,
                check,
            )?;
            Ok(json!(report))
        }
    }
}
