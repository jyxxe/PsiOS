//! Typed Secret changes on the normal authenticated local authority connection.
use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::Serialize;

pub const MAX_SECRET_BYTES: usize = 8192;

#[derive(Serialize)]
struct Put<'a> {
    name: &'a str,
    value: &'a str,
}

#[derive(Serialize)]
struct Change<'a> {
    id: &'a str,
    expected_resource_version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a str>,
}

fn validate_value(value: &str) -> ApiResult<()> {
    if (1..=MAX_SECRET_BYTES).contains(&value.len()) {
        Ok(())
    } else {
        Err(invalid("secret size outside limit"))
    }
}

impl Client {
    pub fn put_secret(&self, name: &str, value: &str) -> ApiResult<ResourceRef> {
        validate_value(value)?;
        serde_json::from_value(self.call_params("Secret.Put", Put { name, value })?)
            .map_err(|_| invalid("invalid Secret reference"))
    }

    pub fn rotate_secret(&self, id: &str, revision: &str, value: &str) -> ApiResult<ResourceRef> {
        validate_value(value)?;
        self.change_secret("Secret.Rotate", id, revision, Some(value))
    }

    pub fn revoke_secret(&self, id: &str, revision: &str) -> ApiResult<ResourceRef> {
        self.change_secret("Secret.Revoke", id, revision, None)
    }

    fn change_secret(
        &self,
        method: &str,
        id: &str,
        revision: &str,
        value: Option<&str>,
    ) -> ApiResult<ResourceRef> {
        serde_json::from_value(self.call_params(
            method,
            Change {
                id,
                expected_resource_version: revision,
                value,
            },
        )?)
        .map_err(|_| invalid("invalid Secret reference"))
    }
}
