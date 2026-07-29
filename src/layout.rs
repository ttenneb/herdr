//! BSP tree layout for tiling panes within a workspace.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use ratatui::{
    layout::{Direction, Rect},
    widgets::Borders,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct PaneId(u32);

/// Global atomic counter for unique PaneId generation across all workspaces.
static NEXT_PANE_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
static NEXT_COLLECTION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl PaneId {
    /// Allocate a globally unique PaneId.
    pub fn alloc() -> Self {
        Self(NEXT_PANE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    /// Reconstruct from a saved u32 (persistence only).
    pub fn from_raw(id: u32) -> Self {
        Self(id)
    }
}

/// Stable identity for a pane collection. Collection identity is independent
/// from pane identity and from the private slot used by the current BSP tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CollectionId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionIdError {
    Invalid,
    Exhausted,
}

impl CollectionId {
    const SERIALIZED_PREFIX: &'static str = "collection_";

    pub fn alloc() -> Result<Self, CollectionIdError> {
        NEXT_COLLECTION_ID
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |current| (current > 0 && current < u64::MAX).then_some(current + 1),
            )
            .map(Self)
            .map_err(|_| CollectionIdError::Exhausted)
    }

    pub fn raw(self) -> u64 {
        self.0
    }

    /// Reconstruct an ID from persisted state while reserving it from allocation.
    pub fn from_raw(id: u64) -> Result<Self, CollectionIdError> {
        let id = Self::parse_raw(id)?;
        let next = id.0.checked_add(1).ok_or(CollectionIdError::Invalid)?;
        let mut current = NEXT_COLLECTION_ID.load(std::sync::atomic::Ordering::Relaxed);
        while current <= id.0 {
            match NEXT_COLLECTION_ID.compare_exchange_weak(
                current,
                next,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        Ok(id)
    }

    /// Parse an untrusted public ID without changing allocator state.
    pub fn parse(raw: &str) -> Result<Self, CollectionIdError> {
        let id = raw
            .strip_prefix(Self::SERIALIZED_PREFIX)
            .ok_or(CollectionIdError::Invalid)?
            .parse::<u64>()
            .map_err(|_| CollectionIdError::Invalid)?;
        Self::parse_raw(id)
    }

    fn parse_raw(id: u64) -> Result<Self, CollectionIdError> {
        (id > 0 && id < u64::MAX)
            .then_some(Self(id))
            .ok_or(CollectionIdError::Invalid)
    }
}

impl serde::Serialize for CollectionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("{}{id}", Self::SERIALIZED_PREFIX, id = self.0))
    }
}

impl<'de> serde::Deserialize<'de> for CollectionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        let id =
            Self::parse(&value).map_err(|_| serde::de::Error::custom("invalid collection id"))?;
        Self::from_raw(id.raw()).map_err(|_| serde::de::Error::custom("invalid collection id"))
    }
}

/// A typed top-level layout leaf. Collections occupy BSP space but own no PTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LayoutLeaf {
    Pane(PaneId),
    Collection(CollectionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanePlacement {
    Tiled,
    Collection(CollectionId),
}

/// Shared runtime organization for one collection leaf. Presentation details
/// such as expansion and scroll position intentionally do not live here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCollection {
    pub id: CollectionId,
    pub label: Option<String>,
    members: Vec<PaneId>,
    archived: HashSet<PaneId>,
    selected: Option<PaneId>,
    /// Monotonic membership/archive revision used to bind destructive confirmations.
    revision: u64,
}

impl PaneCollection {
    fn empty(id: CollectionId, label: Option<String>) -> Self {
        Self {
            id,
            label,
            members: Vec::new(),
            archived: HashSet::new(),
            selected: None,
            revision: 0,
        }
    }

    pub fn members(&self) -> &[PaneId] {
        &self.members
    }

    pub fn selected(&self) -> Option<PaneId> {
        self.selected
    }

    pub fn is_archived(&self, pane_id: PaneId) -> bool {
        self.archived.contains(&pane_id)
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub fn active_members(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.members
            .iter()
            .copied()
            .filter(|pane_id| !self.archived.contains(pane_id))
    }

    pub fn archived_members(&self) -> impl Iterator<Item = PaneId> + '_ {
        self.members
            .iter()
            .copied()
            .filter(|pane_id| self.archived.contains(pane_id))
    }

    pub(crate) fn from_saved(
        id: CollectionId,
        label: Option<String>,
        members: Vec<PaneId>,
        selected: Option<PaneId>,
        archived: HashSet<PaneId>,
    ) -> Option<Self> {
        let unique: HashSet<_> = members.iter().copied().collect();
        if unique.len() != members.len()
            || !archived.is_subset(&unique)
            || selected.is_some_and(|pane| !unique.contains(&pane))
            || (selected.is_none() && !members.is_empty())
        {
            return None;
        }
        Some(Self {
            id,
            label,
            members,
            archived,
            selected,
            revision: 0,
        })
    }
}

/// Snapshot of a pane's position and focus state after layout.
#[derive(Clone)]
pub struct PaneInfo {
    pub id: PaneId,
    /// Outer rect (including borders if present).
    pub rect: Rect,
    /// Inner rect (content area, excluding borders). Used for selection.
    pub inner_rect: Rect,
    /// Visible scrollbar lane, when scrollback is present. `inner_rect` may still
    /// exclude a stable hidden gutter when this is `None`.
    pub scrollbar_rect: Option<Rect>,
    /// Borders drawn around this pane after UI chrome is applied.
    pub borders: Borders,
    pub is_focused: bool,
}

/// Info about a split boundary, used for mouse drag resize.
#[derive(Clone)]
pub struct SplitBorder {
    /// Position of the divider line (x for horizontal split, y for vertical).
    pub pos: u16,
    /// Direction of the split that created this border.
    pub direction: Direction,
    /// Ratio assigned to the first child of this split.
    pub ratio: f32,
    /// Total area of the split node.
    pub area: Rect,
    /// Path from root to this split node (false=first, true=second).
    pub path: Vec<bool>,
}

/// Cardinal direction for pane navigation.
#[derive(Debug, Clone, Copy)]
pub enum NavDirection {
    Left,
    Right,
    Up,
    Down,
}

