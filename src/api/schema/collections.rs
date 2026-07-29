use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::{PaneInfo, SplitDirection};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct CollectionListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionTarget {
    pub collection_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionCreateParams {
    pub target_pane_id: String,
    pub direction: SplitDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionAddParams {
    pub collection_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionMoveParams {
    pub pane_id: String,
    pub collection_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionPromoteParams {
    pub pane_id: String,
    pub target_pane_id: String,
    pub direction: SplitDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<f32>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionSelectParams {
    pub collection_id: String,
    pub pane_id: String,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionReorderParams {
    pub collection_id: String,
    pub pane_id: String,
    pub index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionMemberTarget {
    pub collection_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionCloseParams {
    pub collection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<CollectionCloseDisposition>,
    /// Legacy split-promotion target. Must be omitted: collection promotion now creates one
    /// standalone tab per member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_pane_id: Option<String>,
    #[serde(default)]
    pub focus_promoted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CollectionCloseDisposition {
    CascadeClose,
    PromoteMembers,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PanePlacementInfo {
    #[default]
    Tiled,
    Collection {
        collection_id: String,
        member_index: usize,
        archived: bool,
        selected: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionMemberInfo {
    pub pane_id: String,
    pub index: usize,
    pub archived: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionLifecycleSummary {
    pub active: usize,
    pub archived: usize,
    pub live: usize,
    pub working: usize,
    pub blocked: usize,
    pub exited: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionInfo {
    pub collection_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_pane_id: Option<String>,
    pub members: Vec<CollectionMemberInfo>,
    pub lifecycle: CollectionLifecycleSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionCreateMemberParams {
    pub collection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CollectionCreateMemberResult {
    pub collection: CollectionInfo,
    pub pane: PaneInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation_id: Option<String>,
}
