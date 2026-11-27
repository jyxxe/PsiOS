//! Bounded inspection of the local Task journal.
use crate::{ApiResult, Client, ResourceRef, invalid};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskLifecycle {
    Accepted,
    Queued,
    Running,
    WaitingInput,
    WaitingApproval,
    WaitingResource,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_principal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<TaskLifecycle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskOutcomeKnowledge {
    NotStarted,
    InFlight,
    Known,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskResultAvailability {
    Pending,
    NoneExpected,
    Available,
    Evicted,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSummary {
    pub task_ref: ResourceRef,
    pub owner_principal_id: String,
    pub task_type: String,
    pub lifecycle: TaskLifecycle,
    pub outcome_knowledge: TaskOutcomeKnowledge,
    pub result_availability: TaskResultAvailability,
    pub current_run_id: Option<String>,
    pub accepted_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskListPage {
    pub items: Vec<TaskSummary>,
    /// Continue even when the current page contains no matches.
    pub next_cursor: Option<String>,
    pub complete: bool,
}

impl Client {
    pub fn task_list(&self, query: &TaskListQuery) -> ApiResult<TaskListPage> {
        serde_json::from_value(self.call_params("Task.List", query)?)
            .map_err(|_| invalid("invalid Task list page"))
    }
}