/// A node in the BSP tree. Public for serialization.
#[derive(Clone)]
pub enum Node {
    Pane(PaneId),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TypedNode {
    Leaf(LayoutLeaf),
    Split {
        direction: Direction,
        ratio: f32,
        first: Box<TypedNode>,
        second: Box<TypedNode>,
    },
}

/// BSP tiling layout with typed pane and collection leaves.
#[derive(Clone)]
pub struct TileLayout {
    root: TypedNode,
    focus: LayoutLeaf,
    legacy_focus: PaneId,
    legacy_root: Node,
    collections: HashMap<CollectionId, PaneCollection>,
}

impl TileLayout {
    pub fn new() -> (Self, PaneId) {
        let root_id = PaneId::alloc();
        (
            Self {
                root: TypedNode::Leaf(LayoutLeaf::Pane(root_id)),
                focus: LayoutLeaf::Pane(root_id),
                legacy_focus: root_id,
                legacy_root: Node::Pane(root_id),
                collections: HashMap::new(),
            },
            root_id,
        )
    }

    /// Compatibility pane focus. A focused non-empty collection resolves to
    /// its selected member; an empty collection falls back to another placed pane.
    pub fn focused(&self) -> PaneId {
        match self.focus {
            LayoutLeaf::Pane(pane_id) => pane_id,
            LayoutLeaf::Collection(collection_id) => self
                .collection(collection_id)
                .and_then(PaneCollection::selected)
                .or_else(|| self.pane_ids().into_iter().next())
                .unwrap_or(self.legacy_focus),
        }
    }

    pub fn focused_leaf(&self) -> LayoutLeaf {
        self.focus
    }

    /// All placed panes, including collection members.
    pub fn pane_count(&self) -> usize {
        self.pane_ids().len()
    }

    #[cfg(test)]
    pub fn tiled_pane_count(&self) -> usize {
        self.tiled_pane_ids().len()
    }

    pub fn leaf_count(&self) -> usize {
        typed_count(&self.root)
    }

    pub fn is_single_pane_leaf(&self) -> bool {
        matches!(self.root, TypedNode::Leaf(LayoutLeaf::Pane(_)))
    }

    pub fn leaves(&self) -> Vec<LayoutLeaf> {
        let mut leaves = Vec::new();
        typed_collect_leaves(&self.root, &mut leaves);
        leaves
    }

    /// Rectangle occupied by a typed top-level leaf.
    pub fn leaf_rect(&self, leaf: LayoutLeaf, area: Rect) -> Option<Rect> {
        typed_leaf_rect(&self.root, leaf, area)
    }

    pub fn collection_ids(&self) -> Vec<CollectionId> {
        self.leaves()
            .into_iter()
            .filter_map(|leaf| match leaf {
                LayoutLeaf::Collection(id) => Some(id),
                LayoutLeaf::Pane(_) => None,
            })
            .collect()
    }

    pub fn collection(&self, id: CollectionId) -> Option<&PaneCollection> {
        self.collections.get(&id)
    }

    pub fn collections(&self) -> impl Iterator<Item = &PaneCollection> {
        self.collections.values()
    }

    pub fn placement(&self, pane_id: PaneId) -> Option<PanePlacement> {
        if self.tiled_pane_ids().contains(&pane_id) {
            return Some(PanePlacement::Tiled);
        }
        self.collections.values().find_map(|collection| {
            collection
                .members
                .contains(&pane_id)
                .then_some(PanePlacement::Collection(collection.id))
        })
    }

    pub fn panes(&self, area: Rect) -> Vec<PaneInfo> {
        let mut result = Vec::new();
        typed_collect_panes(&self.root, area, self.focus, &mut result);
        result
    }

    pub fn splits(&self, area: Rect) -> Vec<SplitBorder> {
        let mut result = Vec::new();
        typed_collect_splits(&self.root, area, vec![], &mut result);
        result
    }

    pub fn split_focused(&mut self, direction: Direction) -> PaneId {
        self.split_focused_with_ratio(direction, 0.5)
    }

    pub fn split_focused_with_ratio(&mut self, direction: Direction, ratio: f32) -> PaneId {
        let new_id = PaneId::alloc();
        let old = std::mem::replace(
            &mut self.root,
            TypedNode::Leaf(LayoutLeaf::Pane(PaneId::from_raw(0))),
        );
        self.root = typed_split_at(
            old,
            self.focus,
            direction,
            LayoutLeaf::Pane(new_id),
            valid_split_ratio(ratio),
        );
        self.focus = LayoutLeaf::Pane(new_id);
        self.legacy_focus = new_id;
        self.refresh_legacy_root();
        new_id
    }

    pub fn insert_pane_near(
        &mut self,
        target: PaneId,
        moved: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> bool {
        if target == moved
            || self.placement(target) != Some(PanePlacement::Tiled)
            || self.placement(moved).is_some()
        {
            return false;
        }
        let old = std::mem::replace(
            &mut self.root,
            TypedNode::Leaf(LayoutLeaf::Pane(PaneId::from_raw(0))),
        );
        self.root = typed_split_at(
            old,
            LayoutLeaf::Pane(target),
            direction,
            LayoutLeaf::Pane(moved),
            valid_split_ratio(ratio),
        );
        self.focus = LayoutLeaf::Pane(moved);
        self.legacy_focus = moved;
        self.refresh_legacy_root();
        true
    }

    pub fn insert_collection_near(
        &mut self,
        target: LayoutLeaf,
        collection_id: CollectionId,
        direction: Direction,
        ratio: f32,
    ) -> bool {
        if self.collections.contains_key(&collection_id) || !self.leaves().contains(&target) {
            return false;
        }
        let old = std::mem::replace(
            &mut self.root,
            TypedNode::Leaf(LayoutLeaf::Pane(PaneId::from_raw(0))),
        );
        self.root = typed_split_at(
            old,
            target,
            direction,
            LayoutLeaf::Collection(collection_id),
            valid_split_ratio(ratio),
        );
        self.collections
            .insert(collection_id, PaneCollection::empty(collection_id, None));
        true
    }

    pub fn focus_leaf(&mut self, leaf: LayoutLeaf) -> bool {
        if !self.leaves().contains(&leaf) {
            return false;
        }
        self.focus = leaf;
        if let LayoutLeaf::Pane(pane_id) = leaf {
            self.legacy_focus = pane_id;
        }
        true
    }

    pub fn remove_collection(&mut self, collection_id: CollectionId) -> bool {
        if self.leaf_count() <= 1
            || !self
                .leaves()
                .contains(&LayoutLeaf::Collection(collection_id))
        {
            return false;
        }
        if !self.remove_leaf(LayoutLeaf::Collection(collection_id)) {
            return false;
        }
        self.collections.remove(&collection_id);
        true
    }

    pub fn set_collection_label(&mut self, id: CollectionId, label: Option<String>) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        collection.label = label;
        true
    }

