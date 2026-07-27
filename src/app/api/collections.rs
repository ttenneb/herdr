use crate::api::schema::{
    CollectionAddParams, CollectionCloseDisposition, CollectionCloseParams,
    CollectionCreateMemberParams, CollectionCreateMemberResult, CollectionCreateParams,
    CollectionInfo, CollectionLifecycleSummary, CollectionListParams, CollectionMemberInfo,
    CollectionMemberTarget, CollectionMoveParams, CollectionPromoteParams, CollectionReorderParams,
    CollectionSelectParams, CollectionTarget, EventData, EventEnvelope, EventKind, LayoutFocusInfo,
    PaneMoveDestination, PaneMoveParams, PanePlacementInfo, PaneTarget, ResponseResult,
};
use crate::app::App;
use crate::delegation::{DelegationId, Delegations, SiblingPosition};
use crate::layout::{CollectionId, LayoutLeaf, PanePlacement};

use super::responses::{encode_error, encode_success};

#[derive(Clone, Copy)]
pub(crate) struct ArchivedMemberInputRestore {
    ws_idx: usize,
    tab_idx: usize,
    collection_id: CollectionId,
    pane_id: crate::layout::PaneId,
    original_revision: u64,
}

impl App {
    pub(super) fn handle_collection_list(
        &self,
        id: String,
        params: CollectionListParams,
    ) -> String {
        let workspace_filter = match params.workspace_id.as_deref() {
            Some(raw) => match self.parse_workspace_id(raw) {
                Some(index) => Some(index),
                None => {
                    return encode_error(
                        id,
                        "workspace_not_found",
                        format!("workspace {raw} not found"),
                    )
                }
            },
            None => None,
        };
        let tab_filter = match params.tab_id.as_deref() {
            Some(raw) => match self.parse_tab_id(raw) {
                Some(target) => Some(target),
                None => return encode_error(id, "tab_not_found", format!("tab {raw} not found")),
            },
            None => None,
        };
        let mut collections = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            if workspace_filter.is_some_and(|wanted| wanted != ws_idx) {
                continue;
            }
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                if tab_filter.is_some_and(|wanted| wanted != (ws_idx, tab_idx)) {
                    continue;
                }
                for collection_id in tab.layout.collection_ids() {
                    if let Some(info) = self.collection_info(ws_idx, tab_idx, collection_id) {
                        collections.push(info);
                    }
                }
            }
        }
        encode_success(id, ResponseResult::CollectionList { collections })
    }

    pub(super) fn handle_collection_get(&self, id: String, target: CollectionTarget) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&target.collection_id)
        else {
            return collection_not_found(id, &target.collection_id);
        };
        let Some(collection) = self.collection_info(ws_idx, tab_idx, collection_id) else {
            return collection_not_found(id, &target.collection_id);
        };
        encode_success(id, ResponseResult::CollectionInfo { collection })
    }

    pub(super) fn handle_collection_create(
        &mut self,
        id: String,
        params: CollectionCreateParams,
    ) -> String {
        let Some((ws_idx, target_pane)) = self.parse_pane_id(&params.target_pane_id) else {
            return encode_error(id, "pane_not_found", "target pane not found");
        };
        let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(target_pane)
        else {
            return encode_error(id, "pane_not_found", "target pane not found");
        };
        let previous_focus = self.state.workspaces[ws_idx].tabs[tab_idx]
            .layout
            .focused_leaf();
        let direction = split_direction(params.direction);
        let target = match self.state.workspaces[ws_idx].tabs[tab_idx].pane_placement(target_pane) {
            Some(PanePlacement::Tiled) => LayoutLeaf::Pane(target_pane),
            Some(PanePlacement::Collection(collection)) => LayoutLeaf::Collection(collection),
            None => return encode_error(id, "pane_not_found", "target pane not found"),
        };
        let result = self.state.workspaces[ws_idx].create_collection_near(
            tab_idx,
            target,
            direction,
            params.ratio.unwrap_or(0.5),
            params.label,
        );
        let collection_id = match result {
            Ok(value) => value,
            Err(err) => return encode_error(id, "collection_create_failed", format!("{err:?}")),
        };
        if params.focus {
            let _ = self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .focus_leaf(LayoutLeaf::Collection(collection_id));
            self.state.switch_workspace_tab(ws_idx, tab_idx);
        } else {
            let _ = self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .focus_leaf(previous_focus);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let collection = self
            .collection_info(ws_idx, tab_idx, collection_id)
            .expect("created collection exists");
        self.emit_collection_event(
            EventKind::CollectionCreated,
            EventData::CollectionCreated {
                collection: collection.clone(),
            },
        );
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::CollectionInfo { collection })
    }

    pub(super) fn handle_collection_add(
        &mut self,
        id: String,
        params: CollectionAddParams,
    ) -> String {
        if self.resolve_collection(&params.collection_id).is_none() {
            return collection_not_found(id, &params.collection_id);
        }
        let Some((pane_ws, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(pane_tab) = self.state.workspaces[pane_ws].find_tab_index_for_pane(pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        if self.state.workspaces[pane_ws].tabs[pane_tab].pane_placement(pane_id)
            != Some(PanePlacement::Tiled)
        {
            return encode_error(id, "collection_add_failed", "pane is not tiled");
        }
        let response = self.handle_pane_move(
            id.clone(),
            PaneMoveParams {
                pane_id: params.pane_id,
                destination: PaneMoveDestination::Collection {
                    collection_id: params.collection_id.clone(),
                },
                focus: false,
            },
        );
        match serde_json::from_str::<crate::api::schema::SuccessResponse>(&response) {
            Ok(crate::api::schema::SuccessResponse {
                result: ResponseResult::PaneMove { move_result },
                ..
            }) if move_result.changed => {}
            Ok(_) => {
                return encode_error(
                    id,
                    "collection_add_unchanged",
                    "pane placement was unchanged",
                )
            }
            Err(_) => return response,
        }
        self.handle_collection_get(
            id,
            CollectionTarget {
                collection_id: params.collection_id,
            },
        )
    }

    pub(super) fn handle_collection_move(
        &mut self,
        id: String,
        params: CollectionMoveParams,
    ) -> String {
        let Some((_target_ws, _target_tab, _target_collection)) =
            self.resolve_collection(&params.collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        let Some((pane_ws, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(pane_tab) = self.state.workspaces[pane_ws].find_tab_index_for_pane(pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(PanePlacement::Collection(_source)) =
            self.state.workspaces[pane_ws].tabs[pane_tab].pane_placement(pane_id)
        else {
            return encode_error(
                id,
                "collection_member_not_found",
                "pane is not a collection member",
            );
        };
        let response = self.handle_pane_move(
            id.clone(),
            PaneMoveParams {
                pane_id: params.pane_id,
                destination: PaneMoveDestination::Collection {
                    collection_id: params.collection_id.clone(),
                },
                focus: false,
            },
        );
        match serde_json::from_str::<crate::api::schema::SuccessResponse>(&response) {
            Ok(crate::api::schema::SuccessResponse {
                result: ResponseResult::PaneMove { move_result },
                ..
            }) if move_result.changed => {}
            Ok(_) => {
                return encode_error(
                    id,
                    "collection_move_unchanged",
                    "pane placement was unchanged",
                )
            }
            Err(_) => return response,
        }
        self.handle_collection_get(
            id,
            CollectionTarget {
                collection_id: params.collection_id,
            },
        )
    }

    pub(crate) fn handle_collection_promote(
        &mut self,
        id: String,
        params: CollectionPromoteParams,
    ) -> String {
        let Some((ws_idx, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some((target_ws, target)) = self.parse_pane_id(&params.target_pane_id) else {
            return encode_error(id, "target_pane_not_found", "target pane not found");
        };
        if ws_idx != target_ws {
            return encode_error(
                id,
                "collection_promote_failed",
                "target must be in the same tab",
            );
        }
        let Some(tab_idx) = self.state.workspaces[ws_idx].find_tab_index_for_pane(pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        if self.state.workspaces[ws_idx].find_tab_index_for_pane(target) != Some(tab_idx) {
            return encode_error(
                id,
                "collection_promote_failed",
                "target must be in the same tab",
            );
        }
        let Some(PanePlacement::Collection(collection_id)) =
            self.state.workspaces[ws_idx].tabs[tab_idx].pane_placement(pane_id)
        else {
            return encode_error(
                id,
                "collection_member_not_found",
                "pane is not a collection member",
            );
        };
        if let Err(err) = self.state.workspaces[ws_idx].promote_collection_member_near(
            pane_id,
            collection_id,
            target,
            split_direction(params.direction),
            params.ratio.unwrap_or(0.5),
        ) {
            return encode_error(id, "collection_promote_failed", format!("{err:?}"));
        }
        self.state.collection_archive_times.remove(&pane_id);
        if params.focus {
            self.state.focus_pane_in_workspace(ws_idx, pane_id);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let pane = self
            .pane_info(ws_idx, pane_id)
            .expect("promoted pane exists");
        self.emit_collection_event(
            EventKind::CollectionMemberPromoted,
            EventData::CollectionMemberPromoted {
                collection_id: collection_id_string(collection_id),
                pane: pane.clone(),
            },
        );
        self.emit_layout_updated_event(ws_idx, tab_idx);
        encode_success(id, ResponseResult::PaneInfo { pane })
    }

    pub(super) fn handle_collection_select(
        &mut self,
        id: String,
        params: CollectionSelectParams,
    ) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&params.collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        let Some((pane_ws, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        if pane_ws != ws_idx {
            return encode_error(
                id,
                "collection_member_not_found",
                "pane is not in collection",
            );
        }
        if let Err(err) =
            self.state.workspaces[ws_idx].select_collection_member(pane_id, collection_id)
        {
            return encode_error(id, "collection_select_failed", format!("{err:?}"));
        }
        if params.focus {
            let _ = self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .focus_leaf(LayoutLeaf::Collection(collection_id));
            self.state.switch_workspace_tab(ws_idx, tab_idx);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let collection = self
            .collection_info(ws_idx, tab_idx, collection_id)
            .expect("collection exists");
        self.emit_collection_event(
            EventKind::CollectionMemberSelected,
            EventData::CollectionMemberSelected {
                collection: collection.clone(),
                pane_id: params.pane_id,
            },
        );
        encode_success(id, ResponseResult::CollectionInfo { collection })
    }

    pub(super) fn handle_collection_reorder(
        &mut self,
        id: String,
        params: CollectionReorderParams,
    ) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&params.collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        let Some((pane_ws, pane_id)) = self.parse_pane_id(&params.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        let Some(collection) =
            self.state.workspaces[ws_idx].tabs[tab_idx].collection(collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        if pane_ws != ws_idx || !collection.members().contains(&pane_id) {
            return encode_error(id, "collection_reorder_failed", "pane is not a member");
        }

        if let Some(record) = self.state.delegations.delegation_for_pane(pane_id).cloned() {
            let displayed = self.canonical_collection_member_order(collection);
            let Some(current) = displayed.iter().position(|candidate| *candidate == pane_id) else {
                return encode_error(id, "collection_reorder_failed", "pane is not displayed");
            };
            let mut subtree_ids = self
                .state
                .delegations
                .descendants(record.id)
                .unwrap_or_default()
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            subtree_ids.insert(record.id);
            let moving_subtree = displayed
                .iter()
                .copied()
                .filter(|candidate| {
                    self.state
                        .delegations
                        .delegation_for_pane(*candidate)
                        .is_some_and(|delegation| subtree_ids.contains(&delegation.id))
                })
                .collect::<std::collections::HashSet<_>>();
            let remaining = displayed
                .iter()
                .copied()
                .filter(|candidate| !moving_subtree.contains(candidate))
                .collect::<Vec<_>>();
            if params.index > remaining.len() {
                return encode_error(
                    id,
                    "collection_reorder_failed",
                    format!(
                        "final displayed index {} is out of range after removing the moving subtree (valid range: 0..={}); choose the final index of the subtree root",
                        params.index,
                        remaining.len()
                    ),
                );
            }
            if params.index != current {
                let archived = collection.is_archived(pane_id);
                let visible_siblings = displayed.iter().filter_map(|candidate| {
                    let sibling = self.state.delegations.delegation_for_pane(*candidate)?;
                    (sibling.id != record.id
                        && sibling.parent_id == record.parent_id
                        && collection.is_archived(*candidate) == archived)
                        .then_some(sibling.id)
                });
                let mut positions = vec![SiblingPosition::First, SiblingPosition::Last];
                for sibling in visible_siblings {
                    positions.push(SiblingPosition::Before(sibling));
                    positions.push(SiblingPosition::After(sibling));
                }

                let mut valid_indices = std::collections::BTreeSet::new();
                let mut selected_position = None;
                for position in positions {
                    let mut candidate_delegations = self.state.delegations.clone();
                    if candidate_delegations.reorder(record.id, position).is_err() {
                        continue;
                    }
                    let candidate_order = Self::canonical_collection_member_order_with(
                        collection,
                        &candidate_delegations,
                    );
                    let Some(candidate_index) = candidate_order
                        .iter()
                        .position(|candidate| *candidate == pane_id)
                    else {
                        continue;
                    };
                    valid_indices.insert(candidate_index);
                    if candidate_index == params.index {
                        selected_position = Some(position);
                        break;
                    }
                }
                let Some(position) = selected_position else {
                    return encode_error(
                        id,
                        "collection_reorder_requires_reparent",
                        format!(
                            "final displayed index {} is not a same-parent, same-section sibling-subtree boundary (valid final indices: {}); target another visible sibling boundary or use delegation.reparent to change parentage",
                            params.index,
                            valid_indices
                                .iter()
                                .map(usize::to_string)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    );
                };
                if let Err(err) = self.state.delegations.reorder(record.id, position) {
                    return encode_error(id, "collection_reorder_failed", err.to_string());
                }
                let delegation = self.delegation_info(
                    self.state
                        .delegations
                        .get(record.id)
                        .expect("reordered delegation exists"),
                );
                self.emit_event(EventEnvelope {
                    event: EventKind::DelegationReordered,
                    data: EventData::DelegationReordered { delegation },
                });
            }
        } else if !self.state.workspaces[ws_idx].tabs[tab_idx]
            .layout
            .reorder_collection_member(collection_id, pane_id, params.index)
        {
            return encode_error(
                id,
                "collection_reorder_failed",
                "index is out of range for the collection member vector",
            );
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let collection = self
            .collection_info(ws_idx, tab_idx, collection_id)
            .expect("collection exists");
        self.emit_collection_event(
            EventKind::CollectionMembersReordered,
            EventData::CollectionMembersReordered {
                collection: collection.clone(),
            },
        );
        encode_success(id, ResponseResult::CollectionInfo { collection })
    }

    pub(crate) fn handle_collection_archive(
        &mut self,
        id: String,
        target: CollectionMemberTarget,
        archived: bool,
    ) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&target.collection_id)
        else {
            return collection_not_found(id, &target.collection_id);
        };
        let Some((pane_ws, pane_id)) = self.parse_pane_id(&target.pane_id) else {
            return encode_error(id, "pane_not_found", "pane not found");
        };
        if pane_ws != ws_idx
            || self.state.workspaces[ws_idx]
                .set_collection_member_archived(pane_id, collection_id, archived)
                .is_err()
        {
            return encode_error(
                id,
                "collection_member_not_found",
                "pane is not a member of collection",
            );
        }
        if archived {
            self.state
                .collection_archive_times
                .entry(pane_id)
                .or_insert_with(std::time::SystemTime::now);
        } else {
            self.state.collection_archive_times.remove(&pane_id);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let collection = self
            .collection_info(ws_idx, tab_idx, collection_id)
            .expect("collection exists");
        self.emit_collection_event(
            if archived {
                EventKind::CollectionMemberArchived
            } else {
                EventKind::CollectionMemberRestored
            },
            if archived {
                EventData::CollectionMemberArchived {
                    collection: collection.clone(),
                    pane_id: target.pane_id,
                }
            } else {
                EventData::CollectionMemberRestored {
                    collection: collection.clone(),
                    pane_id: target.pane_id,
                }
            },
        );
        encode_success(id, ResponseResult::CollectionInfo { collection })
    }

    pub(super) fn handle_collection_close(
        &mut self,
        id: String,
        params: CollectionCloseParams,
    ) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&params.collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        let closed_workspace_id = self.public_workspace_id(ws_idx);
        let closed_tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_default();
        let mut layout_update_tab_idx = Some(tab_idx);
        let members = self.state.workspaces[ws_idx].tabs[tab_idx]
            .collection(collection_id)
            .map(|c| c.members().to_vec())
            .unwrap_or_default();
        let member_terminal_ids: Vec<_> = members
            .iter()
            .filter_map(|pane_id| self.state.workspaces[ws_idx].terminal_id(*pane_id).cloned())
            .collect();
        let closes_workspace = self.state.workspaces[ws_idx].tabs.len() == 1
            && self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .leaf_count()
                == 1;
        let requests_cascade = members.is_empty()
            || params.disposition == Some(CollectionCloseDisposition::CascadeClose);
        if closes_workspace && requests_cascade {
            if self.state.confirm_close
                && self
                    .state
                    .workspace_close_would_close_worktree_group(ws_idx)
            {
                return encode_error(
                    id,
                    "confirmation_required",
                    "closing this collection would close a worktree group",
                );
            }
            // A final collection is workspace closure regardless of whether it has members. Keep
            // all group, collection, pane, runtime, delegation, and event cleanup centralized.
            return self.handle_workspace_close(
                id,
                crate::api::schema::WorkspaceTarget {
                    workspace_id: closed_workspace_id,
                },
            );
        }
        if members.is_empty() {
            if self.state.workspaces[ws_idx].tabs[tab_idx]
                .layout
                .leaf_count()
                == 1
            {
                let workspace_snapshot = (self.state.workspaces[ws_idx].tabs.len() == 1)
                    .then(|| self.workspace_info(ws_idx));
                let outcome =
                    match self.state.workspaces[ws_idx].cascade_close_collection(collection_id) {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            return encode_error(id, "collection_close_failed", format!("{err:?}"))
                        }
                    };
                layout_update_tab_idx = None;
                if outcome.workspace_empty {
                    self.state.workspaces.remove(ws_idx);
                    self.state.active = if self.state.workspaces.is_empty() {
                        None
                    } else {
                        Some(ws_idx.min(self.state.workspaces.len() - 1))
                    };
                    self.state.selected = self
                        .state
                        .selected
                        .min(self.state.workspaces.len().saturating_sub(1));
                }
                self.emit_event(EventEnvelope {
                    event: EventKind::TabClosed,
                    data: EventData::TabClosed {
                        tab_id: closed_tab_id.clone(),
                        workspace_id: closed_workspace_id.clone(),
                    },
                });
                if let Some(workspace) = workspace_snapshot {
                    self.emit_event(EventEnvelope {
                        event: EventKind::WorkspaceClosed,
                        data: EventData::WorkspaceClosed {
                            workspace_id: closed_workspace_id.clone(),
                            workspace: Some(workspace),
                        },
                    });
                }
            } else if let Err(err) =
                self.state.workspaces[ws_idx].remove_empty_collection(collection_id)
            {
                return encode_error(id, "collection_close_failed", format!("{err:?}"));
            }
        } else {
            let Some(disposition) = params.disposition else {
                return encode_error(
                    id,
                    "collection_disposition_required",
                    "non-empty collection close requires cascade_close or promote_members",
                );
            };
            match disposition {
                CollectionCloseDisposition::CascadeClose => {
                    let public_members = members
                        .iter()
                        .map(|pane_id| {
                            self.public_pane_id(ws_idx, *pane_id)
                                .map(|public| (*pane_id, public))
                        })
                        .collect::<Option<Vec<_>>>();
                    let Some(public_members) = public_members else {
                        return encode_error(
                            id,
                            "collection_close_failed",
                            "collection contains an unaddressable pane",
                        );
                    };
                    let workspace_id = self.public_workspace_id(ws_idx);
                    let tab_id = self.public_tab_id(ws_idx, tab_idx).unwrap_or_default();
                    let workspace_snapshot = None;
                    let outcome = match self.state.workspaces[ws_idx]
                        .cascade_close_collection(collection_id)
                    {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            return encode_error(id, "collection_close_failed", format!("{err:?}"))
                        }
                    };
                    let destruction = self
                        .state
                        .finalize_pane_destruction(public_members.iter().map(|(pane, _)| *pane));
                    self.state.remove_unattached_terminal_ids(
                        outcome
                            .detached
                            .iter()
                            .map(|(_, terminal_id)| terminal_id.clone()),
                    );
                    if outcome.workspace_empty {
                        layout_update_tab_idx = None;
                        self.state.workspaces.remove(ws_idx);
                        self.state.active = if self.state.workspaces.is_empty() {
                            None
                        } else {
                            Some(ws_idx.min(self.state.workspaces.len() - 1))
                        };
                        self.state.selected = self
                            .state
                            .selected
                            .min(self.state.workspaces.len().saturating_sub(1));
                    }
                    self.shutdown_detached_terminal_runtimes();
                    for (_, pane_id) in &public_members {
                        self.emit_event(EventEnvelope {
                            event: EventKind::CollectionMemberRemoved,
                            data: EventData::CollectionMemberRemoved {
                                collection_id: params.collection_id.clone(),
                                pane_id: pane_id.clone(),
                            },
                        });
                        self.emit_event(EventEnvelope {
                            event: EventKind::PaneClosed,
                            data: EventData::PaneClosed {
                                pane_id: pane_id.clone(),
                                workspace_id: workspace_id.clone(),
                            },
                        });
                    }
                    for (delegation_id, pane_id) in destruction.tombstoned_delegations {
                        let public_pane_id = public_members
                            .iter()
                            .find_map(|(pane, public)| (*pane == pane_id).then(|| public.clone()))
                            .unwrap_or_default();
                        self.emit_event(EventEnvelope {
                            event: EventKind::DelegationTombstoned,
                            data: EventData::DelegationTombstoned {
                                delegation_id: delegation_id.to_string(),
                                pane_id: public_pane_id,
                            },
                        });
                    }
                    for delegation_id in destruction.garbage_collected_delegations {
                        self.emit_event(EventEnvelope {
                            event: EventKind::DelegationGarbageCollected,
                            data: EventData::DelegationGarbageCollected {
                                delegation_id: delegation_id.to_string(),
                            },
                        });
                    }
                    if outcome.removed_tab_idx.is_some() {
                        layout_update_tab_idx = None;
                        self.emit_event(EventEnvelope {
                            event: EventKind::TabClosed,
                            data: EventData::TabClosed {
                                tab_id,
                                workspace_id: workspace_id.clone(),
                            },
                        });
                    }
                    if let Some(workspace) = workspace_snapshot {
                        self.emit_event(EventEnvelope {
                            event: EventKind::WorkspaceClosed,
                            data: EventData::WorkspaceClosed {
                                workspace_id,
                                workspace: Some(workspace),
                            },
                        });
                    }
                }
                CollectionCloseDisposition::PromoteMembers => {
                    let explicit_target = match params.target_pane_id.as_deref() {
                        Some(raw) => {
                            let Some((target_ws, pane_id)) = self.parse_pane_id(raw) else {
                                return encode_error(
                                    id,
                                    "target_pane_not_found",
                                    format!("target pane {raw} not found"),
                                );
                            };
                            if target_ws != ws_idx
                                || self.state.workspaces[ws_idx].find_tab_index_for_pane(pane_id)
                                    != Some(tab_idx)
                                || self.state.workspaces[ws_idx].tabs[tab_idx]
                                    .pane_placement(pane_id)
                                    != Some(PanePlacement::Tiled)
                            {
                                return encode_error(
                                    id,
                                    "collection_promote_failed",
                                    "promotion target must be a tiled pane in the collection tab",
                                );
                            }
                            Some(pane_id)
                        }
                        None => self.state.workspaces[ws_idx].tabs[tab_idx]
                            .layout
                            .tiled_pane_ids()
                            .into_iter()
                            .next(),
                    };
                    let source_tab_id = closed_tab_id.clone();
                    let outcome = match self.state.workspaces[ws_idx]
                        .promote_all_collection_members(
                            collection_id,
                            explicit_target,
                            ratatui::layout::Direction::Horizontal,
                            0.5,
                        ) {
                        Ok(outcome) => outcome,
                        Err(err) => {
                            return encode_error(id, "collection_close_failed", format!("{err:?}"))
                        }
                    };
                    layout_update_tab_idx = Some(outcome.target_tab_idx);
                    for pane_id in &outcome.members {
                        self.state.collection_archive_times.remove(pane_id);
                    }
                    if params.focus_promoted {
                        if let Some(first) = outcome.members.first().copied() {
                            self.state.focus_pane_in_workspace(ws_idx, first);
                        }
                    }
                    for pane_id in &outcome.members {
                        if let Some(pane) = self.pane_info(ws_idx, *pane_id) {
                            self.emit_collection_event(
                                EventKind::CollectionMemberPromoted,
                                EventData::CollectionMemberPromoted {
                                    collection_id: params.collection_id.clone(),
                                    pane,
                                },
                            );
                        }
                    }
                    if outcome.removed_tab_idx.is_some() {
                        self.emit_event(EventEnvelope {
                            event: EventKind::TabClosed,
                            data: EventData::TabClosed {
                                tab_id: source_tab_id,
                                workspace_id: closed_workspace_id.clone(),
                            },
                        });
                    }
                    if outcome.created_tab {
                        if let Some(tab) = self.tab_info(ws_idx, outcome.target_tab_idx) {
                            self.emit_event(EventEnvelope {
                                event: EventKind::TabCreated,
                                data: EventData::TabCreated { tab },
                            });
                        }
                    }
                }
            }
        }
        // Presentation state is keyed by stable collection/terminal IDs and must not survive
        // collection removal or promotion. Server-owned per-client maps are pruned after this API
        // request by the headless runtime.
        self.state.collection_views.remove(&collection_id);
        for terminal_id in member_terminal_ids {
            self.state.collection_geometry.remove(&terminal_id);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_collection_event(
            EventKind::CollectionClosed,
            EventData::CollectionClosed {
                collection_id: params.collection_id,
                workspace_id: closed_workspace_id,
                tab_id: closed_tab_id,
            },
        );
        if let Some(tab_idx) = layout_update_tab_idx {
            self.emit_layout_updated_event(ws_idx, tab_idx);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_collection_create_member(
        &mut self,
        id: String,
        params: CollectionCreateMemberParams,
    ) -> String {
        let Some((ws_idx, tab_idx, collection_id)) = self.resolve_collection(&params.collection_id)
        else {
            return collection_not_found(id, &params.collection_id);
        };
        // Validate all delegation input before allocating a pane or spawning a process. Invalid
        // orchestration requests must be completely side-effect free (including lifecycle events).
        let parent = match params
            .delegation_parent_id
            .as_deref()
            .map(str::parse::<DelegationId>)
            .transpose()
        {
            Ok(value) => value,
            Err(err) => return encode_error(id, "invalid_delegation_id", err.to_string()),
        };
        if let Some(parent_id) = parent {
            if self.state.delegations.get(parent_id).is_none() {
                return encode_error(
                    id,
                    "delegation_create_failed",
                    format!("parent delegation {parent_id} was not found"),
                );
            }
        }
        let purpose = params
            .purpose
            .map(|value| value.trim().chars().take(200).collect::<String>())
            .filter(|value| !value.is_empty());
        let create_delegation = parent.is_some() || purpose.is_some();
        let extra_env = match super::env::normalize_launch_env(params.env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        let (_, estimated_cols) = self.state.estimate_pane_size();
        let collection_cols = self.state.workspaces[ws_idx].tabs[tab_idx]
            .layout
            .leaf_rect(
                LayoutLeaf::Collection(collection_id),
                self.state.view.terminal_area,
            )
            .map(|rect| rect.width.saturating_sub(2))
            .filter(|cols| *cols > 0)
            .unwrap_or(estimated_cols);
        let follow_cwd = self.state.workspaces[ws_idx].tabs[tab_idx]
            .collection(collection_id)
            .and_then(|collection| collection.selected())
            .and_then(|pane_id| self.launch_cwd_for_pane_in_workspace(ws_idx, pane_id));
        let cwd = params
            .cwd
            .map(std::path::PathBuf::from)
            .or_else(|| Some(self.resolve_new_terminal_cwd(follow_cwd)));
        let shell_config =
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode);
        let new_pane = match self.state.workspaces[ws_idx].create_collection_member(
            tab_idx,
            collection_id,
            crate::app::collection_view::DEFAULT_PREVIEW_HEIGHT,
            collection_cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            shell_config,
            extra_env,
        ) {
            Ok(new_pane) => new_pane,
            Err(err) => {
                return encode_error(id, "collection_create_member_failed", err.to_string())
            }
        };
        let pane_id = new_pane.pane_id;
        self.terminal_runtimes
            .insert(new_pane.terminal.id.clone(), new_pane.runtime);
        self.state
            .remove_alias_shadowed_by_new_pane(new_pane.pane_id);
        self.state
            .terminals
            .insert(new_pane.terminal.id.clone(), new_pane.terminal);
        let delegation_id = if create_delegation {
            match self
                .state
                .delegations
                .create(Some(pane_id), parent, purpose)
            {
                Ok(value) => Some(value),
                Err(err) => {
                    // Parent and pane association were prevalidated, so only ID exhaustion can
                    // reach this path. Close the just-created process rather than expose a member
                    // without the requested provenance.
                    let public = self.public_pane_id(ws_idx, pane_id).unwrap_or_default();
                    let _ =
                        self.close_pane(format!("{id}:rollback"), &PaneTarget { pane_id: public });
                    return encode_error(id, "delegation_create_failed", err.to_string());
                }
            }
        } else {
            None
        };
        let pane = self
            .pane_info(ws_idx, pane_id)
            .expect("created member exists");
        self.emit_event(EventEnvelope {
            event: EventKind::PaneCreated,
            data: EventData::PaneCreated { pane: pane.clone() },
        });
        if let Some(delegation_id) = delegation_id {
            let delegation = self.delegation_info(
                self.state
                    .delegations
                    .get(delegation_id)
                    .expect("created delegation exists"),
            );
            self.emit_event(EventEnvelope {
                event: EventKind::DelegationCreated,
                data: EventData::DelegationCreated { delegation },
            });
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let collection = self
            .collection_info(ws_idx, tab_idx, collection_id)
            .expect("collection exists");
        self.emit_collection_event(
            EventKind::CollectionMemberAdded,
            EventData::CollectionMemberAdded {
                collection: collection.clone(),
                pane: pane.clone(),
            },
        );
        encode_success(
            id,
            ResponseResult::CollectionMemberCreated {
                created: Box::new(CollectionCreateMemberResult {
                    collection,
                    pane,
                    delegation_id: delegation_id.map(|value| value.to_string()),
                }),
            },
        )
    }

    fn canonical_collection_member_order(
        &self,
        collection: &crate::layout::PaneCollection,
    ) -> Vec<crate::layout::PaneId> {
        Self::canonical_collection_member_order_with(collection, &self.state.delegations)
    }

    fn canonical_collection_member_order_with(
        collection: &crate::layout::PaneCollection,
        delegations: &Delegations,
    ) -> Vec<crate::layout::PaneId> {
        let mut ordered = Vec::with_capacity(collection.members().len());
        for archived in [false, true] {
            let section = collection
                .members()
                .iter()
                .copied()
                .filter(|pane| collection.is_archived(*pane) == archived)
                .collect::<Vec<_>>();
            let section_set = section.iter().copied().collect();
            let mut included = std::collections::HashSet::new();
            for entry in delegations.preorder_for_panes(&section_set) {
                if let Some(pane) = delegations.get(entry.id).and_then(|record| record.pane_id) {
                    included.insert(pane);
                    ordered.push(pane);
                }
            }
            ordered.extend(section.into_iter().filter(|pane| !included.contains(pane)));
        }
        ordered
    }

    pub(super) fn collection_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        collection_id: CollectionId,
    ) -> Option<CollectionInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let tab = ws.tabs.get(tab_idx)?;
        let collection = tab.collection(collection_id)?;
        let members: Vec<_> = self
            .canonical_collection_member_order(collection)
            .into_iter()
            .enumerate()
            .filter_map(|(index, pane)| {
                Some(CollectionMemberInfo {
                    pane_id: self.public_pane_id(ws_idx, pane)?,
                    index,
                    archived: collection.is_archived(pane),
                    selected: collection.selected() == Some(pane),
                })
            })
            .collect();
        let archived = collection.archived_members().count();
        let active = collection.members().len().saturating_sub(archived);
        let mut live = 0usize;
        let mut working = 0usize;
        let mut blocked = 0usize;
        for pane_id in collection.members() {
            let Some(pane) = tab.panes.get(pane_id) else {
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
        let lifecycle = CollectionLifecycleSummary {
            active,
            archived,
            live,
            working,
            blocked,
            exited: collection.members().len().saturating_sub(live),
        };
        let policy = self.state.collection_lifecycle;
        let mut warnings = Vec::new();
        if policy.archive_count > 0 && archived > policy.archive_count {
            warnings.push(format!(
                "archive count {archived} exceeds advisory limit {}",
                policy.archive_count
            ));
        }
        if policy.concurrency > 0 && working.saturating_add(blocked) > policy.concurrency {
            warnings.push(format!(
                "working/blocked concurrency {} exceeds advisory limit {}",
                working.saturating_add(blocked),
                policy.concurrency
            ));
        }
        if policy.archive_age_days > 0 {
            let age_limit = std::time::Duration::from_secs(
                policy.archive_age_days.saturating_mul(24 * 60 * 60),
            );
            if collection.archived_members().any(|pane| {
                self.state
                    .collection_archive_times
                    .get(&pane)
                    .and_then(|archived_at| archived_at.elapsed().ok())
                    .is_some_and(|age| age >= age_limit)
            }) {
                warnings.push(format!(
                    "archive contains members older than advisory limit of {} days",
                    policy.archive_age_days
                ));
            }
        }
        Some(CollectionInfo {
            collection_id: collection_id_string(collection_id),
            workspace_id: self.public_workspace_id(ws_idx),
            tab_id: self.public_tab_id(ws_idx, tab_idx)?,
            label: collection.label.clone(),
            focused: self.state.active == Some(ws_idx)
                && ws.active_tab == tab_idx
                && tab.layout.focused_leaf() == LayoutLeaf::Collection(collection_id),
            selected_pane_id: collection
                .selected()
                .and_then(|pane| self.public_pane_id(ws_idx, pane)),
            members,
            lifecycle,
            warnings,
        })
    }

    pub(crate) fn pane_placement_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<PanePlacementInfo> {
        let tab = self.state.workspaces.get(ws_idx)?.tabs.get(tab_idx)?;
        match tab.pane_placement(pane_id)? {
            PanePlacement::Tiled => Some(PanePlacementInfo::Tiled),
            PanePlacement::Collection(collection_id) => {
                let collection = tab.collection(collection_id)?;
                let member_index = collection
                    .members()
                    .iter()
                    .position(|member| *member == pane_id)?;
                Some(PanePlacementInfo::Collection {
                    collection_id: collection_id_string(collection_id),
                    member_index,
                    archived: collection.is_archived(pane_id),
                    selected: collection.selected() == Some(pane_id),
                })
            }
        }
    }

    pub(super) fn layout_focus_info(
        &self,
        ws_idx: usize,
        tab_idx: usize,
    ) -> Option<LayoutFocusInfo> {
        let tab = self.state.workspaces.get(ws_idx)?.tabs.get(tab_idx)?;
        match tab.layout.focused_leaf() {
            LayoutLeaf::Pane(pane) => Some(LayoutFocusInfo::Pane {
                pane_id: self.public_pane_id(ws_idx, pane)?,
            }),
            LayoutLeaf::Collection(collection) => Some(LayoutFocusInfo::Collection {
                collection_id: collection_id_string(collection),
                selected_pane_id: tab
                    .collection(collection)?
                    .selected()
                    .and_then(|pane| self.public_pane_id(ws_idx, pane)),
            }),
        }
    }

    /// Begin restoring an archived member for input. The caller must commit after the first
    /// successful enqueue or roll back when no input was accepted.
    pub(crate) fn begin_archived_member_input(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<ArchivedMemberInputRestore> {
        let tab_idx = self
            .state
            .workspaces
            .get(ws_idx)?
            .find_tab_index_for_pane(pane_id)?;
        let PanePlacement::Collection(collection_id) =
            self.state.workspaces[ws_idx].tabs[tab_idx].pane_placement(pane_id)?
        else {
            return None;
        };
        let collection = self.state.workspaces[ws_idx].tabs[tab_idx].collection(collection_id)?;
        let (archived, original_revision) =
            (collection.is_archived(pane_id), collection.revision());
        (archived
            && self.state.workspaces[ws_idx]
                .set_collection_member_archived(pane_id, collection_id, false)
                .is_ok())
        .then_some(ArchivedMemberInputRestore {
            ws_idx,
            tab_idx,
            collection_id,
            pane_id,
            original_revision,
        })
    }

    pub(crate) fn commit_archived_member_input(&mut self, restore: ArchivedMemberInputRestore) {
        self.state.collection_archive_times.remove(&restore.pane_id);
        self.state.mark_session_dirty();
        self.schedule_session_save();
        if let (Some(collection), Some(public_pane_id)) = (
            self.collection_info(restore.ws_idx, restore.tab_idx, restore.collection_id),
            self.public_pane_id(restore.ws_idx, restore.pane_id),
        ) {
            self.emit_collection_event(
                EventKind::CollectionMemberRestored,
                EventData::CollectionMemberRestored {
                    collection,
                    pane_id: public_pane_id,
                },
            );
        }
    }

    pub(crate) fn rollback_archived_member_input(&mut self, restore: ArchivedMemberInputRestore) {
        let _ = self.state.workspaces[restore.ws_idx].rollback_collection_member_restore(
            restore.tab_idx,
            restore.pane_id,
            restore.collection_id,
            restore.original_revision,
        );
    }

    // TUI input is already accepted before this helper is called, so it commits immediately.
    pub(crate) fn restore_archived_member_for_input(
        &mut self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) {
        if let Some(restore) = self.begin_archived_member_input(ws_idx, pane_id) {
            self.commit_archived_member_input(restore);
        }
    }

    pub(crate) fn resolve_collection(&self, raw: &str) -> Option<(usize, usize, CollectionId)> {
        let id = parse_collection_id(raw)?;
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                ws.tabs
                    .iter()
                    .enumerate()
                    .find(|(_, tab)| tab.collection(id).is_some())
                    .map(|(tab_idx, _)| (ws_idx, tab_idx, id))
            })
    }

    fn emit_collection_event(&mut self, event: EventKind, data: EventData) {
        self.emit_event(EventEnvelope { event, data });
    }
}

pub(super) fn collection_id_string(id: CollectionId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("collection_{}", id.raw()))
}
fn parse_collection_id(raw: &str) -> Option<CollectionId> {
    CollectionId::parse(raw).ok()
}
fn split_direction(direction: crate::api::schema::SplitDirection) -> ratatui::layout::Direction {
    match direction {
        crate::api::schema::SplitDirection::Right => ratatui::layout::Direction::Horizontal,
        crate::api::schema::SplitDirection::Down => ratatui::layout::Direction::Vertical,
    }
}
fn collection_not_found(id: String, collection_id: &str) -> String {
    encode_error(
        id,
        "collection_not_found",
        format!("collection {collection_id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request};
    use crate::{config::Config, workspace::Workspace};

    fn app_with_panes() -> (
        App,
        crate::layout::PaneId,
        crate::layout::PaneId,
        crate::layout::PaneId,
    ) {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("collections-api");
        let root = workspace.tabs[0].root_pane.expect("root pane");
        let second = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let third = workspace.test_split(ratatui::layout::Direction::Vertical);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        (app, root, second, third)
    }

    fn request(app: &mut App, method: Method) -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(Request {
            id: "test".into(),
            method,
        }))
        .expect("valid response")
    }

    fn create_collection_in(app: &mut App, ws_idx: usize, target: crate::layout::PaneId) -> String {
        let target_pane_id = app.public_pane_id(ws_idx, target).expect("public pane");
        let response = request(
            app,
            Method::CollectionCreate(CollectionCreateParams {
                target_pane_id,
                direction: crate::api::schema::SplitDirection::Right,
                ratio: Some(0.4),
                label: Some("helpers".into()),
                focus: false,
            }),
        );
        assert_eq!(response["result"]["type"], "collection_info");
        response["result"]["collection"]["collection_id"]
            .as_str()
            .expect("collection ID")
            .to_string()
    }

    fn create_collection(app: &mut App, target: crate::layout::PaneId) -> String {
        create_collection_in(app, 0, target)
    }

    #[tokio::test]
    async fn member_create_populates_collection_only_tab_at_standard_preview_geometry() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let event_hub = crate::api::EventHub::default();
        let mut app = App::new(&Config::default(), true, None, rx, event_hub);
        let mut workspace = Workspace::test_new("collection-only");
        let root = workspace.tabs[0].root_pane.expect("root");
        let collection = workspace
            .create_collection_near(
                0,
                crate::layout::LayoutLeaf::Pane(root),
                ratatui::layout::Direction::Horizontal,
                0.5,
                None,
            )
            .expect("collection");
        workspace.close_pane(root);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);

        let response = request(
            &mut app,
            Method::CollectionCreateMember(CollectionCreateMemberParams {
                collection_id: collection_id_string(collection),
                cwd: None,
                env: Default::default(),
                delegation_parent_id: None,
                purpose: None,
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        let pane_public = response["result"]["created"]["pane"]["pane_id"]
            .as_str()
            .expect("pane id");
        let (_, pane_id) = app.parse_pane_id(pane_public).expect("created pane");
        assert_eq!(
            app.state.workspaces[0].tabs[0].pane_placement(pane_id),
            Some(PanePlacement::Collection(collection))
        );
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal");
        let runtime = app.terminal_runtimes.get(terminal_id).expect("runtime");
        assert_eq!(
            runtime.current_size(),
            (crate::app::collection_view::DEFAULT_PREVIEW_HEIGHT, 98)
        );
    }

    #[test]
    fn invalid_member_delegation_is_rejected_before_process_or_events() {
        let (mut app, root, _, _) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        let panes_before = app.state.terminals.len();
        let events_before = app.event_hub.current_sequence();
        let response = request(
            &mut app,
            Method::CollectionCreateMember(CollectionCreateMemberParams {
                collection_id,
                cwd: None,
                env: Default::default(),
                delegation_parent_id: Some("d999999".into()),
                purpose: Some("  helper  ".into()),
            }),
        );
        assert_eq!(response["error"]["code"], "delegation_create_failed");
        assert_eq!(app.state.terminals.len(), panes_before);
        assert_eq!(app.event_hub.current_sequence(), events_before);
    }

    #[test]
    fn collection_move_reports_unchanged_with_original_request_id() {
        let (mut app, root, second, _) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        let pane_id = app.public_pane_id(0, second).expect("public pane");
        let added = request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: collection_id.clone(),
                pane_id: pane_id.clone(),
            }),
        );
        assert!(added.get("error").is_none(), "{added}");
        app.state.workspaces[0].tabs[0].zoomed = true;

        let response = request(
            &mut app,
            Method::CollectionMove(CollectionMoveParams {
                pane_id,
                collection_id,
            }),
        );
        assert_eq!(response["id"], "test");
        assert_eq!(response["error"]["code"], "collection_move_unchanged");
    }

    #[test]
    fn collection_reorder_uses_delegation_display_order_and_preserves_plain_vector_order() {
        let (mut app, root, second, third) = app_with_panes();
        let fourth = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let collection_id = create_collection(&mut app, root);
        for pane in [second, third, fourth, root] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            let response = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
            assert!(response.get("error").is_none(), "{response}");
        }
        let parent = app
            .state
            .delegations
            .create(Some(second), None, Some("parent".into()))
            .expect("parent delegation");
        app.state
            .delegations
            .create(Some(third), Some(parent), Some("child".into()))
            .expect("child delegation");
        let sibling = app
            .state
            .delegations
            .create(Some(fourth), None, Some("sibling".into()))
            .expect("sibling delegation");
        let second_public = app.public_pane_id(0, second).expect("public pane");
        let third_public = app.public_pane_id(0, third).expect("public pane");
        let fourth_public = app.public_pane_id(0, fourth).expect("public pane");
        let root_public = app.public_pane_id(0, root).expect("public pane");

        let reordered = request(
            &mut app,
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: collection_id.clone(),
                pane_id: fourth_public.clone(),
                index: 0,
            }),
        );
        let displayed = reordered["result"]["collection"]["members"]
            .as_array()
            .expect("members")
            .iter()
            .map(|member| member["pane_id"].as_str().expect("pane id"))
            .collect::<Vec<_>>();
        assert_eq!(
            displayed,
            vec![
                fourth_public.clone(),
                second_public.clone(),
                third_public.clone(),
                root_public.clone(),
            ]
        );
        assert_eq!(
            app.state
                .delegations
                .get(sibling)
                .map(|record| record.sibling_rank),
            Some(0)
        );
        let emitted = app.event_hub.events_after(0);
        let collection_event = emitted
            .iter()
            .rev()
            .find_map(|(_, event)| match &event.data {
                EventData::CollectionMembersReordered { collection } => Some(collection),
                _ => None,
            })
            .expect("collection reorder event");
        assert_eq!(
            serde_json::to_value(&collection_event.members).expect("event members"),
            reordered["result"]["collection"]["members"]
        );

        let rejected = request(
            &mut app,
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: collection_id.clone(),
                pane_id: third_public.clone(),
                index: 0,
            }),
        );
        assert_eq!(
            rejected["error"]["code"],
            "collection_reorder_requires_reparent"
        );
        assert!(rejected["error"]["message"]
            .as_str()
            .expect("message")
            .contains("delegation.reparent"));

        let plain = request(
            &mut app,
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: collection_id.clone(),
                pane_id: root_public.clone(),
                index: 0,
            }),
        );
        let collection = parse_collection_id(&collection_id).expect("collection id");
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection")
                .members()[0],
            root
        );
        assert_eq!(
            plain["result"]["collection"]["members"][3]["pane_id"],
            root_public
        );
    }

    #[test]
    fn delegated_reorder_uses_final_subtree_root_indices_at_sibling_boundaries() {
        let (mut app, root, a, a_child) = app_with_panes();
        let a_grandchild =
            app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        let b = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        let b_child = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        let c = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        let a_peer = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let collection_id = create_collection(&mut app, root);
        for pane in [a, a_child, a_grandchild, b, b_child, c, a_peer, root] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            let added = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
            assert!(added.get("error").is_none(), "{added}");
        }
        let a_id = app.state.delegations.create(Some(a), None, None).unwrap();
        let a_child_id = app
            .state
            .delegations
            .create(Some(a_child), Some(a_id), None)
            .unwrap();
        app.state
            .delegations
            .create(Some(a_grandchild), Some(a_child_id), None)
            .unwrap();
        app.state
            .delegations
            .create(Some(a_peer), Some(a_id), None)
            .unwrap();
        let b_id = app.state.delegations.create(Some(b), None, None).unwrap();
        app.state
            .delegations
            .create(Some(b_child), Some(b_id), None)
            .unwrap();
        app.state.delegations.create(Some(c), None, None).unwrap();
        let public = [a, a_child, a_grandchild, b, b_child, c, a_peer, root]
            .into_iter()
            .map(|pane| app.public_pane_id(0, pane).unwrap())
            .collect::<Vec<_>>();

        let reorder = |app: &mut App, pane_id: String, index| {
            request(
                app,
                Method::CollectionReorder(CollectionReorderParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                    index,
                }),
            )
        };
        let forward = reorder(&mut app, public[0].clone(), 2);
        assert_eq!(
            forward["result"]["collection"]["members"][2]["pane_id"],
            public[0]
        );
        assert_eq!(forward["result"]["collection"]["members"][2]["index"], 2);
        assert_eq!(
            forward["result"]["collection"]["members"][6]["pane_id"],
            public[5]
        );

        let backward = reorder(&mut app, public[5].clone(), 0);
        assert_eq!(
            backward["result"]["collection"]["members"][0]["pane_id"],
            public[5]
        );
        assert_eq!(
            backward["result"]["collection"]["members"][3]["pane_id"],
            public[0]
        );

        let nested_forward = reorder(&mut app, public[1].clone(), 5);
        assert_eq!(
            nested_forward["result"]["collection"]["members"][4]["pane_id"],
            public[6]
        );
        assert_eq!(
            nested_forward["result"]["collection"]["members"][5]["pane_id"],
            public[1]
        );
        let nested_backward = reorder(&mut app, public[1].clone(), 4);
        assert_eq!(
            nested_backward["result"]["collection"]["members"][4]["pane_id"],
            public[1]
        );
        assert_eq!(
            nested_backward["result"]["collection"]["members"][4]["index"],
            4
        );
        assert_eq!(
            nested_backward["result"]["collection"]["members"][6]["pane_id"],
            public[6]
        );

        let inside_other_subtree = reorder(&mut app, public[5].clone(), 4);
        assert_eq!(
            inside_other_subtree["error"]["code"],
            "collection_reorder_requires_reparent"
        );
        let message = inside_other_subtree["error"]["message"].as_str().unwrap();
        assert!(message.contains("sibling-subtree boundary"), "{message}");
        assert!(message.contains("valid final indices"), "{message}");

        let inside_moving_subtree = reorder(&mut app, public[0].clone(), 6);
        assert_eq!(
            inside_moving_subtree["error"]["code"],
            "collection_reorder_failed"
        );
        assert!(inside_moving_subtree["error"]["message"]
            .as_str()
            .unwrap()
            .contains("after removing the moving subtree"));

        let event = app
            .event_hub
            .events_after(0)
            .into_iter()
            .rev()
            .find_map(|(_, event)| match event.data {
                EventData::CollectionMembersReordered { collection } => Some(collection),
                _ => None,
            })
            .expect("reorder event");
        let event_members = serde_json::to_value(event.members).unwrap();
        assert_eq!(
            event_members,
            nested_backward["result"]["collection"]["members"]
        );
        assert_eq!(event_members[4]["pane_id"], public[1]);
        assert_eq!(event_members[4]["index"], 4);
    }

    #[test]
    fn tui_and_api_delegated_reorder_produce_the_same_display_order() {
        let build = || {
            let (mut app, root, second, third) = app_with_panes();
            let collection_id = create_collection(&mut app, root);
            for pane in [second, third, root] {
                let pane_id = app.public_pane_id(0, pane).expect("public pane");
                request(
                    &mut app,
                    Method::CollectionAdd(CollectionAddParams {
                        collection_id: collection_id.clone(),
                        pane_id,
                    }),
                );
            }
            let first = app
                .state
                .delegations
                .create(Some(second), None, None)
                .expect("first sibling");
            app.state
                .delegations
                .create(Some(root), Some(first), None)
                .expect("first sibling child");
            app.state
                .delegations
                .create(Some(third), None, None)
                .expect("second sibling");
            (app, collection_id, first, second, third)
        };
        let (mut api, collection_id, _, second, third) = build();
        let third_public = api.public_pane_id(0, third).expect("public pane");
        let api_result = request(
            &mut api,
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: collection_id.clone(),
                pane_id: third_public,
                index: 0,
            }),
        );
        let api_order = api_result["result"]["collection"]["members"]
            .as_array()
            .expect("API members")
            .iter()
            .map(|member| {
                member["pane_id"]
                    .as_str()
                    .expect("pane id")
                    .split(':')
                    .next_back()
                    .expect("public pane suffix")
                    .to_string()
            })
            .collect::<Vec<_>>();

        let (mut tui, tui_collection_id, _, _, tui_third) = build();
        let collection = parse_collection_id(&tui_collection_id).expect("collection id");
        tui.state.active = Some(0);
        tui.state.workspaces[0].tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection));
        tui.state.workspaces[0].tabs[0]
            .layout
            .select_collection_member(collection, tui_third);
        tui.reorder_relative(collection, -1);
        let tui_info = tui
            .collection_info(0, 0, collection)
            .expect("collection info");
        let tui_order = tui_info
            .members
            .iter()
            .map(|member| {
                member
                    .pane_id
                    .split(':')
                    .next_back()
                    .expect("public pane suffix")
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(tui_order, api_order);
        assert_eq!(
            api.collection_info(
                0,
                0,
                parse_collection_id(&collection_id).expect("collection id")
            )
            .expect("api info")
            .members[1]
                .pane_id,
            api.public_pane_id(0, second).expect("public pane")
        );
    }

    #[test]
    fn unknown_near_max_collection_requests_do_not_reserve_ids() {
        let (mut app, _root, member, _) = app_with_panes();
        let member_public = app.public_pane_id(0, member).expect("public pane");
        let before = CollectionId::alloc().expect("collection ID available");
        let unknown = "collection_18446744073709551614".to_string();

        assert!(app.resolve_collection(&unknown).is_none());
        let response = app.handle_collection_archive(
            "unknown".into(),
            CollectionMemberTarget {
                collection_id: unknown,
                pane_id: member_public,
            },
            true,
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&response).is_ok());
        let after = CollectionId::alloc().expect("unknown mutation must not exhaust allocation");
        assert_eq!(after.raw(), before.raw() + 1);
    }

    #[tokio::test]
    async fn archived_input_restores_only_after_an_enqueue_is_accepted() {
        let (mut app, root, member, _) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        let member_public = app.public_pane_id(0, member).expect("public pane");
        request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: collection_id.clone(),
                pane_id: member_public.clone(),
            }),
        );
        request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: member_public.clone(),
            }),
        );
        let archived_at = app.state.collection_archive_times[&member];
        let collection = parse_collection_id(&collection_id).expect("collection ID");
        let archive_revision = app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .revision();

        let events_before = app.event_hub.current_sequence();
        let missing = app.handle_pane_send_text(
            "missing".into(),
            crate::api::schema::PaneSendTextParams {
                pane_id: member_public.clone(),
                text: "resume".into(),
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&missing).is_ok());
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(member));
        assert_eq!(app.state.collection_archive_times[&member], archived_at);
        assert_eq!(app.event_hub.current_sequence(), events_before);

        let (runtime, _rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        runtime
            .try_send_bytes(bytes::Bytes::from_static(b"occupied"))
            .expect("fill runtime input queue");
        app.state.insert_test_runtime(member, runtime);
        let failed = app.handle_pane_send_text(
            "failed".into(),
            crate::api::schema::PaneSendTextParams {
                pane_id: member_public.clone(),
                text: "resume".into(),
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&failed).is_ok());
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(member));
        assert_eq!(app.state.collection_archive_times[&member], archived_at);
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection")
                .revision(),
            archive_revision
        );
        assert_eq!(app.event_hub.current_sequence(), events_before);

        let invalid_input = app.handle_pane_send_input(
            "invalid-input".into(),
            crate::api::schema::PaneSendInputParams {
                pane_id: member_public.clone(),
                text: "resume".into(),
                keys: vec!["not-a-key".into()],
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&invalid_input).is_ok());
        let invalid_keys = app.handle_pane_send_keys(
            "invalid-keys".into(),
            crate::api::schema::PaneSendKeysParams {
                pane_id: member_public.clone(),
                keys: vec!["not-a-key".into()],
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&invalid_keys).is_ok());
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(member));
        assert_eq!(app.state.collection_archive_times[&member], archived_at);
        assert_eq!(app.event_hub.current_sequence(), events_before);

        let (runtime, mut empty_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
        app.state.insert_test_runtime(member, runtime);
        for (id, response) in [
            (
                "empty-text",
                app.handle_pane_send_text(
                    "empty-text".into(),
                    crate::api::schema::PaneSendTextParams {
                        pane_id: member_public.clone(),
                        text: String::new(),
                    },
                ),
            ),
            (
                "empty-input",
                app.handle_pane_send_input(
                    "empty-input".into(),
                    crate::api::schema::PaneSendInputParams {
                        pane_id: member_public.clone(),
                        text: String::new(),
                        keys: Vec::new(),
                    },
                ),
            ),
            (
                "empty-keys",
                app.handle_pane_send_keys(
                    "empty-keys".into(),
                    crate::api::schema::PaneSendKeysParams {
                        pane_id: member_public.clone(),
                        keys: Vec::new(),
                    },
                ),
            ),
        ] {
            assert!(
                serde_json::from_str::<crate::api::schema::SuccessResponse>(&response).is_ok(),
                "{id} retains its successful response"
            );
            assert!(app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection")
                .is_archived(member));
            assert_eq!(app.state.collection_archive_times[&member], archived_at);
            assert_eq!(app.event_hub.current_sequence(), events_before);
        }
        assert_eq!(
            empty_rx.try_recv().expect("empty text enqueued"),
            bytes::Bytes::new()
        );
        assert_eq!(
            empty_rx.try_recv().expect("empty input enqueued"),
            bytes::Bytes::new()
        );
        assert!(empty_rx.try_recv().is_err());

        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
        app.state.insert_test_runtime(member, runtime);
        let sent = app.handle_pane_send_text(
            "sent".into(),
            crate::api::schema::PaneSendTextParams {
                pane_id: member_public,
                text: "resume".into(),
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&sent).is_ok());
        assert_eq!(
            rx.try_recv().expect("input accepted"),
            bytes::Bytes::from_static(b"resume")
        );
        assert!(!app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(member));
        assert!(!app.state.collection_archive_times.contains_key(&member));
        assert_eq!(app.event_hub.current_sequence(), events_before + 1);
        assert!(matches!(
            app.event_hub.events_after(events_before).as_slice(),
            [(
                _,
                EventEnvelope {
                    event: EventKind::CollectionMemberRestored,
                    ..
                }
            )]
        ));

        app.state.workspaces[0]
            .set_collection_member_archived(member, collection, true)
            .expect("archive member for partial delivery");
        app.state
            .collection_archive_times
            .insert(member, std::time::SystemTime::now());
        let events_before_partial = app.event_hub.current_sequence();
        let (runtime, mut partial_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 1);
        app.state.insert_test_runtime(member, runtime);
        let partial = app.handle_pane_send_keys(
            "partial".into(),
            crate::api::schema::PaneSendKeysParams {
                pane_id: app.public_pane_id(0, member).expect("public pane"),
                keys: vec!["enter".into(), "up".into()],
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::ErrorResponse>(&partial).is_ok());
        assert_eq!(
            partial_rx.try_recv().expect("first key accepted"),
            bytes::Bytes::from_static(b"\r")
        );
        assert!(!app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(member));
        assert!(!app.state.collection_archive_times.contains_key(&member));
        assert_eq!(app.event_hub.current_sequence(), events_before_partial + 1);
    }

    #[tokio::test]
    async fn collection_api_covers_membership_selection_order_archive_promotion_and_close() {
        let (mut app, root, second, third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        let second_public = app.public_pane_id(0, second).expect("second public");
        let third_public = app.public_pane_id(0, third).expect("third public");
        let root_public = app.public_pane_id(0, root).expect("root public");

        for pane_id in [&second_public, &third_public] {
            let response = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id: pane_id.clone(),
                }),
            );
            assert!(response.get("error").is_none(), "{response}");
        }
        let get = request(
            &mut app,
            Method::CollectionGet(CollectionTarget {
                collection_id: collection_id.clone(),
            }),
        );
        assert_eq!(
            get["result"]["collection"]["members"]
                .as_array()
                .expect("members")
                .len(),
            2
        );

        let other_collection = create_collection(&mut app, root);
        let moved = request(
            &mut app,
            Method::CollectionMove(CollectionMoveParams {
                pane_id: third_public.clone(),
                collection_id: other_collection,
            }),
        );
        assert_eq!(
            moved["result"]["collection"]["members"][0]["pane_id"],
            third_public
        );
        let moved_back = request(
            &mut app,
            Method::CollectionMove(CollectionMoveParams {
                pane_id: third_public.clone(),
                collection_id: collection_id.clone(),
            }),
        );
        assert_eq!(
            moved_back["result"]["collection"]["members"]
                .as_array()
                .expect("members")
                .len(),
            2
        );

        let reordered = request(
            &mut app,
            Method::CollectionReorder(CollectionReorderParams {
                collection_id: collection_id.clone(),
                pane_id: third_public.clone(),
                index: 0,
            }),
        );
        assert_eq!(
            reordered["result"]["collection"]["members"][0]["pane_id"],
            third_public
        );

        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&second)
            .expect("member")
            .seen = false;
        let selected = request(
            &mut app,
            Method::CollectionSelect(CollectionSelectParams {
                collection_id: collection_id.clone(),
                pane_id: second_public.clone(),
                focus: true,
            }),
        );
        assert_eq!(selected["result"]["collection"]["focused"], true);
        assert!(
            !app.state.workspaces[0].tabs[0].panes[&second].seen,
            "API selection must not acknowledge attention"
        );

        let archived = request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: second_public.clone(),
            }),
        );
        assert_eq!(
            archived["result"]["collection"]["members"][1]["archived"],
            true
        );
        assert!(app.state.collection_archive_times.contains_key(&second));
        let restored = request(
            &mut app,
            Method::CollectionRestore(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: second_public.clone(),
            }),
        );
        assert_eq!(
            restored["result"]["collection"]["members"][1]["archived"],
            false
        );
        assert!(!app.state.collection_archive_times.contains_key(&second));
        request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: second_public.clone(),
            }),
        );
        let (runtime, mut input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(second, runtime);
        let sent = app.handle_pane_send_text(
            "write".into(),
            crate::api::schema::PaneSendTextParams {
                pane_id: second_public.clone(),
                text: "resume".into(),
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&sent).is_ok());
        assert_eq!(
            input_rx.try_recv().expect("delivered bytes"),
            bytes::Bytes::from_static(b"resume")
        );
        assert!(!app.state.workspaces[0].tabs[0]
            .collection(parse_collection_id(&collection_id).expect("ID"))
            .expect("collection")
            .is_archived(second));
        assert!(!app.state.collection_archive_times.contains_key(&second));

        let promoted = request(
            &mut app,
            Method::CollectionPromote(CollectionPromoteParams {
                pane_id: third_public,
                target_pane_id: root_public.clone(),
                direction: crate::api::schema::SplitDirection::Down,
                ratio: Some(0.5),
                focus: false,
            }),
        );
        assert_eq!(promoted["result"]["pane"]["placement"]["type"], "tiled");

        let missing_disposition = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id: collection_id.clone(),
                disposition: None,
                target_pane_id: None,
                focus_promoted: false,
            }),
        );
        assert_eq!(
            missing_disposition["error"]["code"],
            "collection_disposition_required"
        );
        request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: second_public,
            }),
        );
        assert!(app.state.collection_archive_times.contains_key(&second));
        let raw_collection = parse_collection_id(&collection_id).expect("collection");
        app.state
            .collection_views
            .entry(raw_collection)
            .or_default()
            .expanded
            .insert(second);
        let terminal_id = app.state.workspaces[0]
            .terminal_id(second)
            .expect("terminal")
            .clone();
        app.state.collection_geometry.insert(
            terminal_id,
            crate::app::collection_view::TerminalGeometry {
                rows: 8,
                cols: 40,
                cell_width_px: 1,
                cell_height_px: 1,
            },
        );
        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id: collection_id.clone(),
                disposition: Some(CollectionCloseDisposition::PromoteMembers),
                target_pane_id: Some(root_public),
                focus_promoted: false,
            }),
        );
        assert_eq!(closed["result"]["type"], "ok");
        assert!(app.resolve_collection(&collection_id).is_none());
        assert_eq!(
            app.state.workspaces[0].tabs[0].pane_placement(second),
            Some(PanePlacement::Tiled)
        );
        assert!(!app.state.collection_archive_times.contains_key(&second));
        assert!(!app.state.collection_views.contains_key(&raw_collection));
        assert!(app.state.collection_geometry.is_empty());
    }

    #[test]
    fn working_state_restores_archived_member_without_changing_focus() {
        let (mut app, root, second, _) = app_with_panes();
        app.state.active = Some(0);
        let collection_id = create_collection(&mut app, root);
        let second_public = app.public_pane_id(0, second).expect("public pane");
        request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: collection_id.clone(),
                pane_id: second_public.clone(),
            }),
        );
        request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: collection_id.clone(),
                pane_id: second_public,
            }),
        );
        app.state.workspaces[0].tabs[0]
            .layout
            .focus_leaf(crate::layout::LayoutLeaf::Pane(root));
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: second,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Working,
            visible_blocker: false,
            visible_working: true,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        let (_, tab_idx, collection) = app.resolve_collection(&collection_id).expect("collection");
        assert!(!app.state.workspaces[0].tabs[tab_idx]
            .collection(collection)
            .expect("collection")
            .is_archived(second));
        assert_eq!(
            app.state.workspaces[0].tabs[0].layout.focused_leaf(),
            crate::layout::LayoutLeaf::Pane(root)
        );
    }

    #[test]
    fn collection_info_reports_advisory_limits_without_closing_members() {
        let (mut app, root, second, third) = app_with_panes();
        app.state.collection_lifecycle = crate::config::CollectionLifecycleConfig {
            archive_age_days: 1,
            archive_count: 1,
            concurrency: 1,
        };
        let collection_id = create_collection(&mut app, root);
        for pane in [second, third] {
            let public = app.public_pane_id(0, pane).expect("public pane");
            request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id: public.clone(),
                }),
            );
            request(
                &mut app,
                Method::CollectionArchive(CollectionMemberTarget {
                    collection_id: collection_id.clone(),
                    pane_id: public,
                }),
            );
            let terminal_id = app.state.workspaces[0]
                .terminal_id(pane)
                .cloned()
                .expect("terminal");
            app.state
                .terminals
                .get_mut(&terminal_id)
                .expect("state")
                .state = crate::detect::AgentState::Working;
        }
        app.state.collection_archive_times.insert(
            second,
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 24 * 60 * 60),
        );

        let info = request(
            &mut app,
            Method::CollectionGet(CollectionTarget {
                collection_id: collection_id.clone(),
            }),
        );
        assert_eq!(info["result"]["collection"]["lifecycle"]["archived"], 2);
        assert_eq!(
            info["result"]["collection"]["warnings"]
                .as_array()
                .expect("warnings")
                .len(),
            3
        );
        let resolved = app
            .resolve_collection(&collection_id)
            .expect("collection remains");
        assert_eq!(
            app.state.workspaces[resolved.0].tabs[resolved.1]
                .collection(resolved.2)
                .expect("collection")
                .members()
                .len(),
            2
        );
    }

    #[test]
    fn injected_cascade_and_promotion_failures_leave_collection_fully_intact() {
        let (mut app, root, second, third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        for pane in [second, third] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
        }
        let collection = parse_collection_id(&collection_id).expect("collection ID");
        let before_members = app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .members()
            .to_vec();
        let before_terminals = app.state.terminals.len();

        crate::workspace::fail_next_collection_mutation_for_test();
        let failed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id: collection_id.clone(),
                disposition: Some(CollectionCloseDisposition::CascadeClose),
                target_pane_id: None,
                focus_promoted: false,
            }),
        );
        assert_eq!(failed["error"]["code"], "collection_close_failed");
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection retained")
                .members(),
            before_members
        );
        assert_eq!(app.state.terminals.len(), before_terminals);
        assert!(app.state.terminal_runtime_shutdowns.is_empty());

        crate::workspace::fail_next_collection_mutation_for_test();
        let root_public = app.public_pane_id(0, root).expect("root public");
        let failed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id,
                disposition: Some(CollectionCloseDisposition::PromoteMembers),
                target_pane_id: Some(root_public),
                focus_promoted: false,
            }),
        );
        assert_eq!(failed["error"]["code"], "collection_close_failed");
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection retained")
                .members(),
            before_members
        );
    }

    #[test]
    fn cross_workspace_collection_moves_preserve_identity_archive_and_delegation() {
        let (mut app, root, second, _) = app_with_panes();
        app.state.workspaces.push(Workspace::test_new("target"));
        app.state.ensure_test_terminals();
        let target_root = app.state.workspaces[1].tabs[0]
            .root_pane
            .expect("target root");
        let target_collection = create_collection_in(&mut app, 1, target_root);
        let source_public = app.public_pane_id(0, second).expect("source public");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(second)
            .cloned()
            .expect("terminal ID");
        let delegation_id = app
            .state
            .delegations
            .create(Some(second), None, Some("helper".into()))
            .expect("delegation");
        let target_focus = app.state.workspaces[1].tabs[0].layout.focused_leaf();

        crate::workspace::fail_next_collection_mutation_for_test();
        let failed = request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: target_collection.clone(),
                pane_id: source_public.clone(),
            }),
        );
        assert_eq!(failed["error"]["code"], "pane_move_failed");
        assert_eq!(
            app.state.workspaces[0].terminal_id(second),
            Some(&terminal_id)
        );
        assert!(app.state.workspaces[1].pane_state(second).is_none());
        assert!(app.state.terminal_runtime_shutdowns.is_empty());

        let moved = request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: target_collection.clone(),
                pane_id: source_public.clone(),
            }),
        );
        assert!(moved.get("error").is_none(), "{moved}");
        let (target_ws, moved_id) = app.parse_pane_id(&source_public).expect("public alias");
        assert_eq!(target_ws, 1);
        assert_eq!(moved_id, second);
        assert_eq!(
            app.state.workspaces[1].terminal_id(second),
            Some(&terminal_id)
        );
        assert_eq!(
            app.state
                .delegations
                .delegation_for_pane(second)
                .map(|record| record.id),
            Some(delegation_id)
        );
        assert_eq!(
            app.state.workspaces[1].tabs[0].layout.focused_leaf(),
            target_focus
        );

        request(
            &mut app,
            Method::CollectionArchive(CollectionMemberTarget {
                collection_id: target_collection.clone(),
                pane_id: source_public.clone(),
            }),
        );
        let source_collection = create_collection(&mut app, root);
        let source_layout_before = app.state.workspaces[1].tabs[0].layout.leaves();
        let source_focus_before = app.state.workspaces[1].tabs[0].layout.focused_leaf();
        let public_number_before = app.state.workspaces[1].public_pane_number(second);
        let archive_time_before = app.state.collection_archive_times.get(&second).copied();
        crate::workspace::fail_next_collection_mutation_for_test();
        let failed_back = request(
            &mut app,
            Method::CollectionMove(CollectionMoveParams {
                pane_id: source_public.clone(),
                collection_id: source_collection.clone(),
            }),
        );
        assert_eq!(failed_back["id"], "test");
        assert_eq!(failed_back["error"]["code"], "pane_move_failed");
        assert_eq!(
            app.state.workspaces[1].tabs[0].layout.leaves(),
            source_layout_before
        );
        assert_eq!(
            app.state.workspaces[1].tabs[0].layout.focused_leaf(),
            source_focus_before
        );
        assert_eq!(
            app.state.workspaces[1].public_pane_number(second),
            public_number_before
        );
        assert_eq!(
            app.state.collection_archive_times.get(&second).copied(),
            archive_time_before
        );
        assert!(app.state.workspaces[1].tabs[0]
            .collection(parse_collection_id(&target_collection).expect("target collection"))
            .expect("source collection retained")
            .is_archived(second));

        let moved_back = request(
            &mut app,
            Method::CollectionMove(CollectionMoveParams {
                pane_id: source_public,
                collection_id: source_collection.clone(),
            }),
        );
        assert!(moved_back.get("error").is_none(), "{moved_back}");
        let (_, _, collection) = app
            .resolve_collection(&source_collection)
            .expect("collection");
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(second));
        assert_eq!(
            app.state.workspaces[0].terminal_id(second),
            Some(&terminal_id)
        );
    }

    #[test]
    fn empty_final_collection_uses_group_workspace_close_path() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let workspace = Workspace::test_new("root");
        let root = workspace.tabs[0].root_pane.expect("root");
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let collection_id = create_collection(&mut app, root);
        let root_id = app.public_pane_id(0, root).expect("public root");
        let added = request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: collection_id.clone(),
                pane_id: root_id,
            }),
        );
        assert!(added.get("error").is_none(), "{added}");
        let collection = parse_collection_id(&collection_id).expect("collection id");
        assert!(app.state.workspaces[0].tabs[0]
            .layout
            .remove_collection_member(collection, root));
        app.state.workspaces[0].tabs[0].panes.remove(&root);
        app.state.workspaces[0].tabs[0].root_pane = None;
        assert_eq!(app.state.workspaces[0].tabs[0].layout.leaf_count(), 1);

        let membership = |linked| crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: if linked {
                "/repo-linked".into()
            } else {
                "/repo".into()
            },
            is_linked_worktree: linked,
        };
        app.state.workspaces[0].worktree_space = Some(membership(false));
        let mut linked = Workspace::test_new("linked");
        linked.worktree_space = Some(membership(true));
        app.state.workspaces.push(linked);
        app.state.ensure_test_terminals();
        app.state.confirm_close = false;
        let sequence = app.event_hub.current_sequence();

        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id,
                disposition: None,
                target_pane_id: None,
                focus_promoted: false,
            }),
        );

        assert_eq!(closed["result"]["type"], "ok");
        assert!(app.state.workspaces.is_empty());
        let events = app.event_hub.events_after(sequence);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| event.event == EventKind::WorkspaceClosed)
                .count(),
            2
        );
        assert!(events
            .iter()
            .any(|(_, event)| event.event == EventKind::CollectionClosed));
    }

    #[test]
    fn cascading_collection_only_tab_does_not_update_unaffected_tab_layout() {
        let (mut app, root, second, third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        for pane in [root, second, third] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            let added = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
            assert!(added.get("error").is_none(), "{added}");
        }
        let source_tab_id = app.public_tab_id(0, 0).expect("source tab");
        let unaffected_tab_idx = app.state.workspaces[0].test_add_tab(Some("unaffected"));
        let unaffected_tab_id = app
            .public_tab_id(0, unaffected_tab_idx)
            .expect("unaffected tab");
        app.state.ensure_test_terminals();
        let sequence = app.event_hub.current_sequence();

        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id,
                disposition: Some(CollectionCloseDisposition::CascadeClose),
                target_pane_id: None,
                focus_promoted: false,
            }),
        );

        assert_eq!(closed["result"]["type"], "ok");
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert_eq!(
            app.public_tab_id(0, 0).as_deref(),
            Some(unaffected_tab_id.as_str())
        );
        let events = app.event_hub.events_after(sequence);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::TabClosed { tab_id, .. } if tab_id == &source_tab_id
            )
        }));
        assert!(!events
            .iter()
            .any(|(_, event)| event.event == EventKind::LayoutUpdated));
    }

    #[test]
    fn final_collection_close_uses_group_workspace_close_and_emits_all_events() {
        let (mut app, root, second, third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        for pane in [root, second, third] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            let added = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
            assert!(added.get("error").is_none(), "{added}");
        }
        app.state.workspaces.push(Workspace::test_new("linked"));
        let membership = |linked| crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "repo".into(),
            repo_root: "/repo".into(),
            checkout_path: if linked {
                "/repo-linked".into()
            } else {
                "/repo".into()
            },
            is_linked_worktree: linked,
        };
        app.state.workspaces[0].worktree_space = Some(membership(false));
        app.state.workspaces[1].worktree_space = Some(membership(true));
        app.state.ensure_test_terminals();
        app.state.confirm_close = false;
        let sequence = app.event_hub.current_sequence();

        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id,
                disposition: Some(CollectionCloseDisposition::CascadeClose),
                target_pane_id: None,
                focus_promoted: false,
            }),
        );

        assert_eq!(closed["result"]["type"], "ok");
        assert!(app.state.workspaces.is_empty());
        let events = app.event_hub.events_after(sequence);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| event.event == EventKind::WorkspaceClosed)
                .count(),
            2
        );
        assert!(events
            .iter()
            .any(|(_, event)| event.event == EventKind::CollectionClosed));
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| event.event == EventKind::PaneClosed)
                .count(),
            4
        );
    }

    #[test]
    fn collection_only_promotion_creates_normal_tab_in_canonical_member_order() {
        let (mut app, root, second, third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        for pane in [second, third, root] {
            let pane_id = app.public_pane_id(0, pane).expect("public pane");
            let response = request(
                &mut app,
                Method::CollectionAdd(CollectionAddParams {
                    collection_id: collection_id.clone(),
                    pane_id,
                }),
            );
            assert!(response.get("error").is_none(), "{response}");
        }
        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id: collection_id.clone(),
                disposition: Some(CollectionCloseDisposition::PromoteMembers),
                target_pane_id: None,
                focus_promoted: false,
            }),
        );
        assert_eq!(closed["result"]["type"], "ok");
        assert!(app.resolve_collection(&collection_id).is_none());
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        let tab = &app.state.workspaces[0].tabs[0];
        assert_eq!(tab.layout.tiled_pane_ids(), vec![second, third, root]);
        for pane in [second, third, root] {
            assert_eq!(tab.pane_placement(pane), Some(PanePlacement::Tiled));
        }
    }

    #[test]
    fn empty_collection_focus_has_no_compatibility_pane_but_keeps_typed_focus() {
        let (mut app, root, _, _) = app_with_panes();
        let root_public = app.public_pane_id(0, root).expect("root public");
        let response = request(
            &mut app,
            Method::CollectionCreate(CollectionCreateParams {
                target_pane_id: root_public,
                direction: crate::api::schema::SplitDirection::Right,
                ratio: Some(0.5),
                label: None,
                focus: true,
            }),
        );
        let collection_id = response["result"]["collection"]["collection_id"]
            .as_str()
            .expect("collection ID")
            .to_string();
        let snapshot = app.pane_layout_snapshot(0, 0).expect("layout snapshot");
        assert_eq!(snapshot.focused_pane_id, None);
        assert_eq!(
            snapshot.focused,
            LayoutFocusInfo::Collection {
                collection_id,
                selected_pane_id: None,
            }
        );
    }

    #[test]
    fn collection_list_get_layout_and_cascade_close_are_typed() {
        let (mut app, root, second, _third) = app_with_panes();
        let collection_id = create_collection(&mut app, root);
        let second_public = app.public_pane_id(0, second).expect("public");
        request(
            &mut app,
            Method::CollectionAdd(CollectionAddParams {
                collection_id: collection_id.clone(),
                pane_id: second_public,
            }),
        );

        let list = request(
            &mut app,
            Method::CollectionList(CollectionListParams::default()),
        );
        assert_eq!(
            list["result"]["collections"]
                .as_array()
                .expect("list")
                .len(),
            1
        );
        let layout = app.pane_layout_snapshot(0, 0).expect("layout");
        assert_eq!(layout.collections.len(), 1);
        assert!(matches!(layout.focused, LayoutFocusInfo::Pane { .. }));
        assert!(matches!(
            app.pane_info(0, second).expect("pane").placement,
            PanePlacementInfo::Collection { .. }
        ));

        let closed = request(
            &mut app,
            Method::CollectionClose(CollectionCloseParams {
                collection_id: collection_id.clone(),
                disposition: Some(CollectionCloseDisposition::CascadeClose),
                target_pane_id: None,
                focus_promoted: false,
            }),
        );
        assert_eq!(closed["result"]["type"], "ok");
        assert!(app.state.workspaces[0].pane_state(second).is_none());
        assert!(app.resolve_collection(&collection_id).is_none());
    }
}
