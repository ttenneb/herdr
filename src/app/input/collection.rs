use crossterm::event::{
    KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::{layout::Rect, widgets::Borders};

use crate::{
    api::schema::{
        CollectionCloseDisposition, CollectionCloseParams, CollectionMemberTarget,
        CollectionPromoteParams, CollectionReorderParams, CollectionSelectParams,
        DelegationReorderParams, DelegationSiblingPosition, Method, SplitDirection,
    },
    app::{
        collection_view::{
            automatic_preview_height, CollectionHitKind, CollectionInteractionMode,
            DEFAULT_PREVIEW_HEIGHT, MIN_PREVIEW_HEIGHT,
        },
        state::{ContextMenuKind, ContextMenuState, MenuListState, Mode},
        App, InputSourceId,
    },
    input::TerminalKey,
    layout::{CollectionId, LayoutLeaf, PaneId, PaneInfo},
};

use super::PaneUrlClickTarget;

impl App {
    fn focused_collection(&self) -> Option<(usize, CollectionId, Option<PaneId>)> {
        let ws_idx = self.state.active?;
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.active_tab()?;
        let LayoutLeaf::Collection(id) = tab.layout.focused_leaf() else {
            return None;
        };
        Some((
            ws_idx,
            id,
            tab.collection(id)
                .and_then(|collection| collection.selected()),
        ))
    }

    pub(crate) fn collection_accepts_terminal_input(&self) -> bool {
        let Some((_, collection_id, selected)) = self.focused_collection() else {
            return true;
        };
        self.state
            .collection_views
            .get(&collection_id)
            .is_some_and(|view| view.terminal_entered(selected))
    }

    pub(crate) fn exit_focused_collection_terminal(&mut self) -> bool {
        let Some((_, collection_id, selected)) = self.focused_collection() else {
            return false;
        };
        let Some(view) = self.state.collection_views.get_mut(&collection_id) else {
            return false;
        };
        if !view.terminal_entered(selected) {
            return false;
        }
        view.mode = CollectionInteractionMode::List;
        view.entered = None;
        true
    }

    pub(crate) fn handle_collection_key(&mut self, key: TerminalKey) -> bool {
        let Some((ws_idx, collection_id, selected)) = self.focused_collection() else {
            return false;
        };
        let event = key.as_key_event();
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return true;
        }
        let entered = self
            .state
            .collection_views
            .get(&collection_id)
            .is_some_and(|view| view.terminal_entered(selected));
        if entered {
            // Entered terminals retain ordinary terminal input, including bare Escape. The
            // configured Herdr prefix enters command mode; prefix then Escape returns to the
            // collection list, while prefix then prefix keeps literal-prefix passthrough.
            return false;
        }

        // Preserve Herdr's configured prefix/direct command language while preventing ordinary
        // list-mode keys from leaking into the selected PTY.
        if self.state.is_prefix_key(key)
            || super::terminal_direct_non_indexed_navigation_action(&self.state, key).is_some()
            || super::terminal_direct_indexed_navigation_action(&self.state, key).is_some()
            || super::navigate::command_for_key(
                &self.state,
                key,
                super::navigate::BindingDispatch::Direct,
            )
            .is_some()
        {
            return false;
        }

        match event.code {
            KeyCode::Up | KeyCode::Char('k') if event.modifiers.is_empty() => {
                self.select_relative(collection_id, -1)
            }
            KeyCode::Down | KeyCode::Char('j') if event.modifiers.is_empty() => {
                self.select_relative(collection_id, 1)
            }
            KeyCode::Up if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reorder_relative(collection_id, -1)
            }
            KeyCode::Down if event.modifiers.contains(KeyModifiers::CONTROL) => {
                self.reorder_relative(collection_id, 1)
            }
            KeyCode::Char(' ') => self.toggle_selected_expanded(collection_id),
            KeyCode::Enter => self.enter_collection_terminal(ws_idx, collection_id),
            KeyCode::Char('z') => self.toggle_collection_maximize(collection_id),
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.resize_selected_preview(collection_id, 1)
            }
            KeyCode::Char('-') => self.resize_selected_preview(collection_id, -1),
            KeyCode::Char('a') => self.toggle_selected_archive(ws_idx, collection_id),
            KeyCode::Char('o') => self.promote_selected(ws_idx, collection_id),
            KeyCode::Char('X') => self.open_collection_close_dialog(collection_id, false),
            KeyCode::Char('A') => self.open_collection_close_dialog(collection_id, true),
            KeyCode::Char('x') | KeyCode::Delete => {
                if selected.is_some() {
                    let _ = self.close_focused_pane_via_api_requires_confirmation();
                } else if self.close_collection_via_api(collection_id, None) {
                    self.state.collection_views.remove(&collection_id);
                }
            }
            KeyCode::Esc => {
                if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                    view.maximized = None;
                    view.mode = CollectionInteractionMode::List;
                    view.entered = None;
                }
            }
            _ => return true,
        }
        true
    }

    pub(crate) fn open_collection_close_dialog(
        &mut self,
        collection_id: CollectionId,
        cleanup_archive: bool,
    ) {
        let Some(ws_idx) = self.state.active else {
            return;
        };
        let tab_idx = self.state.workspaces[ws_idx].active_tab;
        self.open_collection_close_dialog_at(ws_idx, tab_idx, collection_id, cleanup_archive);
    }

    pub(crate) fn open_collection_close_dialog_at(
        &mut self,
        ws_idx: usize,
        tab_idx: usize,
        collection_id: CollectionId,
        cleanup_archive: bool,
    ) {
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(tab_id) = self.public_tab_id(ws_idx, tab_idx) else {
            return;
        };
        let Some(collection) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.tabs.get(tab_idx))
            .and_then(|tab| tab.collection(collection_id))
        else {
            return;
        };
        let collection_is_empty = collection.members().is_empty();
        let active = collection.active_members().count();
        let archived = collection.archived_members().count();
        if cleanup_archive && archived == 0 {
            return;
        }
        let counted_members: Vec<_> = if cleanup_archive {
            collection.archived_members().collect()
        } else {
            collection.members().to_vec()
        };
        let mut live = 0;
        let mut working = 0;
        let mut blocked = 0;
        for pane_id in &counted_members {
            let Some(pane) = self.state.workspaces[ws_idx]
                .tabs
                .get(tab_idx)
                .and_then(|tab| tab.panes.get(pane_id))
            else {
                continue;
            };
            if self
                .terminal_runtimes
                .get(&pane.attached_terminal_id)
                .is_some()
            {
                live += 1;
            }
            if let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id) {
                working += usize::from(terminal.state == crate::detect::AgentState::Working);
                blocked += usize::from(terminal.state == crate::detect::AgentState::Blocked);
            }
        }
        self.state.pending_collection_close =
            Some(crate::app::collection_view::PendingCollectionClose {
                workspace_id,
                tab_id,
                collection_id,
                member_ids: counted_members.clone(),
                collection_revision: collection.revision(),
                cleanup_archive,
                active,
                archived,
                live,
                exited: counted_members.len().saturating_sub(live),
                working,
                blocked,
            });
        self.state.mode = Mode::CollectionClose;
        if collection_is_empty && self.close_collection_via_api(collection_id, None) {
            self.state.pending_collection_close = None;
            self.state.collection_views.remove(&collection_id);
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
        }
    }

    fn close_collection_via_api(
        &mut self,
        collection_id: CollectionId,
        disposition: Option<CollectionCloseDisposition>,
    ) -> bool {
        let response = self.dispatch_runtime_mutation(
            "tui.collection.close",
            Method::CollectionClose(CollectionCloseParams {
                collection_id: format!("collection_{}", collection_id.raw()),
                disposition,
                target_pane_id: None,
                focus_promoted: false,
            }),
        );
        Self::runtime_mutation_succeeded(&response, |message| {
            self.show_plugin_action_failure(message)
        })
    }

    fn runtime_mutation_succeeded(response: &str, show_error: impl FnOnce(&str)) -> bool {
        let Some(error) = serde_json::from_str::<serde_json::Value>(response)
            .ok()
            .and_then(|value| value.get("error").cloned())
        else {
            return true;
        };
        let message = error
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| {
                error
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| error.to_string());
        show_error(&message);
        false
    }

    fn pending_collection_origin_index(
        &self,
        pending: &crate::app::collection_view::PendingCollectionClose,
    ) -> Option<(usize, usize)> {
        let ws_idx = self.parse_workspace_id(&pending.workspace_id)?;
        let (tab_ws_idx, tab_idx) = self.parse_tab_id(&pending.tab_id)?;
        (ws_idx == tab_ws_idx).then_some((ws_idx, tab_idx))
    }

    fn pending_collection_matches(
        &self,
        pending: &crate::app::collection_view::PendingCollectionClose,
    ) -> Option<(usize, usize)> {
        let (ws_idx, tab_idx) = self.pending_collection_origin_index(pending)?;
        let collection = self
            .state
            .workspaces
            .get(ws_idx)?
            .tabs
            .get(tab_idx)?
            .collection(pending.collection_id)?;
        let members = if pending.cleanup_archive {
            collection.archived_members().collect::<Vec<_>>()
        } else {
            collection.members().to_vec()
        };
        (collection.revision() == pending.collection_revision && members == pending.member_ids)
            .then_some((ws_idx, tab_idx))
    }

    fn reopen_pending_collection_close(
        &mut self,
        pending: &crate::app::collection_view::PendingCollectionClose,
    ) {
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        let Some((ws_idx, tab_idx)) = self.pending_collection_origin_index(pending) else {
            return;
        };
        self.open_collection_close_dialog_at(
            ws_idx,
            tab_idx,
            pending.collection_id,
            pending.cleanup_archive,
        );
    }

    pub(crate) fn handle_collection_close_key(&mut self, key: crossterm::event::KeyEvent) {
        let cleanup_archive = self
            .state
            .pending_collection_close
            .as_ref()
            .is_some_and(|pending| pending.cleanup_archive);
        let disposition = match key.code {
            KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Enter if cleanup_archive => {
                Some(CollectionCloseDisposition::CascadeClose)
            }
            KeyCode::Char('c') | KeyCode::Char('C') => {
                Some(CollectionCloseDisposition::CascadeClose)
            }
            KeyCode::Char('p') | KeyCode::Char('P') | KeyCode::Enter if !cleanup_archive => {
                Some(CollectionCloseDisposition::PromoteMembers)
            }
            KeyCode::Esc => {
                self.state.pending_collection_close = None;
                self.state.mode = if self.state.active.is_some() {
                    Mode::Terminal
                } else {
                    Mode::Navigate
                };
                return;
            }
            _ => return,
        };
        let Some(pending) = self.state.pending_collection_close.take() else {
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
            return;
        };
        let Some((ws_idx, _tab_idx)) = self.pending_collection_matches(&pending) else {
            self.reopen_pending_collection_close(&pending);
            return;
        };
        if pending.cleanup_archive {
            for pane_id in pending.member_ids.iter().copied() {
                if let Some(public) = self.public_pane_id(ws_idx, pane_id) {
                    let response = self.dispatch_runtime_mutation(
                        "tui.collection.archive_cleanup",
                        Method::PaneClose(crate::api::schema::PaneTarget { pane_id: public }),
                    );
                    if !Self::runtime_mutation_succeeded(&response, |message| {
                        self.show_plugin_action_failure(message)
                    }) {
                        self.reopen_pending_collection_close(&pending);
                        return;
                    }
                }
            }
        } else if self.close_collection_via_api(pending.collection_id, disposition) {
            self.state.collection_views.remove(&pending.collection_id);
        } else {
            self.reopen_pending_collection_close(&pending);
            return;
        }
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
    }

    pub(crate) fn select_collection_member_via_runtime(
        &mut self,
        ws_idx: usize,
        collection_id: CollectionId,
        pane_id: PaneId,
        focus: bool,
    ) {
        let Some(pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return;
        };
        let _ = self.dispatch_runtime_mutation(
            "tui.collection.select",
            Method::CollectionSelect(CollectionSelectParams {
                collection_id: format!("collection_{}", collection_id.raw()),
                pane_id,
                focus,
            }),
        );
    }

    fn select_relative(&mut self, collection_id: CollectionId, delta: isize) {
        let Some(ws_idx) = self.state.active else {
            return;
        };
        let Some(tab) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.active_tab())
        else {
            return;
        };
        let Some(collection) = tab.collection(collection_id) else {
            return;
        };
        let layout = self
            .state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)
            .cloned();
        let display_order: Vec<_> = layout
            .as_ref()
            .map(|layout| layout.rows.iter().map(|row| row.pane_id).collect())
            .unwrap_or_else(|| collection.members().to_vec());
        if display_order.is_empty() {
            return;
        }
        let current = collection
            .selected()
            .and_then(|pane| {
                display_order
                    .iter()
                    .position(|candidate| *candidate == pane)
            })
            .unwrap_or(0);
        let next = (current as isize + delta)
            .clamp(0, display_order.len().saturating_sub(1) as isize) as usize;
        let pane = display_order[next];
        let target_virtual_y = layout.as_ref().and_then(|layout| {
            layout
                .rows
                .iter()
                .find(|row| row.pane_id == pane)
                .map(|row| (row.virtual_y, layout.viewport_height))
        });
        self.select_collection_member_via_runtime(ws_idx, collection_id, pane, true);
        if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
            view.mode = CollectionInteractionMode::List;
            view.entered = None;
            if let Some((virtual_y, viewport_height)) = target_virtual_y {
                if virtual_y < view.scroll {
                    view.scroll = virtual_y;
                } else if virtual_y >= view.scroll.saturating_add(viewport_height) {
                    view.scroll = virtual_y.saturating_add(1).saturating_sub(viewport_height);
                }
            }
        }
    }

    pub(crate) fn reorder_relative(&mut self, collection_id: CollectionId, delta: isize) {
        let Some(ws_idx) = self.state.active else {
            return;
        };
        let Some(tab) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.active_tab())
        else {
            return;
        };
        let Some(collection) = tab.collection(collection_id) else {
            return;
        };
        let Some(pane) = collection.selected() else {
            return;
        };
        if let Some(record) = self.state.delegations.delegation_for_pane(pane) {
            let mut siblings: Vec<_> = self
                .state
                .delegations
                .records()
                .values()
                .filter(|candidate| candidate.parent_id == record.parent_id)
                .filter_map(|candidate| {
                    candidate
                        .pane_id
                        .filter(|candidate_pane| {
                            collection.members().contains(candidate_pane)
                                && collection.is_archived(*candidate_pane)
                                    == collection.is_archived(pane)
                        })
                        .map(|_| (candidate.sibling_rank, candidate.id))
                })
                .collect();
            siblings.sort_unstable();
            let Some(index) = siblings.iter().position(|(_, id)| *id == record.id) else {
                return;
            };
            let target = (index as isize + delta)
                .clamp(0, siblings.len().saturating_sub(1) as isize)
                as usize;
            if target == index {
                return;
            }
            let anchor = siblings[target].1;
            let position = if delta < 0 {
                DelegationSiblingPosition::Before {
                    delegation_id: anchor.to_string(),
                }
            } else {
                DelegationSiblingPosition::After {
                    delegation_id: anchor.to_string(),
                }
            };
            let _ = self.dispatch_runtime_mutation(
                "tui.delegation.reorder",
                Method::DelegationReorder(DelegationReorderParams {
                    delegation_id: record.id.to_string(),
                    position,
                }),
            );
            return;
        }

        let Some(index) = collection
            .members()
            .iter()
            .position(|candidate| *candidate == pane)
        else {
            return;
        };
        let target = (index as isize + delta)
            .clamp(0, collection.members().len().saturating_sub(1) as isize)
            as usize;
        let Some(pane_id) = self.public_pane_id(ws_idx, pane) else {
            return;
        };
        let _ = self.dispatch_runtime_mutation(
            "tui.collection.reorder",
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: format!("collection_{}", collection_id.raw()),
                pane_id,
                index: target,
            }),
        );
    }

    fn toggle_selected_expanded(&mut self, collection_id: CollectionId) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let view = self
            .state
            .collection_views
            .entry(collection_id)
            .or_default();
        if view.expanded.remove(&pane) {
            if view.entered == Some(pane) {
                view.mode = CollectionInteractionMode::List;
                view.entered = None;
            }
        } else {
            view.expanded.insert(pane);
        }
    }
    fn effective_preview_height(&self, collection_id: CollectionId, pane_id: PaneId) -> u16 {
        if let Some(height) = self
            .state
            .collection_views
            .get(&collection_id)
            .and_then(|view| view.preview_heights.get(&pane_id))
            .copied()
        {
            return height.max(MIN_PREVIEW_HEIGHT);
        }
        self.state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)
            .map(|layout| automatic_preview_height(layout.rect.height))
            .unwrap_or(DEFAULT_PREVIEW_HEIGHT)
    }
    fn resize_selected_preview(&mut self, collection_id: CollectionId, delta: i16) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let current = self.effective_preview_height(collection_id, pane);
        let next = (i32::from(current) + i32::from(delta))
            .clamp(i32::from(MIN_PREVIEW_HEIGHT), i32::from(u16::MAX)) as u16;
        let view = self
            .state
            .collection_views
            .entry(collection_id)
            .or_default();
        view.expanded.insert(pane);
        view.set_preview_height(pane, next);
    }
    pub(crate) fn toggle_collection_maximize(&mut self, collection_id: CollectionId) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let view = self
            .state
            .collection_views
            .entry(collection_id)
            .or_default();
        view.maximized = if view.maximized == Some(pane) {
            None
        } else {
            Some(pane)
        };
    }
    fn enter_collection_terminal(&mut self, ws_idx: usize, collection_id: CollectionId) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        if self
            .state
            .enter_collection_terminal_from_foreground(ws_idx, collection_id, pane)
        {
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
    }
    pub(crate) fn toggle_selected_archive(&mut self, ws_idx: usize, collection_id: CollectionId) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let archived = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.active_tab())
            .and_then(|tab| tab.collection(collection_id))
            .is_some_and(|collection| collection.is_archived(pane));
        let Some(pane_id) = self.public_pane_id(ws_idx, pane) else {
            return;
        };
        let target = CollectionMemberTarget {
            collection_id: format!("collection_{}", collection_id.raw()),
            pane_id,
        };
        let method = if archived {
            Method::CollectionRestore(target)
        } else {
            Method::CollectionArchive(target)
        };
        let _ = self.dispatch_runtime_mutation("tui.collection.archive", method);
    }
    pub(crate) fn promote_selected(&mut self, ws_idx: usize, collection_id: CollectionId) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let Some(target) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|ws| ws.active_tab())
            .and_then(|tab| tab.layout.tiled_pane_ids().into_iter().next())
        else {
            return;
        };
        let (Some(pane_id), Some(target_pane_id)) = (
            self.public_pane_id(ws_idx, pane),
            self.public_pane_id(ws_idx, target),
        ) else {
            return;
        };
        let _ = self.dispatch_runtime_mutation(
            "tui.collection.promote",
            Method::CollectionPromote(CollectionPromoteParams {
                pane_id,
                target_pane_id,
                direction: SplitDirection::Right,
                ratio: Some(0.5),
                focus: true,
            }),
        );
        self.state.collection_views.remove(&collection_id);
    }

    fn collection_child_preview_info(
        &self,
        collection_id: CollectionId,
        pane_id: PaneId,
    ) -> Option<(Rect, u16, u16, u16)> {
        let layout = self
            .state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)?;
        if layout.maximized == Some(pane_id) {
            let rect = layout.maximized_preview_rect.unwrap_or(layout.inner_rect);
            return Some((rect, 0, rect.height, rect.width));
        }
        let row = layout.rows.iter().find(|row| row.pane_id == pane_id)?;
        let (rows, cols) = row.preview_size?;
        Some((row.preview_rect?, row.preview_row_offset, rows, cols))
    }

    fn set_collection_scrollbar_from_row(
        &mut self,
        collection_id: CollectionId,
        screen_row: u16,
        grab_row_offset: Option<u16>,
    ) -> Option<u16> {
        let layout = self
            .state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)?;
        let track = layout.scrollbar_rect?;
        if track.width == 0 || track.height == 0 {
            return None;
        }
        let max_scroll = layout.content_height.saturating_sub(layout.viewport_height);
        if max_scroll == 0 {
            return None;
        }
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_scroll.saturating_sub(layout.scroll.min(max_scroll)),
            max_offset_from_bottom: max_scroll,
            viewport_rows: layout.viewport_height,
        };
        if grab_row_offset.is_none() {
            if let Some(grab) = crate::ui::scrollbar_thumb_grab_offset(metrics, track, screen_row) {
                return Some(grab);
            }
        }
        let offset_from_bottom = if let Some(grab) = grab_row_offset {
            crate::ui::scrollbar_offset_from_drag_row(metrics, track, screen_row, grab)
        } else {
            crate::ui::scrollbar_offset_from_row(metrics, track, screen_row)
        };
        self.state
            .collection_views
            .entry(collection_id)
            .or_default()
            .scroll = max_scroll.saturating_sub(offset_from_bottom);
        None
    }

    fn scroll_collection_child_wheel(
        &mut self,
        ws_idx: usize,
        collection_id: CollectionId,
        pane_id: PaneId,
        mouse: MouseEvent,
        allow_terminal_routing: bool,
    ) {
        self.restore_archived_member_for_input(ws_idx, pane_id);
        let Some((rect, row_offset, _, _)) =
            self.collection_child_preview_info(collection_id, pane_id)
        else {
            return;
        };
        let logical_mouse = MouseEvent {
            column: mouse.column.clamp(rect.x, rect.right().saturating_sub(1)),
            row: mouse.row.saturating_add(row_offset),
            ..mouse
        };
        let info = PaneInfo {
            id: pane_id,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            borders: Borders::NONE,
            is_focused: true,
        };
        if allow_terminal_routing
            && self
                .state
                .forward_pane_wheel(&self.terminal_runtimes, &info, logical_mouse)
        {
            return;
        }
        if let Some(runtime) =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)
        {
            match mouse.kind {
                MouseEventKind::ScrollUp => runtime.scroll_up(self.state.mouse_scroll_lines),
                MouseEventKind::ScrollDown => runtime.scroll_down(self.state.mouse_scroll_lines),
                _ => {}
            }
        }
    }

    fn set_collection_child_scrollbar_from_row(
        &self,
        ws_idx: usize,
        collection_id: CollectionId,
        pane_id: PaneId,
        screen_row: u16,
        grab_row_offset: Option<u16>,
    ) -> Option<u16> {
        let (_, clipped_row_offset, logical_rows, _) =
            self.collection_child_preview_info(collection_id, pane_id)?;
        let layout = self
            .state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)?;
        let visible_track = if layout.maximized == Some(pane_id) {
            layout.maximized_scrollbar_rect?
        } else {
            layout
                .rows
                .iter()
                .find(|row| row.pane_id == pane_id)?
                .preview_scrollbar_rect?
        };
        let runtime =
            self.state
                .runtime_for_pane_in_workspace(&self.terminal_runtimes, ws_idx, pane_id)?;
        let metrics = runtime.scroll_metrics()?;
        let logical_track = Rect::new(0, 0, 1, logical_rows);
        let logical_row = clipped_row_offset.saturating_add(
            screen_row
                .clamp(visible_track.y, visible_track.bottom().saturating_sub(1))
                .saturating_sub(visible_track.y),
        );
        if grab_row_offset.is_none() {
            if let Some(grab) =
                crate::ui::scrollbar_thumb_grab_offset(metrics, logical_track, logical_row)
            {
                return Some(grab);
            }
        }
        let offset = if let Some(grab) = grab_row_offset {
            crate::ui::scrollbar_offset_from_drag_row(metrics, logical_track, logical_row, grab)
        } else {
            crate::ui::scrollbar_offset_from_row(metrics, logical_track, logical_row)
        };
        runtime.set_scroll_offset_from_bottom(offset);
        None
    }

    #[cfg(test)]
    pub(crate) fn handle_collection_mouse(&mut self, mouse: MouseEvent) -> bool {
        self.handle_collection_mouse_from(crate::app::LOCAL_INPUT_SOURCE, mouse)
    }

    pub(crate) fn handle_collection_mouse_from(
        &mut self,
        source_id: InputSourceId,
        mouse: MouseEvent,
    ) -> bool {
        // A passthrough gesture remains owned by the pane where it started. Continue it before
        // collection hit-testing so clipped-preview coordinates survive chrome and off-layout
        // drag/up events.
        if self.state.right_click_passthrough.is_some()
            && self.state.handle_right_click_passthrough(
                &self.terminal_runtimes,
                mouse,
                false,
                None,
            )
        {
            return true;
        }
        // A collection's outer frame belongs to its native menu. Test it before normal hits:
        // maximized previews cover the whole layout and would otherwise hide that frame. This
        // path is deliberately limited to a right-button down, so split-border resize gestures
        // (including their drag/up continuation) retain the ordinary layout handler.
        let outer_chrome_hit = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Right))
            .then(|| {
                self.state
                    .view
                    .collection_layouts
                    .iter()
                    .find_map(|layout| {
                        let in_outer = mouse.column >= layout.rect.x
                            && mouse.column < layout.rect.right()
                            && mouse.row >= layout.rect.y
                            && mouse.row < layout.rect.bottom();
                        let is_outer_frame = layout.maximized.is_some().then(|| {
                            mouse.column == layout.rect.x
                                || mouse.column == layout.rect.right().saturating_sub(1)
                                || mouse.row == layout.rect.y
                                || mouse.row == layout.rect.bottom().saturating_sub(1)
                        });
                        let outside_inner = mouse.column < layout.inner_rect.x
                            || mouse.column >= layout.inner_rect.right()
                            || mouse.row < layout.inner_rect.y
                            || mouse.row >= layout.inner_rect.bottom();
                        (in_outer && is_outer_frame.unwrap_or(outside_inner)).then_some(
                            crate::app::collection_view::CollectionHitRegion {
                                collection_id: layout.id,
                                pane_id: None,
                                kind: CollectionHitKind::Chrome,
                                rect: layout.rect,
                                terminal_row_offset: 0,
                            },
                        )
                    })
            })
            .flatten();
        let hit = outer_chrome_hit.or_else(|| {
            self.state
                .view
                .collection_layouts
                .iter()
                .find_map(|layout| layout.hit_at(mouse.column, mouse.row))
        });
        if let Some((collection_id, grab_row_offset)) = self
            .state
            .collection_views
            .iter()
            .find_map(|(id, view)| view.collection_scrollbar_drag.map(|grab| (*id, grab)))
        {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let _ = self.set_collection_scrollbar_from_row(
                        collection_id,
                        mouse.row,
                        Some(grab_row_offset),
                    );
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        view.collection_scrollbar_drag = None;
                    }
                    return true;
                }
                _ => {}
            }
        }
        if let Some((collection_id, pane, grab_row_offset)) =
            self.state.collection_views.iter().find_map(|(id, view)| {
                view.child_scrollbar_drag
                    .map(|(pane, grab)| (*id, pane, grab))
            })
        {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(ws_idx) = self.state.active {
                        let _ = self.set_collection_child_scrollbar_from_row(
                            ws_idx,
                            collection_id,
                            pane,
                            mouse.row,
                            Some(grab_row_offset),
                        );
                    }
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        view.child_scrollbar_drag = None;
                    }
                    return true;
                }
                _ => {}
            }
        }
        if let Some((collection_id, pane, start_row, start_height)) =
            self.state.collection_views.iter().find_map(|(id, view)| {
                view.resize_drag
                    .map(|(pane, row, height)| (*id, pane, row, height))
            })
        {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let delta = mouse.row as i32 - start_row as i32;
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        let height = (i32::from(start_height) + delta)
                            .clamp(i32::from(MIN_PREVIEW_HEIGHT), i32::from(u16::MAX))
                            as u16;
                        view.set_preview_height(pane, height);
                    }
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        view.resize_drag = None;
                    }
                    return true;
                }
                _ => {}
            }
        }
        if let Some((collection_id, _pane, start_row)) = self
            .state
            .collection_views
            .iter()
            .find_map(|(id, view)| view.row_drag.map(|(pane, row)| (*id, pane, row)))
        {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) if mouse.row != start_row => {
                    self.reorder_relative(
                        collection_id,
                        if mouse.row < start_row { -1 } else { 1 },
                    );
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        if let Some((pane, _)) = view.row_drag {
                            view.row_drag = Some((pane, mouse.row));
                        }
                    }
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    if let Some(view) = self.state.collection_views.get_mut(&collection_id) {
                        view.row_drag = None;
                    }
                    return true;
                }
                _ => {}
            }
        }
        let Some(hit) = hit else { return false };
        // The hit rectangle is viewport-clipped, while terminal mouse coordinates are logical.
        // Reapply the clipped-off row count before the standard pane forwarding path subtracts
        // the visible rectangle origin.
        let terminal_mouse = MouseEvent {
            row: mouse.row.saturating_add(hit.terminal_row_offset),
            ..mouse
        };
        let Some(ws_idx) = self.state.active else {
            return true;
        };
        let entered_preview = hit.kind == CollectionHitKind::Preview
            && self
                .state
                .collection_views
                .get(&hit.collection_id)
                .is_some_and(|v| {
                    v.mode == CollectionInteractionMode::Terminal && v.entered == hit.pane_id
                });
        let preview_geometry = entered_preview
            .then(|| {
                let pane = hit.pane_id?;
                let (screen_rect, logical_row_offset, logical_rows, logical_cols) =
                    self.collection_child_preview_info(hit.collection_id, pane)?;
                Some((
                    PaneInfo {
                        id: pane,
                        rect: screen_rect,
                        inner_rect: screen_rect,
                        scrollbar_rect: None,
                        borders: Borders::NONE,
                        is_focused: true,
                    },
                    PaneUrlClickTarget {
                        pane_id: pane,
                        screen_rect,
                        logical_rows,
                        logical_cols,
                        logical_row_offset,
                    },
                ))
            })
            .flatten();
        let preview_info = preview_geometry.as_ref().map(|(info, _)| info.clone());
        if self.state.handle_right_click_passthrough(
            &self.terminal_runtimes,
            terminal_mouse,
            false,
            preview_info
                .clone()
                .map(|info| (info, hit.terminal_row_offset, 0)),
        ) {
            return true;
        }
        if preview_geometry
            .as_ref()
            .is_some_and(|(_, target)| self.handle_modified_url_click_at(source_id, mouse, *target))
        {
            return true;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let child_scrollbar = hit.kind == CollectionHitKind::PreviewScrollbar;
                if child_scrollbar || entered_preview {
                    if let Some(pane) = hit.pane_id {
                        // A scrollbar gutter is host UI, so wheel input there must always move
                        // host scrollback even when the child requested mouse or alternate-scroll
                        // reporting. Only entered terminal content forwards those routing modes.
                        self.scroll_collection_child_wheel(
                            ws_idx,
                            hit.collection_id,
                            pane,
                            mouse,
                            entered_preview,
                        );
                    }
                } else {
                    let max_scroll = self
                        .state
                        .view
                        .collection_layouts
                        .iter()
                        .find(|layout| layout.id == hit.collection_id)
                        .map(|layout| layout.content_height.saturating_sub(layout.viewport_height))
                        .unwrap_or_default();
                    let view = self
                        .state
                        .collection_views
                        .entry(hit.collection_id)
                        .or_default();
                    view.scroll = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                        view.scroll.saturating_sub(3)
                    } else {
                        view.scroll.saturating_add(3).min(max_scroll)
                    };
                }
                true
            }
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if entered_preview => {
                if let Some(info) = preview_info.as_ref() {
                    let _ = self.state.forward_pane_reported_wheel(
                        &self.terminal_runtimes,
                        info,
                        terminal_mouse,
                    );
                }
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // A new left-button gesture owns exactly one drag surface, even if a malformed
                // event stream omitted the previous release. Cancel both ordinary host drags and
                // collection-local drags so the newest press is the unambiguous owner.
                self.state.drag = None;
                for view in self.state.collection_views.values_mut() {
                    view.collection_scrollbar_drag = None;
                    view.child_scrollbar_drag = None;
                    view.resize_drag = None;
                    view.row_drag = None;
                }
                if hit.kind == CollectionHitKind::CollectionScrollbar {
                    let grab =
                        self.set_collection_scrollbar_from_row(hit.collection_id, mouse.row, None);
                    if let Some(grab_row_offset) = grab {
                        self.state
                            .collection_views
                            .entry(hit.collection_id)
                            .or_default()
                            .collection_scrollbar_drag = Some(grab_row_offset);
                    }
                } else if let Some(pane) = hit.pane_id {
                    self.select_collection_member_via_runtime(
                        ws_idx,
                        hit.collection_id,
                        pane,
                        true,
                    );
                    match hit.kind {
                        CollectionHitKind::PreviewScrollbar => {
                            self.restore_archived_member_for_input(ws_idx, pane);
                            let grab = self.set_collection_child_scrollbar_from_row(
                                ws_idx,
                                hit.collection_id,
                                pane,
                                mouse.row,
                                None,
                            );
                            if let Some(grab_row_offset) = grab {
                                let view = self
                                    .state
                                    .collection_views
                                    .entry(hit.collection_id)
                                    .or_default();
                                view.child_scrollbar_drag = Some((pane, grab_row_offset));
                            }
                        }
                        CollectionHitKind::Disclosure => {
                            self.toggle_selected_expanded(hit.collection_id)
                        }
                        CollectionHitKind::Preview => {
                            self.enter_collection_terminal(ws_idx, hit.collection_id);
                            self.restore_archived_member_for_input(ws_idx, pane);
                            let info = PaneInfo {
                                id: pane,
                                rect: hit.rect,
                                inner_rect: hit.rect,
                                scrollbar_rect: None,
                                borders: Borders::NONE,
                                is_focused: true,
                            };
                            let _ = self.state.forward_pane_mouse_button(
                                &self.terminal_runtimes,
                                &info,
                                terminal_mouse,
                            );
                        }
                        CollectionHitKind::ResizeHandle => {
                            let height = self.effective_preview_height(hit.collection_id, pane);
                            let view = self
                                .state
                                .collection_views
                                .entry(hit.collection_id)
                                .or_default();
                            view.resize_drag = Some((pane, mouse.row, height));
                        }
                        CollectionHitKind::Row => {
                            let view = self
                                .state
                                .collection_views
                                .entry(hit.collection_id)
                                .or_default();
                            view.mode = CollectionInteractionMode::List;
                            view.entered = None;
                            view.row_drag = Some((pane, mouse.row));
                        }
                        _ => {
                            if let Some(view) =
                                self.state.collection_views.get_mut(&hit.collection_id)
                            {
                                view.mode = CollectionInteractionMode::List;
                                view.entered = None;
                            }
                        }
                    }
                } else if let Some(tab) = self.state.workspaces[ws_idx].active_tab_mut() {
                    let _ = tab
                        .layout
                        .focus_leaf(LayoutLeaf::Collection(hit.collection_id));
                    let view = self
                        .state
                        .collection_views
                        .entry(hit.collection_id)
                        .or_default();
                    view.mode = CollectionInteractionMode::List;
                    view.entered = None;
                    self.state.mark_session_dirty();
                }
                true
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Pane-less collection surfaces (outer/inner chrome and its scrollbar) are
                // native collection targets; only a member-owned hit opens a pane menu.
                if hit.pane_id.is_none() {
                    self.state.context_menu = Some(ContextMenuState::new(
                        ContextMenuKind::Collection {
                            collection_id: hit.collection_id,
                        },
                        mouse.column,
                        mouse.row,
                    ));
                    self.state.mode = Mode::ContextMenu;
                } else if let Some(pane) = hit.pane_id {
                    self.select_collection_member_via_runtime(
                        ws_idx,
                        hit.collection_id,
                        pane,
                        true,
                    );
                    let archived = self.state.workspaces[ws_idx]
                        .active_tab()
                        .and_then(|tab| tab.collection(hit.collection_id))
                        .is_some_and(|collection| collection.is_archived(pane));
                    self.state.context_menu = Some(ContextMenuState {
                        kind: ContextMenuKind::CollectionMember {
                            ws_idx,
                            collection_id: hit.collection_id,
                            pane_id: pane,
                            archived,
                        },
                        x: mouse.column,
                        y: mouse.row,
                        list: MenuListState::new(0),
                        scroll_offset: 0,
                        plugin: None,
                    });
                    self.state.mode = Mode::ContextMenu;
                }
                true
            }
            MouseEventKind::Down(MouseButton::Middle) if entered_preview => {
                if let Some(pane) = hit.pane_id {
                    self.restore_archived_member_for_input(ws_idx, pane);
                }
                if let Some(info) = preview_info.as_ref() {
                    let _ = self.state.forward_pane_mouse_button(
                        &self.terminal_runtimes,
                        info,
                        terminal_mouse,
                    );
                }
                true
            }
            MouseEventKind::Moved | MouseEventKind::Up(_) | MouseEventKind::Drag(_) => {
                if hit.kind == CollectionHitKind::Preview
                    && self
                        .state
                        .collection_views
                        .get(&hit.collection_id)
                        .is_some_and(|view| {
                            view.mode == CollectionInteractionMode::Terminal
                                && view.entered == hit.pane_id
                        })
                {
                    if let Some(pane) = hit.pane_id {
                        self.restore_archived_member_for_input(ws_idx, pane);
                        let info = PaneInfo {
                            id: pane,
                            rect: hit.rect,
                            inner_rect: hit.rect,
                            scrollbar_rect: None,
                            borders: Borders::NONE,
                            is_focused: true,
                        };
                        if matches!(mouse.kind, MouseEventKind::Moved) {
                            let _ = self.state.forward_pane_mouse_motion(
                                &self.terminal_runtimes,
                                &info,
                                terminal_mouse,
                            );
                        } else {
                            let _ = self.state.forward_pane_mouse_button(
                                &self.terminal_runtimes,
                                &info,
                                terminal_mouse,
                            );
                        }
                    }
                }
                true
            }
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    use super::super::navigate::{ActionContext, NavigateAction};
    use super::{CollectionHitKind, CollectionInteractionMode, KeyCode};
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

    use crate::{
        app::{
            state::{ContextMenuKind, ContextMenuState, Mode},
            AppState,
        },
        layout::LayoutLeaf,
        terminal::TerminalRuntimeRegistry,
        workspace::Workspace,
    };

    fn collection_scroll_app(
        terminal_mode: &[u8],
        extra_members: usize,
    ) -> (
        crate::app::App,
        crate::layout::CollectionId,
        crate::layout::PaneId,
        mpsc::Receiver<bytes::Bytes>,
    ) {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("collection-scroll");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(ratatui::layout::Direction::Horizontal);
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Horizontal,
                0.5,
                None,
            )
            .expect("collection");
        ws.collect_pane(child, collection).expect("collect child");
        for _ in 0..extra_members {
            let pane = ws.test_split(ratatui::layout::Direction::Horizontal);
            ws.collect_pane(pane, collection).expect("collect extra");
        }
        ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        let history = (0..60).map(|n| format!("line {n}\r\n")).collect::<String>();
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                40,
                8,
                64 * 1024,
                history.as_bytes(),
                8,
            );
        runtime.test_process_pty_bytes(terminal_mode);
        ws.tabs[0].runtimes.insert(child, runtime);
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state
            .enter_collection_terminal_from_foreground(0, collection, child);
        app.state
            .collection_views
            .entry(collection)
            .or_default()
            .set_preview_height(child, 8);
        let surface = crate::ui::compute_tab_surface(
            &mut app.state,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        app.state.view.collection_layouts = surface.collection_layouts;
        (app, collection, child, rx)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[cfg(unix)]
    fn install_test_link_handler(app: &mut crate::app::App) {
        let plugin_root = std::env::temp_dir();
        app.state.installed_plugins = std::collections::HashMap::from([(
            "example.links".to_string(),
            crate::api::schema::InstalledPluginInfo {
                plugin_id: "example.links".into(),
                name: "Links".into(),
                version: "0.1.0".into(),
                min_herdr_version: "0.6.10".into(),
                description: None,
                manifest_path: plugin_root.join("herdr-plugin.toml").display().to_string(),
                plugin_root: plugin_root.display().to_string(),
                enabled: true,
                platforms: None,
                build: Vec::new(),
                startup: Vec::new(),
                actions: vec![crate::api::schema::PluginManifestAction {
                    id: "open".into(),
                    title: "Open link".into(),
                    description: None,
                    contexts: Vec::new(),
                    platforms: None,
                    command: vec!["sh".into(), "-c".into(), ":".into()],
                    choices_command: None,
                }],
                events: Vec::new(),
                panes: Vec::new(),
                link_handlers: vec![crate::api::schema::PluginManifestLinkHandler {
                    id: "github-issue".into(),
                    title: "Open GitHub issue".into(),
                    pattern: "^https://github\\.com/[^/]+/[^/]+/issues/[0-9]+$".into(),
                    action: "open".into(),
                    platforms: None,
                }],
                source: crate::api::schema::PluginSourceInfo::default(),
                warnings: Vec::new(),
            },
        )]);
    }

    #[tokio::test]
    async fn collection_member_right_click_initializes_plugin_context_through_mouse_router() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 0);
        let row = app.state.view.collection_layouts[0]
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .expect("child row")
            .row_rect;

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            row.x,
            row.y,
        ));

        let menu = app.state.context_menu.as_ref().expect("context menu");
        assert!(matches!(
            menu.kind,
            crate::app::state::ContextMenuKind::CollectionMember {
                collection_id,
                pane_id,
                ..
            } if collection_id == collection && pane_id == child
        ));
        let plugin = menu.plugin.as_ref().expect("plugin context initialized");
        assert_eq!(
            plugin.target,
            crate::app::state::ContextMenuTarget::Pane(
                app.public_pane_id(0, child).expect("public child pane")
            )
        );
    }

    #[tokio::test]
    async fn collection_chrome_and_member_hit_regions_keep_their_distinct_context_menus() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 12);
        let layout = app.state.view.collection_layouts[0].clone();
        let mut member_hits = [
            CollectionHitKind::Row,
            CollectionHitKind::Disclosure,
            CollectionHitKind::Preview,
            CollectionHitKind::PreviewScrollbar,
            CollectionHitKind::ResizeHandle,
        ]
        .into_iter()
        .map(|kind| {
            layout
                .hits
                .iter()
                .find(|hit| hit.kind == kind && hit.pane_id == Some(child))
                .map(|hit| (kind, hit.rect.x, hit.rect.y))
                .expect("rendered member hit")
        })
        .collect::<Vec<_>>();
        // The active header is blank inner chrome; the remaining points are all outer borders.
        let header = layout.active_header.expect("active header");
        let collection_scrollbar = layout
            .scrollbar_rect
            .expect("rendered collection scrollbar");
        let chrome_hits = [
            (header.x, header.y),
            (collection_scrollbar.x, collection_scrollbar.y),
            (layout.rect.x, layout.rect.y),
            (layout.rect.right().saturating_sub(1), layout.rect.y),
            (layout.rect.x, layout.rect.bottom().saturating_sub(1)),
            (
                layout.rect.right().saturating_sub(1),
                layout.rect.bottom().saturating_sub(1),
            ),
        ];

        for (column, row) in chrome_hits {
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), column, row));
            let menu = app.state.context_menu.as_ref().expect("collection menu");
            assert!(
                matches!(menu.kind, ContextMenuKind::Collection { collection_id } if collection_id == collection)
            );
            assert_eq!(menu.items(), ["Close collection…"]);
            assert!(
                menu.plugin.is_none(),
                "native collection menu has no plugin state"
            );
            app.state.context_menu = None;
            app.state.mode = Mode::Terminal;
        }
        for (kind, column, row) in member_hits.drain(..) {
            app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Right), column, row));
            let menu = app.state.context_menu.as_ref().expect("member menu");
            assert!(
                matches!(menu.kind, ContextMenuKind::CollectionMember { collection_id, pane_id, .. } if collection_id == collection && pane_id == child),
                "{kind:?}"
            );
            assert_eq!(
                menu.plugin.as_ref().map(|plugin| &plugin.target),
                Some(&crate::app::state::ContextMenuTarget::Pane(
                    app.public_pane_id(0, child).expect("public child")
                )),
                "{kind:?} must retain pane plugin context"
            );
            app.state.context_menu = None;
            app.state.mode = Mode::Terminal;
        }
    }

    #[tokio::test]
    async fn visible_disclosure_toggles_while_selector_only_selects() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 0);
        let layout = app.state.view.collection_layouts[0].clone();
        let row = layout
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .expect("child row");
        let disclosure = layout
            .hits
            .iter()
            .find(|hit| hit.pane_id == Some(child) && hit.kind == CollectionHitKind::Disclosure)
            .expect("visible disclosure")
            .rect;
        assert_eq!(disclosure.x, row.row_rect.x + 2);
        assert!(app.state.collection_views[&collection]
            .expanded
            .contains(&child));

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            row.row_rect.x,
            row.row_rect.y,
        ));
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            row.row_rect.x,
            row.row_rect.y,
        ));
        assert!(app.state.collection_views[&collection]
            .expanded
            .contains(&child));

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            disclosure.x,
            disclosure.y,
        ));
        assert!(!app.state.collection_views[&collection]
            .expanded
            .contains(&child));
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            disclosure.x,
            disclosure.y,
        ));
        assert!(app.state.collection_views[&collection]
            .expanded
            .contains(&child));
    }

    #[tokio::test]
    async fn collection_context_menu_and_close_dialog_own_left_clicks_over_collection_content() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 12);
        let layout = app.state.view.collection_layouts[0].clone();
        let header = layout.active_header.expect("active header");

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            header.x,
            header.y,
        ));
        let menu = app.state.context_menu_rect().expect("context menu rect");
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            menu.x + 1,
            menu.y + 1,
        ));
        assert_eq!(app.state.mode, Mode::CollectionClose);
        assert_eq!(
            app.state
                .pending_collection_close
                .as_ref()
                .map(|pending| pending.collection_id),
            Some(collection)
        );

        let popup = crate::ui::collection_close_popup_rect(app.state.view.terminal_area)
            .expect("collection close popup");
        let inner = Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        );
        let (promote, _, _) = crate::ui::collection_close_button_rects(inner);
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            promote.x,
            promote.y,
        ));

        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .is_none());
        let promoted_tab = app.state.workspaces[0]
            .find_tab_index_for_pane(child)
            .expect("standalone promoted tab");
        assert_ne!(promoted_tab, 0);
        assert_eq!(app.state.workspaces[0].tabs[promoted_tab].pane_count(), 1);
    }

    #[tokio::test]
    async fn maximized_preview_keeps_member_menu_but_its_outer_border_opens_collection_menu() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 0);
        app.state
            .collection_views
            .get_mut(&collection)
            .expect("view")
            .maximized = Some(child);
        let surface = crate::ui::compute_tab_surface(
            &mut app.state,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        app.state.view.collection_layouts = surface.collection_layouts;
        let layout = app.state.view.collection_layouts[0].clone();
        let preview = layout.maximized_preview_rect.expect("maximized preview");

        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            preview.x + 2,
            preview.y + 2,
        ));
        assert!(
            matches!(app.state.context_menu.as_ref().map(|menu| menu.kind.clone()), Some(ContextMenuKind::CollectionMember { pane_id, .. }) if pane_id == child)
        );
        app.state.context_menu = None;
        app.state.mode = Mode::Terminal;
        app.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Right),
            layout.rect.right().saturating_sub(1),
            layout.rect.y + 2,
        ));
        assert!(
            matches!(app.state.context_menu.as_ref().map(|menu| menu.kind.clone()), Some(ContextMenuKind::Collection { collection_id }) if collection_id == collection)
        );

        // In the normal layout the left frame remains unclaimed, so a surrounding split-border
        // resize receives its complete down/drag/up gesture.
        app.state
            .collection_views
            .get_mut(&collection)
            .expect("view")
            .maximized = None;
        let surface = crate::ui::compute_tab_surface(
            &mut app.state,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        app.state.view.collection_layouts = surface.collection_layouts;
        let normal = app.state.view.collection_layouts[0].clone();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            assert!(
                !app.handle_collection_mouse(mouse(kind, normal.rect.x, normal.rect.y + 2)),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn entered_preview_host_wheel_scrolls_child_and_non_entered_preview_scrolls_collection() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 12);
        let row = app.state.view.collection_layouts[0]
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .expect("child row");
        let preview = row.preview_rect.expect("preview");
        let child_scrollbar = row.preview_scrollbar_rect.expect("child scrollbar");
        let collection_row = row.row_rect;
        let before = app.state.workspaces[0].tabs[0].runtimes[&child]
            .scroll_metrics()
            .expect("metrics")
            .offset_from_bottom;
        app.handle_collection_mouse(mouse(MouseEventKind::ScrollUp, preview.x, preview.y));
        let after = app.state.workspaces[0].tabs[0].runtimes[&child]
            .scroll_metrics()
            .expect("metrics")
            .offset_from_bottom;
        assert!(
            after > before,
            "HostScroll must fall back to child scrollback"
        );
        assert_eq!(app.state.collection_views[&collection].scroll, 0);

        app.state
            .collection_views
            .get_mut(&collection)
            .expect("view")
            .mode = crate::app::collection_view::CollectionInteractionMode::List;
        app.handle_collection_mouse(mouse(MouseEventKind::ScrollDown, preview.x, preview.y));
        assert!(app.state.collection_views[&collection].scroll > 0);
        assert_eq!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("metrics")
                .offset_from_bottom,
            after
        );

        let collection_after_preview = app.state.collection_views[&collection].scroll;
        app.handle_collection_mouse(mouse(
            MouseEventKind::ScrollDown,
            child_scrollbar.x,
            child_scrollbar.y,
        ));
        assert!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("metrics")
                .offset_from_bottom
                < after
        );
        assert_eq!(
            app.state.collection_views[&collection].scroll,
            collection_after_preview
        );

        app.handle_collection_mouse(mouse(
            MouseEventKind::ScrollDown,
            collection_row.x,
            collection_row.y,
        ));
        assert!(app.state.collection_views[&collection].scroll > collection_after_preview);
    }

    #[tokio::test]
    async fn entered_preview_mouse_report_and_alternate_scroll_are_forwarded() {
        for (mode, host_scrollback_available) in [
            (b"\x1b[?1000h".as_slice(), true),
            (b"\x1b[?1049h\x1b[?1007h".as_slice(), false),
        ] {
            // Build the layout while host scrollback is visible so the host scrollbar has a hit
            // region, then let the child request terminal wheel routing. The existing host gutter
            // remains host-owned until the next render, just like an ordinary pane.
            let (mut app, _collection, child, mut rx) = collection_scroll_app(b"", 0);
            let row = &app.state.view.collection_layouts[0].rows[0];
            let preview = row.preview_rect.expect("preview");
            let scrollbar = row.preview_scrollbar_rect.expect("child scrollbar");
            app.state.workspaces[0].tabs[0].runtimes[&child].test_process_pty_bytes(mode);
            app.handle_collection_mouse(mouse(MouseEventKind::ScrollUp, preview.x, preview.y));
            assert!(
                rx.try_recv().is_ok(),
                "routing mode {mode:?} must reach PTY"
            );
            assert_eq!(
                app.state.workspaces[0].tabs[0].runtimes[&child]
                    .scroll_metrics()
                    .expect("metrics")
                    .offset_from_bottom,
                0
            );

            app.handle_collection_mouse(mouse(MouseEventKind::ScrollUp, scrollbar.x, scrollbar.y));
            assert!(
                rx.try_recv().is_err(),
                "scrollbar wheel must not reach PTY in routing mode {mode:?}"
            );
            let offset = app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("metrics")
                .offset_from_bottom;
            if host_scrollback_available {
                assert!(
                    offset > 0,
                    "scrollbar wheel must move available host scrollback in routing mode {mode:?}"
                );
            } else {
                assert_eq!(
                    offset, 0,
                    "alternate-screen scrollbar input must remain host-owned without reaching PTY"
                );
            }
        }
    }

    fn clip_entered_preview(
        app: &mut crate::app::App,
        child: crate::layout::PaneId,
        logical_row_offset: u16,
        visible_height: u16,
    ) -> Rect {
        let layout = &mut app.state.view.collection_layouts[0];
        let row = layout
            .rows
            .iter_mut()
            .find(|row| row.pane_id == child)
            .expect("child row");
        let mut rect = row.preview_rect.expect("preview");
        rect.height = visible_height;
        row.preview_rect = Some(rect);
        row.preview_row_offset = logical_row_offset;
        let hit = layout
            .hits
            .iter_mut()
            .find(|hit| {
                hit.pane_id == Some(child)
                    && hit.kind == crate::app::collection_view::CollectionHitKind::Preview
            })
            .expect("preview hit");
        hit.rect = rect;
        hit.terminal_row_offset = logical_row_offset;
        rect
    }

    fn ctrl_click(app: &mut crate::app::App, source_id: u64, column: u16, row: u16) {
        let mut down = mouse(MouseEventKind::Down(MouseButton::Left), column, row);
        down.modifiers = KeyModifiers::CONTROL;
        app.handle_mouse_from_input_source(source_id, down);
        app.handle_mouse_from_input_source(
            source_id,
            mouse(MouseEventKind::Up(MouseButton::Left), column, row),
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn entered_preview_ctrl_click_finds_plain_urls_in_top_and_bottom_clipped_previews() {
        let (mut app, _collection, child, mut rx) = collection_scroll_app(b"", 0);
        install_test_link_handler(&mut app);
        app.state.mode = crate::app::state::Mode::Terminal;

        for (source_id, row_offset, logical_row, issue) in [(77, 0, 1, 398), (78, 6, 6, 399)] {
            let line = format!("see https://github.com/ogulcancelik/herdr/issues/{issue}");
            app.state.workspaces[0].tabs[0].runtimes[&child].test_process_pty_bytes(
                format!("\x1b[2J\x1b[{};1H{line}", logical_row + 1).as_bytes(),
            );
            let preview = clip_entered_preview(&mut app, child, row_offset, 2);
            let commands_before = app.state.plugin_command_logs.len();

            ctrl_click(
                &mut app,
                source_id,
                preview.x + line.find("github").expect("host") as u16,
                preview.y + logical_row - row_offset,
            );

            assert_eq!(app.state.plugin_command_logs.len(), commands_before + 1);
            assert_eq!(
                app.state
                    .plugin_command_logs
                    .last()
                    .map(|log| log.plugin_id.as_str()),
                Some("example.links")
            );
            assert!(rx.try_recv().is_err(), "URL gesture must not reach the PTY");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn entered_preview_ctrl_click_finds_osc_links_in_top_and_bottom_clipped_previews() {
        let (mut app, _collection, child, mut rx) = collection_scroll_app(b"", 0);
        install_test_link_handler(&mut app);
        app.state.mode = crate::app::state::Mode::Terminal;

        for (source_id, row_offset, logical_row, issue) in [(79, 0, 1, 400), (80, 6, 6, 401)] {
            let uri = format!("https://github.com/ogulcancelik/herdr/issues/{issue}");
            app.state.workspaces[0].tabs[0].runtimes[&child].test_process_pty_bytes(
                format!(
                    "\x1b[2J\x1b[{};1H\x1b]8;;{uri}\x1b\\label\x1b]8;;\x1b\\",
                    logical_row + 1
                )
                .as_bytes(),
            );
            let preview = clip_entered_preview(&mut app, child, row_offset, 2);
            let commands_before = app.state.plugin_command_logs.len();

            ctrl_click(
                &mut app,
                source_id,
                preview.x,
                preview.y + logical_row - row_offset,
            );

            assert_eq!(app.state.plugin_command_logs.len(), commands_before + 1);
            assert_eq!(
                app.state
                    .plugin_command_logs
                    .last()
                    .map(|log| log.plugin_id.as_str()),
                Some("example.links")
            );
            assert!(rx.try_recv().is_err(), "URL gesture must not reach the PTY");
        }
    }

    #[tokio::test]
    async fn entered_preview_routes_horizontal_motion_buttons_and_right_passthrough() {
        let (mut app, _collection, child, mut rx) =
            collection_scroll_app(b"\x1b[?1003h\x1b[?1006h", 0);
        let preview = app.state.view.collection_layouts[0].rows[0]
            .preview_rect
            .expect("preview");

        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Up(MouseButton::Middle),
            MouseEventKind::ScrollLeft,
        ] {
            assert!(app.handle_collection_mouse(mouse(kind, preview.x, preview.y)));
            assert!(rx.try_recv().is_ok(), "{kind:?} must reach the child PTY");
        }

        app.state.right_click_passthrough_modifiers = Some(KeyModifiers::CONTROL);
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
        ] {
            let mut event = mouse(kind, preview.x, preview.y);
            event.modifiers = KeyModifiers::CONTROL;
            assert!(app.handle_collection_mouse(event));
            assert!(rx.try_recv().is_ok(), "{kind:?} must reach the child PTY");
        }
        assert!(app.state.right_click_passthrough.is_none());
        assert!(app.state.context_menu.is_none());
        assert_eq!(
            app.state.workspaces[0].focused_pane_id(),
            Some(child),
            "preview routing must retain typed child focus"
        );
    }

    #[tokio::test]
    async fn clipped_preview_right_passthrough_keeps_transform_over_preview_chrome_and_outside() {
        let (mut app, _collection, child, mut rx) =
            collection_scroll_app(b"\x1b[?1003h\x1b[?1006h", 0);
        let preview = clip_entered_preview(&mut app, child, 5, 2);
        let chrome = app.state.view.collection_layouts[0]
            .hits
            .iter()
            .find(|hit| hit.kind == CollectionHitKind::Chrome)
            .map(|hit| (hit.rect.x, hit.rect.y))
            .expect("collection chrome");
        let logical_rows = app.state.view.collection_layouts[0].rows[0]
            .preview_size
            .expect("logical preview size")
            .0;
        let layout_rect = app.state.view.collection_layouts[0].rect;
        let outside = (preview.x + 3, layout_rect.bottom().saturating_add(2));
        app.state.right_click_passthrough_modifiers = Some(KeyModifiers::CONTROL);

        let events = [
            (
                MouseEventKind::Down(MouseButton::Right),
                preview.x + 1,
                preview.y,
            ),
            (
                MouseEventKind::Drag(MouseButton::Right),
                preview.x + 2,
                preview.y + 1,
            ),
            (MouseEventKind::Drag(MouseButton::Right), chrome.0, chrome.1),
            (MouseEventKind::Up(MouseButton::Right), outside.0, outside.1),
        ];
        for (kind, column, row) in events {
            let mut event = mouse(kind, column, row);
            event.modifiers = KeyModifiers::CONTROL;
            app.handle_mouse_from_input_source(91, event);
        }

        let expected_cell = |column: u16, row: u16| {
            (
                column.saturating_sub(preview.x).saturating_add(1),
                row.saturating_add(5)
                    .saturating_sub(preview.y)
                    .saturating_add(1)
                    .min(logical_rows),
            )
        };
        let preview_down = expected_cell(preview.x + 1, preview.y);
        let preview_drag = expected_cell(preview.x + 2, preview.y + 1);
        let chrome_drag = expected_cell(chrome.0, chrome.1);
        let outside_up = expected_cell(outside.0, outside.1);
        assert_eq!(
            rx.try_recv().expect("preview down"),
            Bytes::from(format!("\x1b[<2;{};{}M", preview_down.0, preview_down.1))
        );
        assert_eq!(
            rx.try_recv().expect("preview drag"),
            Bytes::from(format!("\x1b[<34;{};{}M", preview_drag.0, preview_drag.1))
        );
        assert_eq!(
            rx.try_recv().expect("chrome drag"),
            Bytes::from(format!("\x1b[<34;{};{}M", chrome_drag.0, chrome_drag.1))
        );
        assert_eq!(
            rx.try_recv().expect("outside up"),
            Bytes::from(format!("\x1b[<2;{};{}m", outside_up.0, outside_up.1))
        );
        assert!(rx.try_recv().is_err());
        assert!(app.state.right_click_passthrough.is_none());
        assert!(app.state.context_menu.is_none());
    }

    #[tokio::test]
    async fn pane_only_mouse_path_routes_entered_preview_but_not_collection_chrome() {
        let (mut app, collection, _child, mut rx) =
            collection_scroll_app(b"\x1b[?1003h\x1b[?1006h", 0);
        let layout = &app.state.view.collection_layouts[0];
        let preview = layout.rows[0].preview_rect.expect("preview");
        let chrome = layout.rows[0].row_rect;

        app.state.handle_pane_mouse_only(
            &app.terminal_runtimes,
            mouse(MouseEventKind::Moved, preview.x, preview.y),
        );
        assert!(rx.try_recv().is_ok());
        app.state.handle_pane_mouse_only(
            &app.terminal_runtimes,
            mouse(MouseEventKind::ScrollRight, preview.x, preview.y),
        );
        assert!(rx.try_recv().is_ok());
        app.state.handle_pane_mouse_only(
            &app.terminal_runtimes,
            mouse(MouseEventKind::Moved, chrome.x, chrome.y),
        );
        assert!(
            rx.try_recv().is_err(),
            "collection row chrome must stay isolated"
        );
        assert_eq!(app.state.collection_views[&collection].scroll, 0);
    }

    #[tokio::test]
    async fn child_scrollbar_click_and_drag_never_move_collection_scroll() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 12);
        app.state
            .collection_views
            .get_mut(&collection)
            .expect("view")
            .scroll = 4;
        let surface = crate::ui::compute_tab_surface(
            &mut app.state,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        app.state.view.collection_layouts = surface.collection_layouts;
        let row = app.state.view.collection_layouts[0]
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .expect("child row");
        let track = row.preview_scrollbar_rect.expect("child scrollbar");
        let metrics = app.state.workspaces[0].tabs[0].runtimes[&child]
            .scroll_metrics()
            .expect("metrics");
        let logical_rows = row.preview_size.expect("logical preview").0;
        let preview_row_offset = row.preview_row_offset;
        assert!(preview_row_offset > 0, "preview must be clipped at the top");
        let thumb =
            crate::ui::scrollbar_thumb(metrics, Rect::new(0, 0, 1, logical_rows)).expect("thumb");
        let thumb_screen_row = track
            .y
            .saturating_add(thumb.top.saturating_sub(row.preview_row_offset));
        let collection_scroll = app.state.collection_views[&collection].scroll;

        app.handle_collection_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            track.x,
            track.y,
        ));
        assert!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("metrics")
                .offset_from_bottom
                > 0,
            "track click must scroll only the child"
        );
        assert_eq!(
            app.state.collection_views[&collection].scroll,
            collection_scroll
        );
        app.state.workspaces[0].tabs[0].runtimes[&child].set_scroll_offset_from_bottom(0);

        app.handle_collection_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            track.x,
            thumb_screen_row,
        ));
        assert!(
            app.state.collection_views[&collection]
                .child_scrollbar_drag
                .is_some(),
            "track={track:?} logical_rows={logical_rows} row_offset={} thumb={thumb:?} click={thumb_screen_row}",
            preview_row_offset
        );
        app.handle_collection_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            track.x,
            track.y,
        ));
        app.handle_collection_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            track.x,
            track.y,
        ));
        assert!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("metrics")
                .offset_from_bottom
                > 0
        );
        assert_eq!(
            app.state.collection_views[&collection].scroll,
            collection_scroll
        );
        assert!(app.state.collection_views[&collection]
            .child_scrollbar_drag
            .is_none());
    }

    #[tokio::test]
    async fn collection_scrollbar_track_click_and_thumb_drag_only_move_collection() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 16);
        let child_offset = app.state.workspaces[0].tabs[0].runtimes[&child]
            .scroll_metrics()
            .expect("child metrics")
            .offset_from_bottom;
        let track = app.state.view.collection_layouts[0]
            .scrollbar_rect
            .expect("collection scrollbar");

        app.handle_collection_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            track.x,
            track.bottom().saturating_sub(1),
        ));
        assert!(app.state.collection_views[&collection].scroll > 0);
        assert_eq!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("child metrics")
                .offset_from_bottom,
            child_offset
        );
        assert!(app.state.collection_views[&collection]
            .collection_scrollbar_drag
            .is_none());
        let layout = &app.state.view.collection_layouts[0];
        let max_scroll = layout.content_height.saturating_sub(layout.viewport_height);
        app.state
            .collection_views
            .get_mut(&collection)
            .expect("collection view")
            .scroll = max_scroll / 3;

        let surface = crate::ui::compute_tab_surface(
            &mut app.state,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        app.state.view.collection_layouts = surface.collection_layouts;
        let layout = &app.state.view.collection_layouts[0];
        let track = layout.scrollbar_rect.expect("collection scrollbar");
        let max_scroll = layout.content_height.saturating_sub(layout.viewport_height);
        let metrics = crate::pane::ScrollMetrics {
            offset_from_bottom: max_scroll.saturating_sub(layout.scroll),
            max_offset_from_bottom: max_scroll,
            viewport_rows: layout.viewport_height,
        };
        let thumb = crate::ui::scrollbar_thumb(metrics, track).expect("collection thumb");
        let before_drag = layout.scroll;

        app.handle_collection_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            track.x,
            thumb.top,
        ));
        assert!(app.state.collection_views[&collection]
            .collection_scrollbar_drag
            .is_some());
        assert!(app.state.collection_views[&collection]
            .child_scrollbar_drag
            .is_none());
        assert!(app.state.collection_views[&collection].row_drag.is_none());
        app.handle_collection_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            track.x,
            track.bottom().saturating_sub(1),
        ));
        app.handle_collection_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            track.x,
            track.bottom().saturating_sub(1),
        ));
        assert!(app.state.collection_views[&collection].scroll > before_drag);
        assert!(app.state.collection_views[&collection]
            .collection_scrollbar_drag
            .is_none());
        assert_eq!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("child metrics")
                .offset_from_bottom,
            child_offset
        );
    }

    #[tokio::test]
    async fn collection_scrollbar_wheel_and_child_scrollbar_are_isolated() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 12);
        let layout = &app.state.view.collection_layouts[0];
        let outer = layout.scrollbar_rect.expect("collection scrollbar");
        let child_track = layout
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .and_then(|row| row.preview_scrollbar_rect)
            .expect("child scrollbar");
        let child_before = app.state.workspaces[0].tabs[0].runtimes[&child]
            .scroll_metrics()
            .expect("child metrics")
            .offset_from_bottom;

        app.handle_collection_mouse(mouse(MouseEventKind::ScrollDown, outer.x, outer.y));
        let collection_after_outer = app.state.collection_views[&collection].scroll;
        assert!(collection_after_outer > 0);
        assert_eq!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("child metrics")
                .offset_from_bottom,
            child_before
        );

        app.handle_collection_mouse(mouse(
            MouseEventKind::ScrollUp,
            child_track.x,
            child_track.y,
        ));
        assert_eq!(
            app.state.collection_views[&collection].scroll,
            collection_after_outer
        );
        assert!(
            app.state.workspaces[0].tabs[0].runtimes[&child]
                .scroll_metrics()
                .expect("child metrics")
                .offset_from_bottom
                > child_before
        );
    }

    fn group_membership(
        linked: bool,
        checkout_path: &str,
    ) -> crate::workspace::WorktreeSpaceMembership {
        crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: checkout_path.into(),
            is_linked_worktree: linked,
        }
    }

    fn grouped_final_collection_app() -> (
        crate::app::App,
        crate::layout::CollectionId,
        crate::layout::PaneId,
    ) {
        let mut app = super::super::app_for_mouse_test();
        let mut root_workspace = Workspace::test_new("root");
        let root = root_workspace.tabs[0].root_pane.expect("root pane");
        let child = root_workspace.test_split(ratatui::layout::Direction::Horizontal);
        let collection = root_workspace
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        root_workspace
            .collect_pane(root, collection)
            .expect("collect root");
        root_workspace
            .collect_pane(child, collection)
            .expect("collect child");
        root_workspace.worktree_space = Some(group_membership(false, "/repo"));
        let mut linked = Workspace::test_new("linked");
        linked.worktree_space = Some(group_membership(true, "/repo-linked"));
        app.state.workspaces = vec![root_workspace, linked];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.confirm_close = true;
        (app, collection, child)
    }
    #[tokio::test]
    async fn close_pane_binding_targets_collection_in_list_and_entered_modes_but_bare_x_closes_member(
    ) {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 0);
        for (mode, context) in [
            (CollectionInteractionMode::List, ActionContext::Direct),
            (CollectionInteractionMode::Terminal, ActionContext::Prefix),
        ] {
            let view = app
                .state
                .collection_views
                .get_mut(&collection)
                .expect("view");
            view.mode = mode;
            view.entered = (mode == CollectionInteractionMode::Terminal).then_some(child);
            app.state.mode = Mode::Terminal;
            app.execute_tui_navigate_action(NavigateAction::ClosePane, context);
            assert_eq!(app.state.mode, Mode::CollectionClose);
            assert_eq!(
                app.state
                    .pending_collection_close
                    .as_ref()
                    .map(|pending| pending.collection_id),
                Some(collection)
            );
            app.state.pending_collection_close = None;
        }

        app.state.mode = Mode::Terminal;
        app.state
            .collection_views
            .get_mut(&collection)
            .expect("view")
            .mode = CollectionInteractionMode::List;
        assert!(app.handle_collection_key(crate::input::TerminalKey::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )));
        assert!(!app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection remains")
            .members()
            .contains(&child));

        // An empty focused collection takes the configured close binding too.
        let mut empty_app = super::super::app_for_mouse_test();
        let mut empty_workspace = Workspace::test_new("empty-bound-close");
        let root = empty_workspace.tabs[0].root_pane.expect("root");
        let empty_collection = empty_workspace
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("empty collection");
        empty_workspace.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(empty_collection));
        empty_app.state.workspaces = vec![empty_workspace];
        empty_app.state.active = Some(0);
        empty_app.execute_tui_navigate_action(NavigateAction::ClosePane, ActionContext::Direct);
        assert_eq!(empty_app.state.workspaces.len(), 1);
        assert!(empty_app.state.workspaces[0].tabs[0]
            .collection(empty_collection)
            .is_none());

        // A regular pane still uses the ordinary pane-close dispatch.
        let mut plain_app = super::super::app_for_mouse_test();
        let mut plain_workspace = Workspace::test_new("plain-close");
        let root = plain_workspace.tabs[0].root_pane.expect("root");
        let sibling = plain_workspace.test_split(ratatui::layout::Direction::Horizontal);
        plain_workspace.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Pane(root));
        plain_app.state.workspaces = vec![plain_workspace];
        plain_app.state.active = Some(0);
        plain_app.state.ensure_test_terminals();
        plain_app.execute_tui_navigate_action(NavigateAction::ClosePane, ActionContext::Direct);
        assert_eq!(plain_app.state.workspaces[0].tabs[0].pane_count(), 1);
        assert!(plain_app.state.workspaces[0].pane_state(sibling).is_some());
    }

    #[tokio::test]
    async fn cancelling_a_stale_collection_prompt_without_an_active_workspace_leaves_navigate_mode()
    {
        let (mut app, collection, _child, _rx) = collection_scroll_app(b"", 0);
        app.open_collection_close_dialog(collection, false);
        app.state.workspaces.clear();
        app.state.active = None;

        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ));

        assert!(app.state.pending_collection_close.is_none());
        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn cascading_the_final_collection_in_the_final_workspace_leaves_navigate_mode() {
        let mut app = super::super::app_for_mouse_test();
        let mut workspace = Workspace::test_new("final-collection");
        let root = workspace.tabs[0].root_pane.expect("root pane");
        let child = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let collection = workspace
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        workspace
            .collect_pane(root, collection)
            .expect("collect root");
        workspace
            .collect_pane(child, collection)
            .expect("collect child");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;

        app.open_collection_close_dialog(collection, false);
        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        ));

        assert!(app.state.workspaces.is_empty());
        assert_eq!(app.state.active, None);
        assert_eq!(app.state.mode, Mode::Navigate);
    }

    #[test]
    fn collection_menu_uses_stable_id_across_workspace_shifts_and_dismisses_when_stale() {
        let (mut app, collection, _) = grouped_final_collection_app();
        let original_workspace_id = app.public_workspace_id(0);
        let menu = || {
            ContextMenuState::new(
                ContextMenuKind::Collection {
                    collection_id: collection,
                },
                0,
                0,
            )
        };
        app.state
            .workspaces
            .insert(0, Workspace::test_new("inserted"));
        app.state.active = Some(0);

        app.apply_context_menu_action_via_api(menu(), 0);
        assert_eq!(app.state.mode, Mode::CollectionClose);
        assert_eq!(
            app.state
                .pending_collection_close
                .as_ref()
                .map(|pending| &pending.workspace_id),
            Some(&original_workspace_id),
            "the menu must resolve the original live collection, not shifted indices"
        );

        app.state.workspaces[1]
            .cascade_close_collection(collection)
            .expect("remove original collection");
        app.state.mode = Mode::ContextMenu;
        app.state.context_menu = Some(menu());
        app.apply_context_menu_action_via_api(menu(), 0);
        assert!(app.state.context_menu.is_none());
        assert_eq!(app.state.mode, Mode::Terminal);
    }

    #[test]
    fn archive_cleanup_confirmation_summarizes_only_archived_members() {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("cleanup");
        let root = ws.tabs[0].root_pane.expect("root");
        let active = ws.test_split(ratatui::layout::Direction::Horizontal);
        let archived = ws.test_split(ratatui::layout::Direction::Horizontal);
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        ws.collect_pane(active, collection).expect("active member");
        ws.collect_pane(archived, collection)
            .expect("archived member");
        ws.set_collection_member_archived(archived, collection, true)
            .expect("archive");
        ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        app.state.workspaces = vec![ws];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        app.open_collection_close_dialog(collection, true);

        let pending = app
            .state
            .pending_collection_close
            .expect("confirmation summary");
        assert!(pending.cleanup_archive);
        assert_eq!(pending.active, 1);
        assert_eq!(pending.archived, 1);
        assert_eq!(pending.live + pending.exited, 1);
        assert_eq!(app.state.mode, crate::app::Mode::CollectionClose);
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection retained")
                .members()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn cascade_mutation_failure_reopens_close_dialog_without_touching_members_or_runtime() {
        let (mut app, collection, child, _rx) = collection_scroll_app(b"", 0);
        let before_members = app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .members()
            .to_vec();
        assert!(app.state.collection_views.contains_key(&collection));
        assert!(app.state.workspaces[0].tabs[0]
            .runtimes
            .contains_key(&child));
        app.open_collection_close_dialog(collection, false);

        crate::workspace::fail_next_collection_mutation_for_test();
        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
        ));

        assert_eq!(app.state.mode, Mode::CollectionClose);
        assert!(app.state.pending_collection_close.is_some());
        assert!(app.state.collection_views.contains_key(&collection));
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection retained")
                .members(),
            before_members
        );
        assert!(app.state.workspaces[0].tabs[0]
            .runtimes
            .contains_key(&child));
        assert!(app.state.terminal_runtime_shutdowns.is_empty());
    }

    #[test]
    fn empty_collection_mutation_failure_keeps_pending_dialog_view_and_collection() {
        let mut app = super::super::app_for_mouse_test();
        let mut workspace = Workspace::test_new("empty-close-failure");
        let root = workspace.tabs[0].root_pane.expect("root");
        let collection = workspace
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("empty collection");
        workspace.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.collection_views.entry(collection).or_default();

        crate::workspace::fail_next_collection_mutation_for_test();
        app.open_collection_close_dialog(collection, false);

        assert_eq!(app.state.mode, Mode::CollectionClose);
        assert!(app.state.pending_collection_close.is_some());
        assert!(app.state.collection_views.contains_key(&collection));
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .is_some());
    }

    #[test]
    fn destructive_confirmation_reprompts_when_archive_revision_changes() {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("stale-close");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(ratatui::layout::Direction::Horizontal);
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        ws.collect_pane(child, collection).expect("collect");
        ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.open_collection_close_dialog(collection, false);
        let original_revision = app
            .state
            .pending_collection_close
            .as_ref()
            .expect("prompt")
            .collection_revision;
        app.state.workspaces[0]
            .set_collection_member_archived(child, collection, true)
            .expect("archive mutation");

        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::empty(),
        ));

        let replacement = app
            .state
            .pending_collection_close
            .as_ref()
            .expect("replacement prompt");
        assert!(replacement.collection_revision > original_revision);
        assert_eq!(app.state.mode, crate::app::Mode::CollectionClose);
        assert_eq!(app.state.workspaces[0].tabs[0].pane_count(), 2);
    }

    #[test]
    fn first_stage_confirmation_uses_origin_after_shared_navigation_changes() {
        let (mut app, collection, _) = grouped_final_collection_app();
        app.open_collection_close_dialog(collection, false);
        let origin_workspace_id = app
            .state
            .pending_collection_close
            .as_ref()
            .expect("prompt")
            .workspace_id
            .clone();

        app.state.active = Some(1);
        app.state.workspaces[1].active_tab = 0;
        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('p'),
            crossterm::event::KeyModifiers::empty(),
        ));

        let origin_idx = app
            .parse_workspace_id(&origin_workspace_id)
            .expect("origin remains");
        assert!(app.state.workspaces[origin_idx].tabs[0]
            .collection(collection)
            .is_none());
        assert_eq!(app.state.active, Some(1));
        assert!(app.state.pending_collection_close.is_none());
    }

    #[test]
    fn entered_collection_routes_bare_escape_and_prefix_escape_exits() {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("escape");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(ratatui::layout::Direction::Horizontal);
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        ws.collect_pane(child, collection).expect("collect");
        ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);
        app.state
            .enter_collection_terminal_from_foreground(0, collection, child);

        let escape = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::empty(),
        );
        assert!(!app.handle_collection_key(escape));
        assert!(app.state.focused_collection_terminal_entered());

        let prefix = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(!app.handle_collection_key(prefix));
        assert!(
            app.state.focused_collection_terminal_entered(),
            "the prefix alone must not leave terminal mode"
        );
        app.state.mode = crate::app::Mode::Prefix;
        app.handle_prefix_key(escape);
        assert!(!app.state.focused_collection_terminal_entered());
        assert_eq!(app.state.mode, crate::app::Mode::Terminal);

        assert!(
            !app.state
                .enter_collection_terminal_from_foreground(0, collection, child),
            "pane was already acknowledged"
        );
        app.toggle_selected_expanded(collection);
        assert!(
            !app.state.focused_collection_terminal_entered(),
            "a collapsed child cannot retain terminal input"
        );
        let next = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        );
        assert!(
            app.handle_collection_key(next),
            "list navigation must be consumed instead of reaching a collapsed PTY"
        );
    }

    #[test]
    fn headless_list_navigation_is_consumed_before_child_pty_forwarding() {
        let mut app = super::super::app_for_mouse_test();
        let mut ws = Workspace::test_new("headless-list");
        let root = ws.tabs[0].root_pane.expect("root");
        let first = ws.test_split(ratatui::layout::Direction::Horizontal);
        let second = ws.test_split(ratatui::layout::Direction::Horizontal);
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .expect("collection");
        ws.collect_pane(first, collection).expect("collect first");
        ws.collect_pane(second, collection).expect("collect second");
        ws.select_collection_member(first, collection)
            .expect("select first");
        ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        app.state.workspaces = vec![ws];
        app.state.active = Some(0);

        let next = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('j'),
            crossterm::event::KeyModifiers::empty(),
        );
        assert!(app.handle_terminal_key_headless(next).is_none());
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .and_then(|collection| collection.selected()),
            Some(second)
        );
        assert!(
            !app.state.focused_collection_terminal_entered(),
            "headless list navigation must not enter a child PTY"
        );
    }

    #[test]
    fn api_selection_does_not_enter_or_mark_seen() {
        let mut ws = Workspace::test_new("c");
        let root = ws.tabs[0].root_pane.unwrap();
        let child = ws.test_split(ratatui::layout::Direction::Horizontal);
        let id = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Vertical,
                0.5,
                None,
            )
            .unwrap();
        ws.collect_pane(child, id).unwrap();
        ws.tabs[0].layout.focus_leaf(LayoutLeaf::Collection(id));
        ws.tabs[0].panes.get_mut(&child).unwrap().seen = false;
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        // Shared/API focus and selection do not create a client entry or acknowledgment.
        app.workspaces[0]
            .select_collection_member(child, id)
            .expect("select member");
        assert!(!app.collection_views.contains_key(&id));
        assert!(!app.workspaces[0].tabs[0].panes[&child].seen);
        assert!(!app.is_active_pane(0, 0, child));

        assert!(app.enter_collection_terminal_from_foreground(0, id, child));
        assert!(app.focused_collection_terminal_entered());
        assert!(app.workspaces[0].tabs[0].panes[&child].seen);
        assert!(app.is_active_pane(0, 0, child));
    }
}