    pub(crate) fn add_collection_member(&mut self, id: CollectionId, pane: PaneId) -> bool {
        if self.placement(pane).is_some() {
            return false;
        }
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        collection.members.push(pane);
        collection.selected.get_or_insert(pane);
        collection.revision = collection.revision.saturating_add(1);
        self.refresh_legacy_root();
        true
    }

    pub(crate) fn remove_collection_member(&mut self, id: CollectionId, pane: PaneId) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        let Some(index) = collection.members.iter().position(|member| *member == pane) else {
            return false;
        };
        collection.members.remove(index);
        collection.archived.remove(&pane);
        if collection.selected == Some(pane) {
            collection.selected = collection
                .members
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| collection.members.get(i)))
                .copied();
        }
        collection.revision = collection.revision.saturating_add(1);
        self.refresh_legacy_root();
        true
    }

    pub(crate) fn select_collection_member(&mut self, id: CollectionId, pane: PaneId) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        if !collection.members.contains(&pane) {
            return false;
        }
        collection.selected = Some(pane);
        if self.focus == LayoutLeaf::Collection(id) {
            self.legacy_focus = pane;
        }
        self.refresh_legacy_root();
        true
    }

    pub fn reorder_collection_member(
        &mut self,
        id: CollectionId,
        pane: PaneId,
        index: usize,
    ) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        let Some(current) = collection.members.iter().position(|member| *member == pane) else {
            return false;
        };
        if index >= collection.members.len() {
            return false;
        }
        let pane = collection.members.remove(current);
        collection.members.insert(index, pane);
        collection.revision = collection.revision.saturating_add(1);
        self.refresh_legacy_root();
        true
    }

    pub(crate) fn set_member_archived(
        &mut self,
        id: CollectionId,
        pane: PaneId,
        archived: bool,
    ) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        if !collection.members.contains(&pane) {
            return false;
        }
        let changed = if archived {
            collection.archived.insert(pane)
        } else {
            collection.archived.remove(&pane)
        };
        if changed {
            collection.revision = collection.revision.saturating_add(1);
        }
        true
    }

    /// Undo a just-started archived-member restore without advancing the destructive
    /// confirmation revision. This is deliberately narrower than a general revision setter.
    pub(crate) fn rollback_member_restore(
        &mut self,
        id: CollectionId,
        pane: PaneId,
        original_revision: u64,
    ) -> bool {
        let Some(collection) = self.collections.get_mut(&id) else {
            return false;
        };
        if !collection.members.contains(&pane) || collection.archived.contains(&pane) {
            return false;
        }
        collection.archived.insert(pane);
        collection.revision = original_revision;
        true
    }

    // Retained as the pure layout primitive exercised by collection invariant tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn remove_tiled_pane_for_collection(&mut self, pane: PaneId) -> bool {
        self.placement(pane) == Some(PanePlacement::Tiled)
            && self.remove_leaf(LayoutLeaf::Pane(pane))
    }

    pub fn close_focused(&mut self) -> bool {
        let LayoutLeaf::Pane(pane) = self.focus else {
            return false;
        };
        self.remove_leaf(LayoutLeaf::Pane(pane))
    }

    pub fn focus_pane(&mut self, id: PaneId) {
        match self.placement(id) {
            Some(PanePlacement::Tiled) => {
                let _ = self.focus_leaf(LayoutLeaf::Pane(id));
            }
            Some(PanePlacement::Collection(collection_id))
                if self.select_collection_member(collection_id, id) =>
            {
                let _ = self.focus_leaf(LayoutLeaf::Collection(collection_id));
                self.legacy_focus = id;
            }
            Some(PanePlacement::Collection(_)) | None => {}
        }
    }

    pub fn swap_panes(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second
            || self.placement(first) != Some(PanePlacement::Tiled)
            || self.placement(second) != Some(PanePlacement::Tiled)
        {
            return false;
        }
        typed_swap_panes(&mut self.root, first, second);
        self.refresh_legacy_root();
        true
    }

    pub fn set_ratio_at(&mut self, path: &[bool], ratio: f32) -> bool {
        let changed = typed_set_ratio(&mut self.root, path, ratio.clamp(0.1, 0.9));
        if changed {
            self.refresh_legacy_root();
        }
        changed
    }

    pub fn resize_focused(&mut self, nav: NavDirection, delta: f32, area: Rect) {
        let Some(focused_rect) = typed_leaf_rect(&self.root, self.focus, area) else {
            return;
        };
        let target_dir = match nav {
            NavDirection::Left | NavDirection::Right => Direction::Horizontal,
            NavDirection::Up | NavDirection::Down => Direction::Vertical,
        };
        let grows = matches!(nav, NavDirection::Right | NavDirection::Down);
        let splits = self.splits(area);
        let best = nearest_resize_split(&splits, target_dir, focused_rect, nav).or_else(|| {
            nearest_resize_split(&splits, target_dir, focused_rect, opposite_direction(nav))
        });
        if let Some(split) = best {
            let current = typed_get_ratio(&self.root, &split.path).unwrap_or(0.5);
            let adjustment = if grows { delta } else { -delta };
            self.set_ratio_at(&split.path, current + adjustment);
        }
    }

    pub fn resize_pane(
        &mut self,
        pane_id: PaneId,
        nav: NavDirection,
        delta: f32,
        area: Rect,
    ) -> bool {
        let Some(placement) = self.placement(pane_id) else {
            return false;
        };
        let target = match placement {
            PanePlacement::Tiled => LayoutLeaf::Pane(pane_id),
            PanePlacement::Collection(collection_id) => LayoutLeaf::Collection(collection_id),
        };
        let before = typed_split_ratios(&self.root);
        let previous = self.focus;
        self.focus = target;
        self.resize_focused(nav, delta, area);
        self.focus = previous;
        typed_split_ratios(&self.root) != before
    }

    /// All placed panes in deterministic top-level/member order.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        self.placed_pane_ids()
    }

    /// Panes that occupy ordinary top-level BSP leaves.
    pub fn tiled_pane_ids(&self) -> Vec<PaneId> {
        self.leaves()
            .into_iter()
            .filter_map(|leaf| match leaf {
                LayoutLeaf::Pane(id) => Some(id),
                LayoutLeaf::Collection(_) => None,
            })
            .collect()
    }

    pub fn placed_pane_ids(&self) -> Vec<PaneId> {
        let mut panes = Vec::new();
        for leaf in self.leaves() {
            match leaf {
                LayoutLeaf::Pane(pane_id) => panes.push(pane_id),
                LayoutLeaf::Collection(collection_id) => {
                    if let Some(collection) = self.collection(collection_id) {
                        panes.extend_from_slice(collection.members());
                    }
                }
            }
        }
        panes
    }

    pub(crate) fn typed_root(&self) -> &TypedNode {
        &self.root
    }

    pub(crate) fn from_typed_saved(
        root: TypedNode,
        focus: LayoutLeaf,
        collections: Vec<PaneCollection>,
    ) -> Option<Self> {
        let mut leaves = Vec::new();
        typed_collect_leaves(&root, &mut leaves);
        let leaf_set: HashSet<_> = leaves.iter().copied().collect();
        if leaves.is_empty() || leaf_set.len() != leaves.len() || !leaf_set.contains(&focus) {
            return None;
        }
        let collection_leaf_ids: HashSet<_> = leaves
            .iter()
            .filter_map(|leaf| match leaf {
                LayoutLeaf::Collection(id) => Some(*id),
                LayoutLeaf::Pane(_) => None,
            })
            .collect();
        let mut collection_map = HashMap::new();
        for collection in collections {
            if !collection_leaf_ids.contains(&collection.id)
                || collection_map.insert(collection.id, collection).is_some()
            {
                return None;
            }
        }
        if collection_map.len() != collection_leaf_ids.len() {
            return None;
        }
        let mut placed: HashSet<_> = leaves
            .iter()
            .filter_map(|leaf| match leaf {
                LayoutLeaf::Pane(id) => Some(*id),
                LayoutLeaf::Collection(_) => None,
            })
            .collect();
        for collection in collection_map.values() {
            for member in collection.members() {
                if !placed.insert(*member) {
                    return None;
                }
            }
        }
        let legacy_focus = match focus {
            LayoutLeaf::Pane(id) => id,
            LayoutLeaf::Collection(id) => collection_map
                .get(&id)
                .and_then(PaneCollection::selected)
                .or_else(|| placed.iter().copied().min_by_key(|id| id.raw()))
                .unwrap_or_else(|| PaneId::from_raw(0)),
        };
        // Pane-only compatibility cannot represent a focused empty collection.
        // Keep its private projection inert; typed focus remains authoritative and
        // public compatibility fields report `None` until a live pane exists.
        let legacy_root =
            legacy_projection(&root, &collection_map).unwrap_or(Node::Pane(legacy_focus));
        Some(Self {
            root,
            focus,
            legacy_focus,
            legacy_root,
            collections: collection_map,
        })
    }

    /// Pane-only compatibility projection for current API code.
    /// Typed persistence must serialize `typed_root()` and collection records.
    pub fn root(&self) -> &Node {
        &self.legacy_root
    }

    pub fn from_saved(root: Node, focus: PaneId) -> Self {
        Self {
            root: typed_from_legacy(&root),
            focus: LayoutLeaf::Pane(focus),
            legacy_focus: focus,
            legacy_root: root,
            collections: HashMap::new(),
        }
    }

    fn remove_leaf(&mut self, target: LayoutLeaf) -> bool {
        if self.leaf_count() <= 1 {
            return false;
        }
        let leaves = self.leaves();
        let Some(position) = leaves.iter().position(|leaf| *leaf == target) else {
            return false;
        };
        let replacement = leaves
            .get(position + 1)
            .or_else(|| position.checked_sub(1).and_then(|i| leaves.get(i)))
            .copied();
        let old = std::mem::replace(
            &mut self.root,
            TypedNode::Leaf(LayoutLeaf::Pane(PaneId::from_raw(0))),
        );
        let Some(root) = typed_remove(old, target) else {
            return false;
        };
        self.root = root;
        if self.focus == target {
            if let Some(replacement) = replacement {
                self.focus = replacement;
                if let LayoutLeaf::Pane(pane_id) = replacement {
                    self.legacy_focus = pane_id;
                }
            }
        }
        if target == LayoutLeaf::Pane(self.legacy_focus) {
            if let Some(pane_id) = self.pane_ids().into_iter().next() {
                self.legacy_focus = pane_id;
            }
        }
        self.refresh_legacy_root();
        true
    }

    fn refresh_legacy_root(&mut self) {
        if let Some(root) = legacy_projection(&self.root, &self.collections) {
            self.legacy_root = root;
        }
    }
}

