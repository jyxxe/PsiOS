use psios_api::{
    ApiError, ApiResult, Client, LocalReadBinding, ResourceRef, VOLUME_FILES_RESULT_BYTES,
    VolumeFileChunkQuery, VolumeFilesQuery, parse,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
pub(crate) struct VolumeAccess {
    pub volume: String,
    pub delegation: String,
    pub preview: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Directory {
    pub key: String,
    pub query: VolumeFilesQuery,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Preview {
    pub key: String,
    pub path: String,
    pub volume_version: String,
}

impl Directory {
    pub(super) fn read(
        self,
        access: VolumeAccess,
        client: &Client,
        check: &impl Fn() -> ApiResult<()>,
    ) -> ApiResult<Value> {
        if !self.query.within_limits()
            || self.query.after.is_some() && self.query.directory_revision.is_none()
        {
            return Err(
                ApiError::new("InvalidArgument", "invalid directory page parameters").into(),
            );
        }
        let binding = LocalReadBinding::VolumeFiles {
            target_id: access.volume.clone(),
            query: self.query,
        };
        let page = access.result(binding, &self.key, client, check)?;
        if !page["entries"].is_array() {
            return Err(ApiError::new("RecoveryRequired", "invalid directory result").into());
        }
        Ok(page)
    }
}

impl Preview {
    pub(super) fn read(
        self,
        access: VolumeAccess,
        client: &Client,
        check: &impl Fn() -> ApiResult<()>,
    ) -> ApiResult<Value> {
        let query = VolumeFileChunkQuery {
            path: self.path.clone(),
            offset: 0,
            limit: None,
            volume_version: Some(self.volume_version),
            file_revision: None,
        };
        if !query.within_limits() {
            return Err(ApiError::new("InvalidArgument", "invalid file preview parameters").into());
        }
        let binding = LocalReadBinding::VolumeReadChunk {
            target_id: access.volume.clone(),
            query: Some(query),
        };
        let result = access.result(binding, &self.key, client, check)?;
        if result["path"] != self.path
            || result["offset"] != 0
            || !result["data_base64"].is_string()
            || !result["eof"].is_boolean()
        {
            return Err(ApiError::new("RecoveryRequired", "invalid file preview result").into());
        }
        Ok(result)
    }
}

impl VolumeAccess {
    fn result(
        &self,
        binding: LocalReadBinding,
        key: &str,
        client: &Client,
        check: &impl Fn() -> ApiResult<()>,
    ) -> ApiResult<Value> {
        let current = client.call("Resource.Get", json!({"id":self.volume}))?;
        let reference: ResourceRef = serde_json::from_value(current["reference"].clone())
            .map_err(|_| ApiError::new("RecoveryRequired", "invalid volume reference"))?;
        if reference.resource_id != self.volume || current["resource"]["kind"] != "Volume" {
            return Err(ApiError::new("NotFound", "configured Volume unavailable").into());
        }
        let input = binding
            .task_input(Some(&reference))
            .ok_or_else(|| ApiError::new("RecoveryRequired", "inconsistent volume target"))?;
        let version = input["target"]["expected_resource_version"].clone();
        check()?;
        let prepared = client.call(
            "Task.Prepare",
            json!({"tool":binding.name(),"input":input,"key":key,"delegation_id":self.delegation}),
        )?;
        check()?;
        let receipt = client.submit_prepared_while(prepared, || check().is_ok())?;
        let id = receipt["task_id"]
            .as_str()
            .ok_or_else(|| ApiError::new("RecoveryRequired", "invalid volume read Task receipt"))?;
        check()?;
        let task = client.call("Task.Wait", json!({"id":id,"timeout_ms":6_000}))?;
        if task["lifecycle"] != "Succeeded" {
            let code = task["conditions"][0]["type"]
                .as_str()
                .unwrap_or("DependencyUnavailable");
            return Err(ApiError::new(code, "volume read did not complete").into());
        }
        let [artifact] = task["result"]["artifact_refs"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        else {
            return Err(
                ApiError::new("RecoveryRequired", "invalid volume read Task result").into(),
            );
        };
        let artifact = serde_json::from_value(artifact.clone()).map_err(|_| {
            ApiError::new("RecoveryRequired", "invalid volume read artifact reference")
        })?;
        let bytes = client.artifact_bytes_checked(&artifact, VOLUME_FILES_RESULT_BYTES, check)?;
        let page: Value = parse(&bytes)?;
        if page["volume_version"] != version {
            return Err(ApiError::new(
                "RecoveryRequired",
                "volume read returned a different version",
            )
            .into());
        }
        check()?;
        Ok(page)
    }
}
