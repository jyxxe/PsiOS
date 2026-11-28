use psios_api::{ApiError, ApiResult, Client, LocalReadTool, ResourceRef, id, parse};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{net::IpAddr, time::Duration};

const MAX_RESULT_BYTES: usize = 8 * 1024;

#[derive(Deserialize, Serialize)]
struct NameServer {
    address: IpAddr,
    zone: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Observation {
    nameservers: Vec<NameServer>,
    boot_id: String,
    observed_at: String,
}

fn check_scope(client: &Client, delegation: &str) -> ApiResult<()> {
    let tool = LocalReadTool::NodeResolver;
    let response = client.call(
        "Capability.CheckLocalReadTool",
        json!({"delegation_id":delegation,"tool":tool}),
    )?;
    match response["allowed"].as_bool() {
        Some(true) => Ok(()),
        Some(false) => Err(ApiError::new(
            "PermissionDenied",
            "local resolver delegation is unavailable",
        )
        .into()),
        None => Err(ApiError::new("RecoveryRequired", "invalid resolver scope response").into()),
    }
}

pub(super) fn observe(
    client: &Client,
    delegation: &str,
    check: &impl Fn() -> ApiResult<()>,
) -> ApiResult<Value> {
    check_scope(client, delegation)?;
    check()?;
    let prepared = client.call(
        "Task.Prepare",
        json!({"tool":LocalReadTool::NodeResolver.name(),"input":{},
               "key":id("shell-resolver"),"delegation_id":delegation}),
    )?;
    check()?;
    let submission = client.submit_prepared_while(prepared, || check().is_ok())?;
    let task = submission["task_id"]
        .as_str()
        .ok_or_else(|| ApiError::new("RecoveryRequired", "invalid resolver Task receipt"))?;
    check()?;
    let settled = client.wait(task, Duration::from_secs(6))?;
    if settled["lifecycle"] != "Succeeded" || settled["outcome_knowledge"] != "Known" {
        let code = settled["conditions"][0]["type"]
            .as_str()
            .unwrap_or("ResultUnavailable");
        return Err(ApiError::new(code, "resolver Task did not complete").into());
    }
    let Some([reference]) = settled["result"]["artifact_refs"]
        .as_array()
        .map(Vec::as_slice)
    else {
        return Err(
            ApiError::new("RecoveryRequired", "invalid resolver artifact reference").into(),
        );
    };
    let reference: ResourceRef = serde_json::from_value(reference.clone())
        .map_err(|_| ApiError::new("RecoveryRequired", "invalid resolver artifact reference"))?;
    let bytes = client.artifact_bytes_checked(&reference, MAX_RESULT_BYTES, check)?;
    let report: Observation = serde_json::from_value(parse(&bytes)?)
        .map_err(|_| ApiError::new("RecoveryRequired", "invalid resolver observation"))?;
    check()?;
    check_scope(client, delegation)?;
    Ok(json!(report))
}
