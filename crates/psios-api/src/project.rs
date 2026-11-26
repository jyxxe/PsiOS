use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    pub key: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ProjectRole {
    Viewer,
    Contributor,
    Reviewer,
    Manager,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetProjectMember {
    pub id: String,
    pub expected_resource_version: String,
    pub principal_id: String,
    /// Supply an explicit `null` to remove a member.
    #[serde(deserialize_with = "project_role")]
    pub role: Option<ProjectRole>,
}

fn project_role<'de, D: Deserializer<'de>>(d: D) -> Result<Option<ProjectRole>, D::Error> {
    Option::deserialize(d)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum WorkItemCriterion {
    TaskResult {
        tool: String,
        target_ref: ResourceRef,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWorkItem {
    pub project_id: String,
    pub expected_project_version: String,
    pub key: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub assignee_principal_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
    pub criterion: WorkItemCriterion,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitWorkItemEvidence {
    pub id: String,
    pub expected_resource_version: String,
    pub task_ref: ResourceRef,
    pub artifact_ref: ResourceRef,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRecord {
    pub reference: ResourceRef,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemPage {
    pub items: Vec<ProjectRecord>,
    pub next_after_id: Option<String>,
}

impl Client {
    pub fn create_project(&self, request: &CreateProject) -> ApiResult<ProjectRecord> {
        self.project_request("Project.Create", request)
    }

    pub fn project(&self, id: &str) -> ApiResult<ProjectRecord> {
        self.project_request("Project.Get", &json!({"id":id}))
    }

    pub fn set_project_member(&self, request: &SetProjectMember) -> ApiResult<ProjectRecord> {
        self.project_request("Project.SetMember", request)
    }

    pub fn create_work_item(&self, request: &CreateWorkItem) -> ApiResult<ProjectRecord> {
        self.project_request("WorkItem.Create", request)
    }

    pub fn work_item(&self, id: &str) -> ApiResult<ProjectRecord> {
        self.project_request("WorkItem.Get", &json!({"id":id}))
    }

    pub fn work_items(
        &self,
        project_id: &str,
        after_id: Option<&str>,
        limit: Option<u32>,
    ) -> ApiResult<WorkItemPage> {
        self.project_request(
            "WorkItem.List",
            &json!({"project_id":project_id,"after_id":after_id,"limit":limit}),
        )
    }

    pub fn submit_work_item_evidence(
        &self,
        request: &SubmitWorkItemEvidence,
    ) -> ApiResult<ProjectRecord> {
        self.project_request("WorkItem.SubmitEvidence", request)
    }

    pub fn review_work_item(
        &self,
        id: &str,
        revision: &str,
        accept: bool,
    ) -> ApiResult<ProjectRecord> {
        self.project_request(
            "WorkItem.Review",
            &json!({"id":id,"expected_resource_version":revision,"accept":accept}),
        )
    }

    pub fn close_work_item(&self, id: &str, revision: &str) -> ApiResult<ProjectRecord> {
        self.project_request(
            "WorkItem.Close",
            &json!({"id":id,"expected_resource_version":revision}),
        )
    }

    fn project_request<T: DeserializeOwned>(
        &self,
        method: &str,
        request: &impl Serialize,
    ) -> ApiResult<T> {
        let request =
            serde_json::to_value(request).map_err(|_| invalid("invalid project request"))?;
        serde_json::from_value(self.call(method, request)?)
            .map_err(|_| invalid("invalid project response"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_member_requires_an_explicit_null_role() {
        let base =
            json!({"id":"project-id","expected_resource_version":"r2","principal_id":"agent-id"});
        assert!(serde_json::from_value::<SetProjectMember>(base.clone()).is_err());
        let mut removal = base;
        removal["role"] = Value::Null;
        let request = serde_json::from_value::<SetProjectMember>(removal.clone()).unwrap();
        assert_eq!(request.role, None);
        assert_eq!(serde_json::to_value(request).unwrap(), removal);
    }
}
