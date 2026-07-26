//! Foreground-client presentation state and computed geometry for pane collections.
//!
//! The server currently has one foreground `AppState`, so this state follows that
//! client. Observer renders do not resize terminals. Moving this map to connection
//! state remains necessary for independent simultaneous full-app clients.

use crate::{
    app::state::AppState,
    layout::{CollectionId, LayoutLeaf, PaneId},
};
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};

use crate::terminal::TerminalId;

pub(crate) const DEFAULT_PREVIEW_HEIGHT: u16 = 8;
pub(crate) const MIN_PREVIEW_HEIGHT: u16 = 3;
pub(crate) const MAX_PREVIEW_HEIGHT: u16 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalGeometry {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) cell_width_px: u32,
    pub(crate) cell_height_px: u32,
}

pub(crate) type CollectionGeometryProjection = HashMap<TerminalId, TerminalGeometry>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCollectionGroupClose {
    pub(crate) workspace_id: String,
    pub(crate) worktree_key: String,
    pub(crate) workspace_member_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingCollectionClose {
    /// Stable origin identities keep the prompt independent of shared foreground navigation.
    pub(crate) workspace_id: String,
    pub(crate) tab_id: String,
    pub(crate) collection_id: CollectionId,
    /// Exact destructive target captured when the prompt opened.
    pub(crate) member_ids: Vec<PaneId>,
    pub(crate) collection_revision: u64,
    /// Second-stage confirmation remains attached to the originating client.
    pub(crate) group_close: Option<PendingCollectionGroupClose>,
    /// Close only archived members, leaving the collection and active members intact.
    pub(crate) cleanup_archive: bool,
    pub(crate) active: usize,
    pub(crate) archived: usize,
    pub(crate) live: usize,
    pub(crate) exited: usize,
    pub(crate) working: usize,
    pub(crate) blocked: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CollectionInteractionMode {
    #[default]
    List,
    Terminal,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CollectionViewState {
    pub(crate) mode: CollectionInteractionMode,
    /// Member explicitly entered by a foreground human. Shared/API selection alone cannot set it.
    pub(crate) entered: Option<PaneId>,
    pub(crate) scroll: usize,
    pub(crate) expanded: HashSet<PaneId>,
    pub(crate) preview_heights: HashMap<PaneId, u16>,
    pub(crate) maximized: Option<PaneId>,
    pub(crate) resize_drag: Option<(PaneId, u16, u16)>,
    pub(crate) row_drag: Option<(PaneId, u16)>,
}
impl CollectionViewState {
    pub(crate) fn preview_height(&self, pane_id: PaneId) -> u16 {
        self.preview_heights
            .get(&pane_id)
            .copied()
            .unwrap_or(DEFAULT_PREVIEW_HEIGHT)
            .clamp(MIN_PREVIEW_HEIGHT, MAX_PREVIEW_HEIGHT)
    }
    pub(crate) fn set_preview_height(&mut self, pane_id: PaneId, height: u16) {
        self.preview_heights.insert(
            pane_id,
            height.clamp(MIN_PREVIEW_HEIGHT, MAX_PREVIEW_HEIGHT),
        );
    }
    pub(crate) fn retain_members(&mut self, members: &[PaneId]) {
        let members: HashSet<_> = members.iter().copied().collect();
        self.expanded.retain(|pane| members.contains(pane));
        self.preview_heights
            .retain(|pane, _| members.contains(pane));
        if self.maximized.is_some_and(|pane| !members.contains(&pane)) {
            self.maximized = None;
            self.mode = CollectionInteractionMode::List;
        }
        if self.entered.is_some_and(|pane| !members.contains(&pane)) {
            self.entered = None;
            self.mode = CollectionInteractionMode::List;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionSection {
    Active,
    Archived,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CollectionHitKind {
    Row,
    Disclosure,
    Preview,
    ResizeHandle,
    Chrome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CollectionHitRegion {
    pub(crate) collection_id: CollectionId,
    pub(crate) pane_id: Option<PaneId>,
    pub(crate) kind: CollectionHitKind,
    /// Visible, clipped hit rectangle.
    pub(crate) rect: Rect,
    /// Terminal row represented by the top of `rect` for clipped previews.
    pub(crate) terminal_row_offset: u16,
}
#[derive(Debug, Clone)]
pub(crate) struct CollectionRowView {
    pub(crate) pane_id: PaneId,
    pub(crate) section: CollectionSection,
    pub(crate) depth: usize,
    pub(crate) external_parent: bool,
    pub(crate) virtual_y: usize,
    pub(crate) row_rect: Rect,
    /// Visible, clipped render/hit rectangle.
    pub(crate) preview_rect: Option<Rect>,
    /// Full logical PTY preview dimensions, independent of viewport clipping.
    pub(crate) preview_size: Option<(u16, u16)>,
    /// First logical terminal row shown in `preview_rect`.
    pub(crate) preview_row_offset: u16,
    pub(crate) resize_rect: Option<Rect>,
}
#[derive(Debug, Clone)]
pub(crate) struct CollectionLayout {
    pub(crate) id: CollectionId,
    pub(crate) rect: Rect,
    pub(crate) inner_rect: Rect,
    pub(crate) active_header: Option<Rect>,
    pub(crate) archive_header: Option<Rect>,
    pub(crate) rows: Vec<CollectionRowView>,
    pub(crate) hits: Vec<CollectionHitRegion>,
    pub(crate) content_height: usize,
    pub(crate) viewport_height: usize,
    pub(crate) scroll: usize,
    pub(crate) maximized: Option<PaneId>,
}
impl CollectionLayout {
    pub(crate) fn hit_at(&self, x: u16, y: u16) -> Option<CollectionHitRegion> {
        self.hits
            .iter()
            .rev()
            .find(|hit| contains(hit.rect, x, y))
            .copied()
    }
}
impl AppState {
    pub(crate) fn focused_collection_id(&self) -> Option<CollectionId> {
        let ws = self.active.and_then(|idx| self.workspaces.get(idx))?;
        match ws.layout.focused_leaf() {
            LayoutLeaf::Collection(id) => Some(id),
            LayoutLeaf::Pane(_) => None,
        }
    }

    pub(crate) fn enter_collection_terminal_from_foreground(
        &mut self,
        ws_idx: usize,
        collection_id: CollectionId,
        pane_id: PaneId,
    ) -> bool {
        let valid = self
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.active_tab())
            .is_some_and(|tab| {
                tab.layout.focused_leaf() == LayoutLeaf::Collection(collection_id)
                    && tab
                        .collection(collection_id)
                        .and_then(|collection| collection.selected())
                        == Some(pane_id)
            });
        if !valid {
            return false;
        }
        let view = self.collection_views.entry(collection_id).or_default();
        view.expanded.insert(pane_id);
        view.mode = CollectionInteractionMode::Terminal;
        view.entered = Some(pane_id);
        let Some(pane) = self
            .workspaces
            .get_mut(ws_idx)
            .and_then(|ws| ws.active_tab_mut())
            .and_then(|tab| tab.panes.get_mut(&pane_id))
        else {
            return false;
        };
        let changed = !pane.seen;
        pane.seen = true;
        changed
    }

    pub(crate) fn focused_collection_terminal_entered(&self) -> bool {
        let Some(id) = self.focused_collection_id() else {
            return true;
        };
        let selected = self
            .active
            .and_then(|idx| self.workspaces.get(idx))
            .and_then(|ws| ws.active_tab())
            .and_then(|tab| tab.collection(id))
            .and_then(|collection| collection.selected());
        self.collection_views.get(&id).is_some_and(|view| {
            view.mode == CollectionInteractionMode::Terminal && view.entered == selected
        })
    }
}

pub(crate) fn contains(rect: Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_heights_are_bounded_and_stale_members_are_removed() {
        let first = PaneId::from_raw(1);
        let stale = PaneId::from_raw(2);
        let mut state = CollectionViewState::default();
        state.expanded.extend([first, stale]);
        state.set_preview_height(first, 1);
        state.maximized = Some(stale);
        state.mode = CollectionInteractionMode::Terminal;
        state.entered = Some(stale);
        state.retain_members(&[first]);
        assert_eq!(state.preview_height(first), MIN_PREVIEW_HEIGHT);
        assert!(!state.expanded.contains(&stale));
        assert_eq!(state.maximized, None);
        assert_eq!(state.mode, CollectionInteractionMode::List);
        assert_eq!(state.entered, None);
    }
}
