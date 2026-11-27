//! Typed Secret changes on the normal authenticated local authority connection.
use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::Serialize;

pub const MAX_SECRET_BYTES: usize = 8192;

#[derive(Serialize)]
struct Put<'a> {
    name: &'a str,
    value: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<&'a str>,
}

#[derive(Serialize)]
struct Change<'a> {
    id: &'a str,
    expected_resource_version: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotency_key: Option<&'a str>,
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
        self.put_secret_with_key(name, value, None)
    }

    /// Recover the original creation reference after an ambiguous response using the same key and input.
    pub fn put_secret_with_key(
        &self,
        name: &str,
        value: &str,
        idempotency_key: Option<&str>,
    ) -> ApiResult<ResourceRef> {
        validate_value(value)?;
        serde_json::from_value(self.call_params(
            "Secret.Put",
            Put {
                name,
                value,
                idempotency_key,
            },
        )?)
        .map_err(|_| invalid("invalid Secret reference"))
    }

    pub fn rotate_secret(&self, id: &str, revision: &str, value: &str) -> ApiResult<ResourceRef> {
        self.rotate_secret_with_key(id, revision, value, None)
    }

    /// Recover the original rotation reference using the same key, expected revision, and value.
    pub fn rotate_secret_with_key(
        &self,
        id: &str,
        revision: &str,
        value: &str,
        idempotency_key: Option<&str>,
    ) -> ApiResult<ResourceRef> {
        validate_value(value)?;
        self.change_secret("Secret.Rotate", id, revision, Some(value), idempotency_key)
    }

    pub fn revoke_secret(&self, id: &str, revision: &str) -> ApiResult<ResourceRef> {
        self.revoke_secret_with_key(id, revision, None)
    }

    /// Recover the original revocation reference using the same key and expected revision.
    pub fn revoke_secret_with_key(
        &self,
        id: &str,
        revision: &str,
        idempotency_key: Option<&str>,
    ) -> ApiResult<ResourceRef> {
        self.change_secret("Secret.Revoke", id, revision, None, idempotency_key)
    }

    fn change_secret(
        &self,
        method: &str,
        id: &str,
        revision: &str,
        value: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> ApiResult<ResourceRef> {
        serde_json::from_value(self.call_params(
            method,
            Change {
                id,
                expected_resource_version: revision,
                value,
                idempotency_key,
            },
        )?)
        .map_err(|_| invalid("invalid Secret reference"))
    }
}