fn typed_from_legacy(node: &Node) -> TypedNode {
    match node {
        Node::Pane(id) => TypedNode::Leaf(LayoutLeaf::Pane(*id)),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => TypedNode::Split {
            direction: *direction,
            ratio: *ratio,
            first: Box::new(typed_from_legacy(first)),
            second: Box::new(typed_from_legacy(second)),
        },
    }
}

fn legacy_projection(
    node: &TypedNode,
    collections: &HashMap<CollectionId, PaneCollection>,
) -> Option<Node> {
    match node {
        TypedNode::Leaf(LayoutLeaf::Pane(id)) => Some(Node::Pane(*id)),
        TypedNode::Leaf(LayoutLeaf::Collection(collection_id)) => collections
            .get(collection_id)
            .and_then(PaneCollection::selected)
            .map(Node::Pane),
        TypedNode::Split {
            direction,
            ratio,
            first,
            second,
        } => match (
            legacy_projection(first, collections),
            legacy_projection(second, collections),
        ) {
            (Some(first), Some(second)) => Some(Node::Split {
                direction: *direction,
                ratio: *ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(node), None) | (None, Some(node)) => Some(node),
            (None, None) => None,
        },
    }
}

fn typed_count(node: &TypedNode) -> usize {
    match node {
        TypedNode::Leaf(_) => 1,
        TypedNode::Split { first, second, .. } => typed_count(first) + typed_count(second),
    }
}

fn typed_collect_leaves(node: &TypedNode, result: &mut Vec<LayoutLeaf>) {
    match node {
        TypedNode::Leaf(leaf) => result.push(*leaf),
        TypedNode::Split { first, second, .. } => {
            typed_collect_leaves(first, result);
            typed_collect_leaves(second, result);
        }
    }
}

fn typed_leaf_rect(node: &TypedNode, target: LayoutLeaf, area: Rect) -> Option<Rect> {
    match node {
        TypedNode::Leaf(leaf) => (*leaf == target).then_some(area),
        TypedNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (first_area, second_area) = split_rect(area, *direction, *ratio);
            typed_leaf_rect(first, target, first_area)
                .or_else(|| typed_leaf_rect(second, target, second_area))
        }
    }
}

