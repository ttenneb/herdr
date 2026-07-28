use serde::{Deserialize, Serialize};

use super::common::AgentStatus;
use crate::repository::CheckoutKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryTarget {
    pub repository_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryRenameParams {
    pub repository_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryMoveParams {
    pub repository_id: String,
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckoutMoveParams {
    pub workspace_id: String,
    /// Insertion coordinate in the repository's complete
    /// `checkout_workspace_ids` order (the pinned primary is index zero).
    /// Values targeting zero are clamped after the primary.
    pub insert_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CheckoutInfo {
    pub workspace_id: String,
    pub repository_id: String,
    pub checkout_path: String,
    pub kind: CheckoutKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RepositoryInfo {
    pub repository_id: String,
    pub label: String,
    pub git_common_dir: String,
    pub checkout_workspace_ids: Vec<String>,
    pub last_focused_workspace_id: Option<String>,
    pub preferred_base: Option<String>,
    pub focused: bool,
    pub pane_count: usize,
    pub active_agent_count: usize,
    pub agent_status: AgentStatus,
}
