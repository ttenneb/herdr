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
        collection_view::{CollectionHitKind, CollectionInteractionMode},
        state::{ContextMenuKind, ContextMenuState, MenuListState, Mode},
        App,
    },
    input::TerminalKey,
    layout::{CollectionId, LayoutLeaf, PaneId, PaneInfo},
};

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
                } else {
                    let _ = self.dispatch_runtime_mutation(
                        "tui.collection.close_empty",
                        Method::CollectionClose(CollectionCloseParams {
                            collection_id: format!("collection_{}", collection_id.raw()),
                            disposition: None,
                            target_pane_id: None,
                            focus_promoted: false,
                        }),
                    );
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

    fn open_collection_close_dialog(&mut self, collection_id: CollectionId, cleanup_archive: bool) {
        let Some(ws_idx) = self.state.active else {
            return;
        };
        let tab_idx = self.state.workspaces[ws_idx].active_tab;
        self.open_collection_close_dialog_at(ws_idx, tab_idx, collection_id, cleanup_archive);
    }

    fn open_collection_close_dialog_at(
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
                group_close: None,
                cleanup_archive,
                active,
                archived,
                live,
                exited: counted_members.len().saturating_sub(live),
                working,
                blocked,
            });
        self.state.mode = Mode::CollectionClose;
        if collection_is_empty {
            let final_collection = self.state.workspaces[ws_idx].tabs.len() == 1
                && self.state.workspaces[ws_idx]
                    .tabs
                    .get(tab_idx)
                    .is_some_and(|tab| tab.layout.leaf_count() == 1);
            if final_collection
                && self.state.confirm_close
                && self.begin_pending_collection_group_close(ws_idx)
            {
                return;
            }
            let _ = self.dispatch_runtime_mutation(
                "tui.collection.close_empty",
                Method::CollectionClose(CollectionCloseParams {
                    collection_id: format!("collection_{}", collection_id.raw()),
                    disposition: None,
                    target_pane_id: None,
                    focus_promoted: false,
                }),
            );
            self.state.pending_collection_close = None;
            self.state.collection_views.remove(&collection_id);
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
        }
    }

    fn begin_pending_collection_group_close(&mut self, ws_idx: usize) -> bool {
        let Some(space) = self.state.workspaces.get(ws_idx).and_then(|workspace| {
            workspace
                .worktree_space()
                .filter(|space| !space.is_linked_worktree)
        }) else {
            return false;
        };
        let worktree_key = space.key.clone();
        let workspace_member_ids = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                workspace
                    .worktree_space()
                    .is_some_and(|member| member.key == worktree_key)
            })
            .map(|(idx, _)| self.public_workspace_id(idx))
            .collect::<Vec<_>>();
        if workspace_member_ids.len() < 2 {
            return false;
        }
        let workspace_id = self.public_workspace_id(ws_idx);
        let Some(pending) = self.state.pending_collection_close.as_mut() else {
            return false;
        };
        pending.group_close = Some(crate::app::collection_view::PendingCollectionGroupClose {
            workspace_id,
            worktree_key,
            workspace_member_ids,
        });
        self.state.selected = ws_idx;
        self.state.mode = Mode::ConfirmClose;
        true
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

    pub(super) fn confirm_pending_collection_group_close(&mut self) -> bool {
        let Some(pending) = self.state.pending_collection_close.take() else {
            return false;
        };
        let Some(group) = pending.group_close.as_ref() else {
            self.state.pending_collection_close = Some(pending);
            return false;
        };
        let target = self.pending_collection_matches(&pending);
        let group_matches = target.is_some_and(|(ws_idx, tab_idx)| {
            self.state.workspaces[ws_idx].tabs.len() == 1
                && self.state.workspaces[ws_idx].tabs[tab_idx]
                    .layout
                    .leaf_count()
                    == 1
                && group.workspace_id == pending.workspace_id
                && self.state.workspaces[ws_idx]
                    .worktree_space()
                    .is_some_and(|space| {
                        !space.is_linked_worktree && space.key == group.worktree_key
                    })
                && self
                    .state
                    .workspaces
                    .iter()
                    .enumerate()
                    .filter(|(_, workspace)| {
                        workspace
                            .worktree_space()
                            .is_some_and(|member| member.key == group.worktree_key)
                    })
                    .map(|(idx, _)| self.public_workspace_id(idx))
                    .eq(group.workspace_member_ids.iter().cloned())
        });
        if group_matches {
            let _ = self.dispatch_runtime_mutation(
                "tui.collection.close_group",
                Method::WorkspaceClose(crate::api::schema::WorkspaceTarget {
                    workspace_id: group.workspace_id.clone(),
                }),
            );
            self.state.collection_views.remove(&pending.collection_id);
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
        } else {
            self.state.mode = if self.state.active.is_some() {
                Mode::Terminal
            } else {
                Mode::Navigate
            };
            self.reopen_pending_collection_close(&pending);
        }
        true
    }

    pub(super) fn cancel_pending_collection_group_close(&mut self) -> bool {
        if self
            .state
            .pending_collection_close
            .as_ref()
            .is_none_or(|pending| pending.group_close.is_none())
        {
            return false;
        }
        self.state.pending_collection_close = None;
        self.state.mode = if self.state.active.is_some() {
            Mode::Terminal
        } else {
            Mode::Navigate
        };
        true
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
                self.state.mode = Mode::Terminal;
                return;
            }
            _ => return,
        };
        let Some(pending) = self.state.pending_collection_close.take() else {
            self.state.mode = Mode::Terminal;
            return;
        };
        let Some((ws_idx, tab_idx)) = self.pending_collection_matches(&pending) else {
            self.reopen_pending_collection_close(&pending);
            return;
        };
        if !pending.cleanup_archive
            && disposition == Some(CollectionCloseDisposition::CascadeClose)
            && self.state.workspaces[ws_idx].tabs.len() == 1
            && self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .leaf_count()
                == 1
        {
            self.state.pending_collection_close = Some(pending.clone());
            if self.begin_pending_collection_group_close(ws_idx) {
                return;
            }
            self.state.pending_collection_close = None;
        }
        if pending.cleanup_archive {
            for pane_id in pending.member_ids.iter().copied() {
                if let Some(public) = self.public_pane_id(ws_idx, pane_id) {
                    let _ = self.dispatch_runtime_mutation(
                        "tui.collection.archive_cleanup",
                        Method::PaneClose(crate::api::schema::PaneTarget { pane_id: public }),
                    );
                }
            }
        } else {
            let _ = self.dispatch_runtime_mutation(
                "tui.collection.close",
                Method::CollectionClose(CollectionCloseParams {
                    collection_id: format!("collection_{}", pending.collection_id.raw()),
                    disposition,
                    target_pane_id: None,
                    focus_promoted: false,
                }),
            );
            self.state.collection_views.remove(&pending.collection_id);
        }
        self.state.mode = Mode::Terminal;
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

    fn reorder_relative(&mut self, collection_id: CollectionId, delta: isize) {
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
    fn resize_selected_preview(&mut self, collection_id: CollectionId, delta: i16) {
        let Some((_, _, Some(pane))) = self.focused_collection() else {
            return;
        };
        let view = self
            .state
            .collection_views
            .entry(collection_id)
            .or_default();
        view.expanded.insert(pane);
        let next = (view.preview_height(pane) as i16 + delta).max(1) as u16;
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
    ) -> Option<(Rect, u16, u16)> {
        let layout = self
            .state
            .view
            .collection_layouts
            .iter()
            .find(|layout| layout.id == collection_id)?;
        if layout.maximized == Some(pane_id) {
            let rect = layout.maximized_preview_rect.unwrap_or(layout.inner_rect);
            return Some((rect, 0, rect.height));
        }
        let row = layout.rows.iter().find(|row| row.pane_id == pane_id)?;
        Some((
            row.preview_rect?,
            row.preview_row_offset,
            row.preview_size?.0,
        ))
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
        let Some((rect, row_offset, _)) =
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
        let (_, clipped_row_offset, logical_rows) =
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

    pub(crate) fn handle_collection_mouse(&mut self, mouse: MouseEvent) -> bool {
        let hit = self
            .state
            .view
            .collection_layouts
            .iter()
            .find_map(|layout| layout.hit_at(mouse.column, mouse.row));
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
                        view.set_preview_height(pane, (start_height as i32 + delta).max(1) as u16);
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
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let child_scrollbar = hit.kind == CollectionHitKind::PreviewScrollbar;
                let entered_preview = hit.kind == CollectionHitKind::Preview
                    && self
                        .state
                        .collection_views
                        .get(&hit.collection_id)
                        .is_some_and(|v| {
                            v.mode == CollectionInteractionMode::Terminal
                                && v.entered == hit.pane_id
                        });
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
            MouseEventKind::Down(MouseButton::Left) => {
                // A new left-button gesture owns exactly one drag surface, even if a malformed
                // event stream omitted the previous release.
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
                            let view = self
                                .state
                                .collection_views
                                .entry(hit.collection_id)
                                .or_default();
                            view.resize_drag = Some((pane, mouse.row, view.preview_height(pane)));
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
                if let Some(pane) = hit.pane_id {
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
                    });
                    self.state.mode = Mode::ContextMenu;
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
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::layout::Rect;
    use tokio::sync::mpsc;

    use crate::{
        app::AppState, layout::LayoutLeaf, terminal::TerminalRuntimeRegistry, workspace::Workspace,
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
    fn second_stage_group_close_revalidates_collection_revision_before_dispatch() {
        let (mut app, collection, child) = grouped_final_collection_app();
        app.open_collection_close_dialog(collection, false);
        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::empty(),
        ));
        assert_eq!(app.state.mode, crate::app::Mode::ConfirmClose);

        app.state.workspaces[0]
            .set_collection_member_archived(child, collection, true)
            .expect("mutate between prompts");
        assert!(app.confirm_pending_collection_group_close());

        assert_eq!(app.state.workspaces.len(), 2);
        assert_eq!(app.state.mode, crate::app::Mode::CollectionClose);
        assert!(app.state.pending_collection_close.is_some());
    }

    #[test]
    fn second_stage_group_close_revalidates_exact_group_membership_before_dispatch() {
        let (mut app, collection, _) = grouped_final_collection_app();
        app.open_collection_close_dialog(collection, false);
        app.handle_collection_close_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::empty(),
        ));
        let mut added = Workspace::test_new("added");
        added.worktree_space = Some(group_membership(true, "/repo-added"));
        app.state.workspaces.push(added);

        assert!(app.confirm_pending_collection_group_close());

        assert_eq!(app.state.workspaces.len(), 3);
        assert_eq!(app.state.mode, crate::app::Mode::CollectionClose);
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