fn typed_collect_panes(
    node: &TypedNode,
    area: Rect,
    focus: LayoutLeaf,
    result: &mut Vec<PaneInfo>,
) {
    match node {
        TypedNode::Leaf(LayoutLeaf::Pane(id)) => result.push(PaneInfo {
            id: *id,
            rect: area,
            inner_rect: area,
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: focus == LayoutLeaf::Pane(*id),
        }),
        TypedNode::Leaf(LayoutLeaf::Collection(_)) => {}
        TypedNode::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let (first_area, second_area) = split_rect(area, *direction, *ratio);
            typed_collect_panes(first, first_area, focus, result);
            typed_collect_panes(second, second_area, focus, result);
        }
    }
}

fn typed_collect_splits(
    node: &TypedNode,
    area: Rect,
    path: Vec<bool>,
    result: &mut Vec<SplitBorder>,
) {
    if let TypedNode::Split {
        direction,
        ratio,
        first,
        second,
    } = node
    {
        let (first_area, second_area) = split_rect(area, *direction, *ratio);
        let pos = match direction {
            Direction::Horizontal => first_area.x + first_area.width,
            Direction::Vertical => first_area.y + first_area.height,
        };
        result.push(SplitBorder {
            pos,
            direction: *direction,
            ratio: *ratio,
            area,
            path: path.clone(),
        });
        let mut first_path = path.clone();
        first_path.push(false);
        typed_collect_splits(first, first_area, first_path, result);
        let mut second_path = path;
        second_path.push(true);
        typed_collect_splits(second, second_area, second_path, result);
    }
}

fn typed_split_at(
    node: TypedNode,
    target: LayoutLeaf,
    direction: Direction,
    new_leaf: LayoutLeaf,
    ratio: f32,
) -> TypedNode {
    match node {
        TypedNode::Leaf(leaf) if leaf == target => TypedNode::Split {
            direction,
            ratio,
            first: Box::new(TypedNode::Leaf(leaf)),
            second: Box::new(TypedNode::Leaf(new_leaf)),
        },
        TypedNode::Leaf(_) => node,
        TypedNode::Split {
            direction: existing_direction,
            ratio: existing_ratio,
            first,
            second,
        } => TypedNode::Split {
            direction: existing_direction,
            ratio: existing_ratio,
            first: Box::new(typed_split_at(*first, target, direction, new_leaf, ratio)),
            second: Box::new(typed_split_at(*second, target, direction, new_leaf, ratio)),
        },
    }
}

fn typed_remove(node: TypedNode, target: LayoutLeaf) -> Option<TypedNode> {
    match node {
        TypedNode::Leaf(leaf) if leaf == target => None,
        TypedNode::Leaf(_) => Some(node),
        TypedNode::Split {
            direction,
            ratio,
            first,
            second,
        } => match (typed_remove(*first, target), typed_remove(*second, target)) {
            (None, Some(node)) | (Some(node), None) => Some(node),
            (Some(first), Some(second)) => Some(TypedNode::Split {
                direction,
                ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (None, None) => None,
        },
    }
}

fn typed_swap_panes(node: &mut TypedNode, first: PaneId, second: PaneId) {
    match node {
        TypedNode::Leaf(LayoutLeaf::Pane(id)) if *id == first => *id = second,
        TypedNode::Leaf(LayoutLeaf::Pane(id)) if *id == second => *id = first,
        TypedNode::Leaf(_) => {}
        TypedNode::Split {
            first: first_node,
            second: second_node,
            ..
        } => {
            typed_swap_panes(first_node, first, second);
            typed_swap_panes(second_node, first, second);
        }
    }
}

fn typed_split_ratios(node: &TypedNode) -> Vec<(Vec<bool>, f32)> {
    fn collect(node: &TypedNode, path: &mut Vec<bool>, result: &mut Vec<(Vec<bool>, f32)>) {
        match node {
            TypedNode::Leaf(_) => {}
            TypedNode::Split {
                ratio,
                first,
                second,
                ..
            } => {
                result.push((path.clone(), *ratio));
                path.push(false);
                collect(first, path, result);
                path.pop();
                path.push(true);
                collect(second, path, result);
                path.pop();
            }
        }
    }
    let mut result = Vec::new();
    collect(node, &mut Vec::new(), &mut result);
    result
}

fn typed_set_ratio(node: &mut TypedNode, path: &[bool], ratio: f32) -> bool {
    if let TypedNode::Split {
        first,
        second,
        ratio: current,
        ..
    } = node
    {
        if path.is_empty() {
            *current = ratio;
            true
        } else if path[0] {
            typed_set_ratio(second, &path[1..], ratio)
        } else {
            typed_set_ratio(first, &path[1..], ratio)
        }
    } else {
        false
    }
}

fn typed_get_ratio(node: &TypedNode, path: &[bool]) -> Option<f32> {
    if let TypedNode::Split {
        first,
        second,
        ratio,
        ..
    } = node
    {
        if path.is_empty() {
            Some(*ratio)
        } else if path[0] {
            typed_get_ratio(second, &path[1..])
        } else {
            typed_get_ratio(first, &path[1..])
        }
    } else {
        None
    }
}

// --- Directional pane navigation ---

/// Find the nearest pane in the given direction from `focused`.
pub fn find_in_direction(
    focused: &PaneInfo,
    direction: NavDirection,
    panes: &[PaneInfo],
) -> Option<PaneId> {
    let fr = focused.rect;

    panes
        .iter()
        .enumerate()
        .filter(|(_, p)| p.id != focused.id)
        .filter(|(_, p)| {
            let r = p.rect;
            match direction {
                NavDirection::Left => {
                    r.x + r.width <= fr.x && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Right => {
                    r.x >= fr.x + fr.width && ranges_overlap(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up => {
                    r.y + r.height <= fr.y && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
                NavDirection::Down => {
                    r.y >= fr.y + fr.height && ranges_overlap(r.x, r.width, fr.x, fr.width)
                }
            }
        })
        .min_by_key(|(index, p)| {
            let r = p.rect;
            let edge_distance = match direction {
                NavDirection::Left => fr.x.saturating_sub(r.x + r.width),
                NavDirection::Right => r.x.saturating_sub(fr.x + fr.width),
                NavDirection::Up => fr.y.saturating_sub(r.y + r.height),
                NavDirection::Down => r.y.saturating_sub(fr.y + fr.height),
            };
            let overlap = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_overlap_amount(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_overlap_amount(r.x, r.width, fr.x, fr.width)
                }
            };
            let center_distance = match direction {
                NavDirection::Left | NavDirection::Right => {
                    range_center_distance(r.y, r.height, fr.y, fr.height)
                }
                NavDirection::Up | NavDirection::Down => {
                    range_center_distance(r.x, r.width, fr.x, fr.width)
                }
            };
            (edge_distance, Reverse(overlap), center_distance, *index)
        })
        .map(|(_, p)| p.id)
}

fn ranges_overlap(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> bool {
    a_start < b_start + b_len && a_start + a_len > b_start
}

fn split_on_requested_edge(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    split_edge_distance(split, focused, nav) <= 1
}

fn split_area_overlaps_focused_pane(split: &SplitBorder, focused: Rect, nav: NavDirection) -> bool {
    match nav {
        NavDirection::Left | NavDirection::Right => {
            ranges_overlap(split.area.y, split.area.height, focused.y, focused.height)
        }
        NavDirection::Up | NavDirection::Down => {
            ranges_overlap(split.area.x, split.area.width, focused.x, focused.width)
        }
    }
}

fn nearest_resize_split(
    splits: &[SplitBorder],
    target_dir: Direction,
    focused: Rect,
    nav: NavDirection,
) -> Option<&SplitBorder> {
    splits
        .iter()
        .filter(|s| s.direction == target_dir)
        .filter(|s| split_area_overlaps_focused_pane(s, focused, nav))
        .filter(|s| split_on_requested_edge(s, focused, nav))
        .min_by_key(|s| split_edge_distance(s, focused, nav))
}

fn opposite_direction(nav: NavDirection) -> NavDirection {
    match nav {
        NavDirection::Left => NavDirection::Right,
        NavDirection::Right => NavDirection::Left,
        NavDirection::Up => NavDirection::Down,
        NavDirection::Down => NavDirection::Up,
    }
}

fn split_edge_distance(split: &SplitBorder, focused: Rect, nav: NavDirection) -> u32 {
    match nav {
        NavDirection::Left => (split.pos as i32 - focused.x as i32).unsigned_abs(),
        NavDirection::Right => {
            (split.pos as i32 - (focused.x + focused.width) as i32).unsigned_abs()
        }
        NavDirection::Up => (split.pos as i32 - focused.y as i32).unsigned_abs(),
        NavDirection::Down => {
            (split.pos as i32 - (focused.y + focused.height) as i32).unsigned_abs()
        }
    }
}

fn range_overlap_amount(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_end = a_start.saturating_add(a_len);
    let b_end = b_start.saturating_add(b_len);
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

fn range_center_distance(a_start: u16, a_len: u16, b_start: u16, b_len: u16) -> u16 {
    let a_center = a_start.saturating_mul(2).saturating_add(a_len);
    let b_center = b_start.saturating_mul(2).saturating_add(b_len);
    a_center.abs_diff(b_center)
}

// --- Tree operations ---

fn valid_split_ratio(ratio: f32) -> f32 {
    if ratio.is_finite() {
        ratio.clamp(0.1, 0.9)
    } else {
        0.5
    }
}

fn split_rect(area: Rect, direction: Direction, ratio: f32) -> (Rect, Rect) {
    match direction {
        Direction::Horizontal => {
            let first_w = ((area.width as f32) * ratio).round() as u16;
            let second_w = area.width.saturating_sub(first_w);
            (
                Rect::new(area.x, area.y, first_w, area.height),
                Rect::new(area.x + first_w, area.y, second_w, area.height),
            )
        }
        Direction::Vertical => {
            let first_h = ((area.height as f32) * ratio).round() as u16;
            let second_h = area.height.saturating_sub(first_h);
            (
                Rect::new(area.x, area.y, area.width, first_h),
                Rect::new(area.x, area.y + first_h, area.width, second_h),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u32) -> PaneId {
        PaneId::from_raw(id)
    }

    fn sample_layout() -> TileLayout {
        TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.3,
                first: Box::new(Node::Pane(pane(1))),
                second: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.6,
                    first: Box::new(Node::Pane(pane(2))),
                    second: Box::new(Node::Split {
                        direction: Direction::Horizontal,
                        ratio: 0.4,
                        first: Box::new(Node::Pane(pane(3))),
                        second: Box::new(Node::Pane(pane(4))),
                    }),
                }),
            },
            pane(2),
        )
    }

    fn pane_rects(layout: &TileLayout) -> Vec<(PaneId, Rect)> {
        layout
            .panes(Rect::new(0, 0, 100, 40))
            .into_iter()
            .map(|info| (info.id, info.rect))
            .collect()
    }

    fn pane_rect(layout: &TileLayout, pane_id: PaneId) -> Rect {
        pane_rects(layout)
            .into_iter()
            .find_map(|(id, rect)| (id == pane_id).then_some(rect))
            .expect("pane should exist")
    }

    fn split_snapshot(layout: &TileLayout) -> Vec<(Direction, f32)> {
        fn collect(node: &Node, out: &mut Vec<(Direction, f32)>) {
            match node {
                Node::Pane(_) => {}
                Node::Split {
                    direction,
                    ratio,
                    first,
                    second,
                } => {
                    out.push((*direction, *ratio));
                    collect(first, out);
                    collect(second, out);
                }
            }
        }

        let mut out = Vec::new();
        collect(layout.root(), &mut out);
        out
    }

    #[test]
    fn pane_only_layout_characterization_preserves_leaf_order_geometry_and_focus() {
        let layout = sample_layout();

        assert_eq!(
            layout.leaves(),
            vec![
                LayoutLeaf::Pane(pane(1)),
                LayoutLeaf::Pane(pane(2)),
                LayoutLeaf::Pane(pane(3)),
                LayoutLeaf::Pane(pane(4)),
            ]
        );
        assert_eq!(layout.focused_leaf(), LayoutLeaf::Pane(pane(2)));
        assert_eq!(layout.focused(), pane(2));
        assert_eq!(pane_rect(&layout, pane(1)), Rect::new(0, 0, 30, 40));
        assert_eq!(pane_rect(&layout, pane(2)), Rect::new(30, 0, 70, 24));
    }

    #[test]
    fn creating_collection_does_not_allocate_or_change_pane_ids() {
        let (mut layout, root) = TileLayout::new();
        let collection = CollectionId::from_raw(9_000_001).expect("valid collection id");
        let pane_ids_before = layout.pane_ids();

        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Horizontal,
            0.5,
        ));

        assert_eq!(layout.pane_ids(), pane_ids_before);
        assert_eq!(
            layout.leaves(),
            vec![LayoutLeaf::Pane(root), LayoutLeaf::Collection(collection)]
        );
        assert_eq!(layout.focused_leaf(), LayoutLeaf::Pane(root));
        assert_eq!(layout.panes(Rect::new(0, 0, 80, 20)).len(), 1);
    }

    #[test]
    fn collection_ids_are_distinct_and_restored_ids_reserve_allocator_space() {
        let restored = CollectionId::from_raw(9_100_001).expect("valid restored id");
        let allocated = CollectionId::alloc().expect("collection id available");
        assert_ne!(restored, allocated);
        assert!(allocated.raw() > restored.raw());
    }

    #[test]
    fn untrusted_collection_id_parsing_does_not_reserve_allocator_space() {
        let before = CollectionId::alloc().expect("collection ID available");
        let parsed = CollectionId::parse("collection_18446744073709551614")
            .expect("near-max public ID parses");
        let after = CollectionId::alloc().expect("untrusted lookup must not exhaust allocation");

        assert!(parsed.raw() > after.raw());
        assert!(after.raw() > before.raw());
    }

    #[test]
    fn collection_id_serde_is_prefixed_string_and_rejects_invalid_values() {
        let id = CollectionId::from_raw(42).expect("valid collection id");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"collection_42\"");
        assert_eq!(
            serde_json::from_str::<CollectionId>("\"collection_42\"").unwrap(),
            id
        );
        assert!(serde_json::from_str::<CollectionId>("42").is_err());
        assert!(serde_json::from_str::<CollectionId>("\"collection_0\"").is_err());
        assert!(CollectionId::from_raw(0).is_err());
        assert!(CollectionId::from_raw(u64::MAX).is_err());
    }

    #[test]
    fn grouped_focus_and_enumeration_resolve_selected_member() {
        let (mut layout, root) = TileLayout::new();
        let child = layout.split_focused(Direction::Horizontal);
        let collection = CollectionId::from_raw(9_200_001).expect("valid collection id");
        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Vertical,
            0.5,
        ));
        assert!(layout.remove_tiled_pane_for_collection(child));
        assert!(layout.add_collection_member(collection, child));

        layout.focus_pane(child);

        assert_eq!(layout.focused_leaf(), LayoutLeaf::Collection(collection));
        assert_eq!(layout.focused(), child);
        assert_eq!(layout.pane_ids(), vec![root, child]);
        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.tiled_pane_ids(), vec![root]);
        assert_eq!(layout.tiled_pane_count(), 1);
        assert_eq!(
            layout.collection(collection).unwrap().selected(),
            Some(child)
        );
    }

    #[test]
    fn collection_only_legacy_root_projects_selected_member_without_stale_tiled_leaf() {
        let (mut layout, root) = TileLayout::new();
        let collection = CollectionId::from_raw(9_300_001).expect("valid collection id");
        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Horizontal,
            0.5,
        ));
        assert!(layout.remove_tiled_pane_for_collection(root));
        assert!(layout.add_collection_member(collection, root));
        assert!(layout.focus_leaf(LayoutLeaf::Collection(collection)));

        assert!(layout.tiled_pane_ids().is_empty());
        assert_eq!(layout.pane_ids(), vec![root]);
        assert_eq!(layout.focused(), root);
        assert!(matches!(layout.root(), Node::Pane(id) if *id == root));
    }

    #[test]
    fn focus_pane_selects_requested_member_inside_focused_collection() {
        let (mut layout, root) = TileLayout::new();
        let second = layout.split_focused(Direction::Horizontal);
        let collection = CollectionId::from_raw(9_400_001).expect("valid collection id");
        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Vertical,
            0.5,
        ));
        assert!(layout.remove_tiled_pane_for_collection(root));
        assert!(layout.add_collection_member(collection, root));
        assert!(layout.remove_tiled_pane_for_collection(second));
        assert!(layout.add_collection_member(collection, second));

        layout.focus_pane(second);

        assert_eq!(layout.focused_leaf(), LayoutLeaf::Collection(collection));
        assert_eq!(layout.focused(), second);
        assert_eq!(
            layout.collection(collection).unwrap().selected(),
            Some(second)
        );
        assert!(matches!(layout.root(), Node::Pane(id) if *id == second));
    }

    #[test]
    fn resizing_focused_collection_resizes_its_top_level_bsp_leaf() {
        let (mut layout, root) = TileLayout::new();
        let collection = CollectionId::from_raw(9_500_001).expect("valid collection id");
        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Horizontal,
            0.5,
        ));
        assert!(layout.focus_leaf(LayoutLeaf::Collection(collection)));

        layout.resize_focused(NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40));

        let splits = layout.splits(Rect::new(0, 0, 100, 40));
        assert_eq!(splits.len(), 1);
        assert!((splits[0].ratio - 0.45).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_member_targets_its_collection_leaf_and_restores_focus() {
        let (mut layout, root) = TileLayout::new();
        let member = layout.split_focused(Direction::Vertical);
        let collection = CollectionId::from_raw(9_600_001).expect("valid collection id");
        assert!(layout.insert_collection_near(
            LayoutLeaf::Pane(root),
            collection,
            Direction::Horizontal,
            0.5,
        ));
        assert!(layout.remove_tiled_pane_for_collection(member));
        assert!(layout.add_collection_member(collection, member));
        let previous_focus = layout.focused_leaf();

        assert!(layout.resize_pane(member, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        assert_eq!(layout.focused_leaf(), previous_focus);
        let splits = layout.splits(Rect::new(0, 0, 100, 40));
        assert!((splits[0].ratio - 0.45).abs() < f32::EPSILON);
    }

    #[test]
    fn swap_panes_exchanges_leaf_ids_without_changing_cells() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);

        assert!(layout.swap_panes(pane(2), pane(4)));

        assert_eq!(layout.pane_count(), 4);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), pane(2));

        let after_rects = pane_rects(&layout);
        assert_eq!(after_rects[0], before_rects[0]);
        assert_eq!(after_rects[1], (pane(4), before_rects[1].1));
        assert_eq!(after_rects[2], before_rects[2]);
        assert_eq!(after_rects[3], (pane(2), before_rects[3].1));
    }

    #[test]
    fn swap_panes_is_noop_for_same_or_missing_pane() {
        let mut layout = sample_layout();
        let before_rects = pane_rects(&layout);
        let before_splits = split_snapshot(&layout);
        let before_focus = layout.focused();

        assert!(!layout.swap_panes(pane(2), pane(2)));
        assert!(!layout.swap_panes(pane(2), pane(99)));
        assert!(!layout.swap_panes(pane(99), pane(2)));

        assert_eq!(pane_rects(&layout), before_rects);
        assert_eq!(split_snapshot(&layout), before_splits);
        assert_eq!(layout.focused(), before_focus);
    }

    #[test]
    fn insert_existing_pane_near_target_preserves_existing_ids_and_focuses_moved_pane() {
        let (mut layout, root) = TileLayout::new();
        let moved = pane(99);

        assert!(layout.insert_pane_near(root, moved, Direction::Horizontal, 0.25));

        assert_eq!(layout.pane_count(), 2);
        assert_eq!(layout.pane_ids(), vec![root, moved]);
        assert_eq!(layout.focused(), moved);
        let splits = split_snapshot(&layout);
        assert_eq!(splits, vec![(Direction::Horizontal, 0.25)]);
        assert_eq!(pane_rect(&layout, root), Rect::new(0, 0, 25, 40));
        assert_eq!(pane_rect(&layout, moved), Rect::new(25, 0, 75, 40));
    }

    #[test]
    fn split_focused_with_ratio_sets_new_split_ratio() {
        let (mut layout, root) = TileLayout::new();
        layout.focus_pane(root);

        layout.split_focused_with_ratio(Direction::Horizontal, 0.333);

        let splits = split_snapshot(&layout);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.333).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_pane_preserves_focus_and_reports_change() {
        let mut layout = sample_layout();
        let original_focus = layout.focused();

        assert!(layout.resize_pane(pane(1), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        assert_eq!(layout.focused(), original_focus);
        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.35).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_second_child_toward_split_decreases_ratio() {
        let (mut layout, root) = TileLayout::new();
        let right = layout.split_focused(Direction::Horizontal);
        layout.focus_pane(root);

        assert!(layout.resize_pane(right, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let split = split_snapshot(&layout)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);
        assert_eq!(layout.focused(), root);
    }

    #[test]
    fn resize_outer_edges_shrink_focused_pane() {
        let (mut horizontal, left) = TileLayout::new();
        horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(left, NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut horizontal, _left) = TileLayout::new();
        let right = horizontal.split_focused(Direction::Horizontal);

        assert!(horizontal.resize_pane(right, NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&horizontal)[0];
        assert_eq!(split.0, Direction::Horizontal);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);

        let (mut vertical, top) = TileLayout::new();
        vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(top, NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.45).abs() < f32::EPSILON);

        let (mut vertical, _top) = TileLayout::new();
        let bottom = vertical.split_focused(Direction::Vertical);

        assert!(vertical.resize_pane(bottom, NavDirection::Down, 0.05, Rect::new(0, 0, 100, 40),));
        let split = split_snapshot(&vertical)[0];
        assert_eq!(split.0, Direction::Vertical);
        assert!((split.1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn resize_outer_edge_falls_back_to_horizontal_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Horizontal,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Vertical,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Left, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.height, before.height);
        assert!(after.width < before.width);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Horizontal);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Vertical, 0.5));
    }

    #[test]
    fn resize_outer_edge_falls_back_to_vertical_ancestor_split() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.6,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Pane(pane(3))),
            },
            pane(1),
        );
        let before = pane_rect(&layout, pane(1));

        assert!(layout.resize_pane(pane(1), NavDirection::Up, 0.05, Rect::new(0, 0, 100, 40),));

        let after = pane_rect(&layout, pane(1));
        assert_eq!(after.width, before.width);
        assert!(after.height < before.height);
        let splits = split_snapshot(&layout);
        assert_eq!(splits[0].0, Direction::Vertical);
        assert!((splits[0].1 - 0.55).abs() < f32::EPSILON);
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
    }

    #[test]
    fn resize_uses_split_in_same_branch_when_borders_share_coordinate() {
        let mut layout = TileLayout::from_saved(
            Node::Split {
                direction: Direction::Vertical,
                ratio: 0.5,
                first: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(1))),
                    second: Box::new(Node::Pane(pane(2))),
                }),
                second: Box::new(Node::Split {
                    direction: Direction::Horizontal,
                    ratio: 0.5,
                    first: Box::new(Node::Pane(pane(3))),
                    second: Box::new(Node::Pane(pane(4))),
                }),
            },
            pane(3),
        );

        assert!(layout.resize_pane(pane(3), NavDirection::Right, 0.05, Rect::new(0, 0, 100, 40),));

        let splits = split_snapshot(&layout);
        assert_eq!(splits[0], (Direction::Vertical, 0.5));
        assert_eq!(splits[1], (Direction::Horizontal, 0.5));
        assert_eq!(splits[2].0, Direction::Horizontal);
        assert!((splits[2].1 - 0.55).abs() < f32::EPSILON);
    }

    #[test]
    fn find_in_direction_tiebreaks_by_larger_overlap_before_layout_order() {
        let focused = PaneInfo {
            id: pane(1),
            rect: Rect::new(10, 10, 10, 10),
            inner_rect: Rect::new(10, 10, 10, 10),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: true,
        };
        let small_overlap_first = PaneInfo {
            id: pane(2),
            rect: Rect::new(0, 10, 10, 2),
            inner_rect: Rect::new(0, 10, 10, 2),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
        };
        let larger_overlap_second = PaneInfo {
            id: pane(3),
            rect: Rect::new(0, 10, 10, 8),
            inner_rect: Rect::new(0, 10, 10, 8),
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: false,
        };
        let panes = vec![focused.clone(), small_overlap_first, larger_overlap_second];

        assert_eq!(
            find_in_direction(&focused, NavDirection::Left, &panes),
            Some(pane(3))
        );
    }
}
