use std::path::PathBuf;

use crate::api::schema::{
    EventData, EventEnvelope, EventKind, ResponseResult, WorkspaceCreateParams,
    WorkspaceMoveBlockParams, WorkspaceMoveParams, WorkspaceRenameParams,
    WorkspaceReportMetadataParams, WorkspaceReportResourcesParams, WorkspaceTarget,
};
use crate::app::App;

use super::super::api_helpers::{normalize_metadata_source, normalize_metadata_ttl};
use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_workspace_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::WorkspaceList {
                workspaces: self.workspace_list_info(),
            },
        )
    }

    pub(super) fn handle_workspace_get(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        let Some(_) = self.state.workspaces.get(index) else {
            return workspace_not_found(id, &target.workspace_id);
        };

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_create(
        &mut self,
        id: String,
        params: WorkspaceCreateParams,
    ) -> String {
        let cwd = params.cwd.map(PathBuf::from).unwrap_or_else(|| {
            let follow_cwd = self.workspace_creation_source().and_then(|ws_idx| {
                self.focused_pane_cwd_in_workspace(ws_idx)
                    .or_else(|| self.seed_cwd_from_workspace(ws_idx))
            });
            self.resolve_new_terminal_cwd(follow_cwd)
        });
        let extra_env = match super::env::normalize_launch_env(params.env) {
            Ok(env) => env,
            Err((code, message)) => return encode_error(id, &code, message),
        };
        if let Some(target_space) = crate::workspace::git_space_metadata(&cwd) {
            let checkout_root = crate::worktree::canonical_or_original(&target_space.repo_root);
            if let Some(index) = self.state.workspaces.iter().position(|workspace| {
                // A live/cached canonical Git identity wins over persisted checkout
                // provenance at the same path. Only use persisted identity if Git
                // discovery for that open workspace is genuinely unavailable.
                let discovered = workspace
                    .checkout
                    .as_ref()
                    .and_then(|_| crate::workspace::git_space_metadata(&workspace.identity_cwd))
                    .or_else(|| workspace.git_space().cloned());
                discovered.map_or_else(
                    || {
                        workspace.checkout.as_ref().is_some_and(|checkout| {
                            crate::worktree::canonical_or_original(&checkout.checkout_path)
                                == checkout_root
                        })
                    },
                    |space| {
                        space.key == target_space.key
                            && crate::worktree::canonical_or_original(&space.repo_root)
                                == checkout_root
                    },
                )
            }) {
                if params.focus {
                    self.state.switch_workspace(index);
                }
                if let Some(label) = params.label {
                    self.state.workspaces[index].set_custom_name(label);
                    self.state.mark_session_dirty();
                }
                return encode_success(
                    id,
                    self.workspace_created_result(index)
                        .expect("existing checkout should produce a complete open response"),
                );
            }
        }
        match self.create_workspace_with_launch_env(cwd, params.focus, extra_env) {
            Ok(index) => {
                if let Some(label) = params.label {
                    if let Some(workspace) = self.state.workspaces.get_mut(index) {
                        workspace.set_custom_name(label);
                        crate::logging::workspace_renamed(&workspace.id);
                    }
                }
                self.emit_workspace_open_events(index);
                encode_success(
                    id,
                    self.workspace_created_result(index)
                        .expect("new workspace should produce a complete create response"),
                )
            }
            Err(err) => encode_error(id, "workspace_create_failed", err.to_string()),
        }
    }

    pub(super) fn handle_workspace_focus(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        self.state.switch_workspace(index);

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_rename(
        &mut self,
        id: String,
        params: WorkspaceRenameParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let Some(ws) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        ws.set_custom_name(params.label.clone());
        crate::logging::workspace_renamed(&ws.id);
        self.schedule_session_save();
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceRenamed,
            data: EventData::WorkspaceRenamed {
                workspace_id: self.public_workspace_id(index),
                label: params.label,
            },
        });

        encode_success(
            id,
            ResponseResult::WorkspaceInfo {
                workspace: self.workspace_info(index),
            },
        )
    }

    pub(super) fn handle_workspace_move(
        &mut self,
        id: String,
        params: WorkspaceMoveParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &params.workspace_id);
        }
        if params.insert_index > self.state.workspaces.len() {
            return encode_error(
                id,
                "workspace_move_failed",
                format!("insert_index {} is out of bounds", params.insert_index),
            );
        }

        let workspace_id = self.public_workspace_id(index);
        let insert_index = params.insert_index;
        let moved = self.state.move_workspace(index, insert_index);
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceMoved,
                data: EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index,
                    workspaces: workspaces.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_move_block(
        &mut self,
        id: String,
        params: WorkspaceMoveBlockParams,
    ) -> String {
        if params.workspace_ids.is_empty() {
            return encode_error(
                id,
                "workspace_move_block_failed",
                "workspace_ids must not be empty",
            );
        }

        let mut workspace_ids = Vec::with_capacity(params.workspace_ids.len());
        let mut seen_ids = std::collections::HashSet::new();
        for requested_id in &params.workspace_ids {
            let Some(index) = self.parse_workspace_id(requested_id) else {
                return workspace_not_found(id, requested_id);
            };
            let Some(workspace) = self.state.workspaces.get(index) else {
                return workspace_not_found(id, requested_id);
            };
            if !seen_ids.insert(workspace.id.clone()) {
                return encode_error(
                    id,
                    "workspace_move_block_failed",
                    format!("workspace {requested_id} appears more than once"),
                );
            }
            workspace_ids.push(workspace.id.clone());
        }

        let before_workspace_id = match params.before_workspace_id {
            Some(requested_id) => {
                let Some(index) = self.parse_workspace_id(&requested_id) else {
                    return workspace_not_found(id, &requested_id);
                };
                let Some(workspace) = self.state.workspaces.get(index) else {
                    return workspace_not_found(id, &requested_id);
                };
                if seen_ids.contains(&workspace.id) {
                    return encode_error(
                        id,
                        "workspace_move_block_failed",
                        "before_workspace_id must not be part of workspace_ids",
                    );
                }
                Some(workspace.id.clone())
            }
            None => None,
        };

        let moved = self
            .state
            .move_workspace_block(&workspace_ids, before_workspace_id.as_deref());
        let workspaces = self.workspace_list_info();
        if moved {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceReordered,
                data: EventData::WorkspaceReordered {
                    workspace_ids,
                    before_workspace_id,
                    workspaces: workspaces.clone(),
                },
            });
        }

        encode_success(id, ResponseResult::WorkspaceList { workspaces })
    }

    pub(super) fn handle_workspace_report_metadata(
        &mut self,
        id: String,
        params: WorkspaceReportMetadataParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let source = match normalize_metadata_source(params.source) {
            Ok(source) => source,
            Err(message) => return encode_error(id, "invalid_metadata_source", message),
        };
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return encode_error(id, "invalid_metadata_ttl", message),
        };
        let tokens = match super::super::api_helpers::normalize_metadata_tokens(params.tokens) {
            Ok(tokens) => tokens,
            Err(message) => return encode_error(id, "invalid_metadata_token", message),
        };
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        if !crate::metadata_tokens::sequence_is_fresh(
            &workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            return encode_success(id, ResponseResult::Ok {});
        }
        if workspace.metadata_tokens.key_count_after_patch(&tokens)
            > super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
        {
            return encode_error(
                id,
                "metadata_token_limit",
                format!(
                    "workspace metadata may contain at most {} tokens",
                    super::super::api_helpers::MAX_METADATA_TOKEN_KEYS_PER_RESOURCE
                ),
            );
        }
        match crate::metadata_tokens::accept_sequence(
            &mut workspace.metadata_token_sequences,
            &source,
            params.seq,
        ) {
            Ok(true) => {}
            Ok(false) => return encode_success(id, ResponseResult::Ok {}),
            Err(()) => {
                return encode_error(
                    id,
                    "metadata_sequence_source_limit",
                    format!(
                        "workspace metadata may track at most {} sequenced sources",
                        crate::metadata_tokens::MAX_SEQUENCE_SOURCES
                    ),
                );
            }
        }
        let changed = workspace
            .metadata_tokens
            .patch(tokens, ttl, std::time::Instant::now());
        if changed {
            self.sync_agent_metadata_deadline();
            self.emit_workspace_token_updated(index);
        }
        encode_success(id, ResponseResult::Ok {})
    }

    pub(super) fn handle_workspace_report_resources(
        &mut self,
        id: String,
        params: WorkspaceReportResourcesParams,
    ) -> String {
        let Some(index) = self.parse_workspace_id(&params.workspace_id) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        let Some(plugin_id) = super::plugins::normalize_plugin_id(&params.plugin_id) else {
            return encode_error(id, "invalid_plugin_id", "invalid plugin id");
        };
        if !self.plugin_can_own_workspace_resources(&plugin_id) {
            return encode_error(
                id,
                "plugin_resource_owner_unavailable",
                "plugin is not an enabled compatible resource owner",
            );
        }
        let ttl = match normalize_metadata_ttl(params.ttl_ms) {
            Ok(ttl) => ttl,
            Err(message) => return encode_error(id, "invalid_resource_ttl", message),
        };
        let resources = match params
            .resources
            .into_iter()
            .map(|resource| {
                crate::workspace_resources::normalize_resource(
                    &plugin_id,
                    resource.resource_id,
                    resource.label,
                    resource.detail,
                    resource.data,
                )
            })
            .collect::<Result<Vec<_>, _>>()
        {
            Ok(resources) => resources,
            Err(message) => return encode_error(id, "invalid_workspace_resource", message),
        };
        let Some(workspace) = self.state.workspaces.get_mut(index) else {
            return workspace_not_found(id, &params.workspace_id);
        };
        match workspace.resources.report(
            plugin_id,
            resources,
            params.seq,
            ttl,
            std::time::Instant::now(),
        ) {
            Ok(changed) => {
                if changed {
                    self.state.reconcile_selected_workspace_resource();
                    self.sync_agent_metadata_deadline();
                    self.emit_workspace_resources_updated(index);
                }
                encode_success(id, ResponseResult::Ok {})
            }
            Err(message) => encode_error(id, "workspace_resource_limit", message),
        }
    }

    pub(super) fn handle_workspace_close(&mut self, id: String, target: WorkspaceTarget) -> String {
        let Some(index) = self.parse_workspace_id(&target.workspace_id) else {
            return workspace_not_found(id, &target.workspace_id);
        };
        if self.state.workspaces.get(index).is_none() {
            return workspace_not_found(id, &target.workspace_id);
        }
        self.close_workspace_with_lifecycle(index, true);
        encode_success(id, ResponseResult::Ok {})
    }

    /// Close exactly one workspace and emit its complete lifecycle snapshot.
    /// Capture happens before the centralized state close so implicit pane/collection/tab removal
    /// has the same observable events as explicit close operations.
    pub(super) fn close_workspace_with_lifecycle(
        &mut self,
        index: usize,
        emit_repository_closed: bool,
    ) {
        let workspace_id = self.public_workspace_id(index);
        let workspace_snapshot = self.workspace_info(index);
        let closed_repository_id = workspace_snapshot.repository_id.clone();
        let mut public_panes = Vec::new();
        let mut closed_collections = Vec::new();
        let mut closed_tabs = Vec::new();
        for (tab_idx, tab) in self.state.workspaces[index].tabs.iter().enumerate() {
            let tab_id = self.public_tab_id(index, tab_idx).unwrap_or_default();
            closed_tabs.push((workspace_id.clone(), tab_id.clone()));
            for pane_id in tab.layout.pane_ids() {
                if let Some(public) = self.public_pane_id(index, pane_id) {
                    public_panes.push((pane_id, public, workspace_id.clone()));
                }
            }
            for collection_id in tab.layout.collection_ids() {
                if let Some(collection) = tab.collection(collection_id) {
                    closed_collections.push((
                        workspace_id.clone(),
                        tab_id.clone(),
                        collection_id,
                        collection.members().to_vec(),
                    ));
                }
            }
        }

        self.state.selected = index;
        let destruction = self.state.close_selected_workspace();
        self.shutdown_detached_terminal_runtimes();

        for (workspace_id, tab_id, collection_id, members) in closed_collections {
            for pane_id in members {
                if let Some((_, public, _)) = public_panes
                    .iter()
                    .find(|(id, _, owner)| *id == pane_id && owner == &workspace_id)
                {
                    self.emit_event(EventEnvelope {
                        event: EventKind::CollectionMemberRemoved,
                        data: EventData::CollectionMemberRemoved {
                            collection_id: super::collections::collection_id_string(collection_id),
                            pane_id: public.clone(),
                        },
                    });
                }
            }
            self.emit_event(EventEnvelope {
                event: EventKind::CollectionClosed,
                data: EventData::CollectionClosed {
                    collection_id: super::collections::collection_id_string(collection_id),
                    workspace_id,
                    tab_id,
                },
            });
        }
        for (_, pane_id, workspace_id) in &public_panes {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                },
            });
        }
        let public_pairs = public_panes
            .iter()
            .map(|(pane, public, _)| (*pane, public.clone()))
            .collect::<Vec<_>>();
        self.emit_pane_destruction_events(destruction, &public_pairs);
        for (workspace_id, tab_id) in closed_tabs {
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id,
                },
            });
        }
        self.emit_event(EventEnvelope {
            event: EventKind::WorkspaceClosed,
            data: EventData::WorkspaceClosed {
                workspace_id,
                workspace: Some(workspace_snapshot),
            },
        });
        if emit_repository_closed
            && closed_repository_id
                .as_ref()
                .is_some_and(|repository_id| self.state.repository(repository_id).is_none())
        {
            self.emit_event(EventEnvelope {
                event: EventKind::RepositoryClosed,
                data: EventData::RepositoryClosed {
                    repository_id: closed_repository_id.expect("repository was checked above"),
                },
            });
        }
    }

    /// Close a worktree group for the tab/pane implicit-close paths. Workspace/Checkout close
    /// deliberately does not use this path.
    pub(super) fn close_workspace_group_with_lifecycle(&mut self, index: usize) {
        let has_nested_lifecycle = self.state.workspaces[index].tabs.iter().any(|tab| {
            !tab.layout.collection_ids().is_empty()
                || tab.layout.pane_ids().into_iter().any(|pane_id| {
                    self.state
                        .delegations
                        .delegation_for_pane(pane_id)
                        .is_some()
                })
        });
        let close_indices = has_nested_lifecycle
            .then(|| self.state.workspaces[index].worktree_space())
            .flatten()
            .filter(|space| !space.is_linked_worktree)
            .map(|space| {
                self.state
                    .workspaces
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, workspace)| {
                        workspace
                            .worktree_space()
                            .is_some_and(|member| member.key == space.key)
                            .then_some(idx)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|indices| indices.len() >= 2)
            .unwrap_or_else(|| vec![index]);
        let workspace_snapshots = close_indices
            .iter()
            .map(|idx| (self.public_workspace_id(*idx), self.workspace_info(*idx)))
            .collect::<Vec<_>>();
        let closed_repository_ids = workspace_snapshots
            .iter()
            .filter_map(|(_, workspace)| workspace.repository_id.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut public_panes = Vec::new();
        let mut closed_collections = Vec::new();
        let mut closed_tabs = Vec::new();
        for ws_idx in &close_indices {
            let workspace_id = self.public_workspace_id(*ws_idx);
            for (tab_idx, tab) in self.state.workspaces[*ws_idx].tabs.iter().enumerate() {
                let tab_id = self.public_tab_id(*ws_idx, tab_idx).unwrap_or_default();
                closed_tabs.push((workspace_id.clone(), tab_id.clone()));
                for pane_id in tab.layout.pane_ids() {
                    if let Some(public) = self.public_pane_id(*ws_idx, pane_id) {
                        public_panes.push((pane_id, public, workspace_id.clone()));
                    }
                }
                for collection_id in tab.layout.collection_ids() {
                    if let Some(collection) = tab.collection(collection_id) {
                        closed_collections.push((
                            workspace_id.clone(),
                            tab_id.clone(),
                            collection_id,
                            collection.members().to_vec(),
                        ));
                    }
                }
            }
        }
        let mut destruction = crate::app::actions::PaneDestructionSummary::default();
        for close_index in close_indices.into_iter().rev() {
            self.state.selected = close_index;
            let summary = self.state.close_selected_workspace();
            destruction
                .tombstoned_delegations
                .extend(summary.tombstoned_delegations);
            destruction
                .garbage_collected_delegations
                .extend(summary.garbage_collected_delegations);
        }
        self.shutdown_detached_terminal_runtimes();
        for (workspace_id, tab_id, collection_id, members) in closed_collections {
            for pane_id in members {
                if let Some((_, public, _)) = public_panes
                    .iter()
                    .find(|(id, _, owner)| *id == pane_id && owner == &workspace_id)
                {
                    self.emit_event(EventEnvelope {
                        event: EventKind::CollectionMemberRemoved,
                        data: EventData::CollectionMemberRemoved {
                            collection_id: super::collections::collection_id_string(collection_id),
                            pane_id: public.clone(),
                        },
                    });
                }
            }
            self.emit_event(EventEnvelope {
                event: EventKind::CollectionClosed,
                data: EventData::CollectionClosed {
                    collection_id: super::collections::collection_id_string(collection_id),
                    workspace_id,
                    tab_id,
                },
            });
        }
        for (_, pane_id, workspace_id) in &public_panes {
            self.emit_event(EventEnvelope {
                event: EventKind::PaneClosed,
                data: EventData::PaneClosed {
                    pane_id: pane_id.clone(),
                    workspace_id: workspace_id.clone(),
                },
            });
        }
        let public_pairs = public_panes
            .iter()
            .map(|(pane, public, _)| (*pane, public.clone()))
            .collect::<Vec<_>>();
        self.emit_pane_destruction_events(destruction, &public_pairs);
        for (workspace_id, tab_id) in closed_tabs {
            self.emit_event(EventEnvelope {
                event: EventKind::TabClosed,
                data: EventData::TabClosed {
                    tab_id,
                    workspace_id,
                },
            });
        }
        for (workspace_id, workspace) in workspace_snapshots {
            self.emit_event(EventEnvelope {
                event: EventKind::WorkspaceClosed,
                data: EventData::WorkspaceClosed {
                    workspace_id,
                    workspace: Some(workspace),
                },
            });
        }
        for repository_id in closed_repository_ids {
            if self.state.repository(&repository_id).is_none() {
                self.emit_event(EventEnvelope {
                    event: EventKind::RepositoryClosed,
                    data: EventData::RepositoryClosed { repository_id },
                });
            }
        }
    }

    fn workspace_list_info(&self) -> Vec<crate::api::schema::WorkspaceInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .map(|(idx, _)| self.workspace_info(idx))
            .collect()
    }
}

fn workspace_not_found(id: String, workspace_id: &str) -> String {
    encode_error(
        id,
        "workspace_not_found",
        format!("workspace {workspace_id} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::schema::SuccessResponse, config::Config, workspace::Workspace};

    // `new_cwd = follow` must anchor on the focused pane for every creation
    // surface. Splits and tabs already do; a new workspace must follow the
    // focused pane too, not the source workspace's first-tab root pane.
    #[tokio::test]
    async fn workspace_create_follows_focused_pane_cwd_not_first_tab_root() {
        use super::super::test_support::{exiting_test_command, shutdown_test_runtimes};
        use crate::config::ShellModeConfig;

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.default_shell = exiting_test_command().into();
        app.state.shell_mode = ShellModeConfig::NonLogin;
        app.state.workspaces = vec![Workspace::test_new("spaces")];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();

        // Second tab becomes the focused pane, away from tab 1's root pane.
        let response = app.handle_tab_create(
            "tab".into(),
            crate::api::schema::TabCreateParams {
                workspace_id: None,
                cwd: None,
                focus: true,
                label: None,
                env: Default::default(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        // Drop runtimes so cwd resolution deterministically uses cached state.
        shutdown_test_runtimes(&mut app);

        let focused_cwd = std::env::temp_dir().join(format!(
            "herdr-ws-follow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&focused_cwd).unwrap();
        let ws = &app.state.workspaces[0];
        let root_cwd = ws.identity_cwd.clone();
        let focused_pane = ws.focused_pane_id().unwrap();
        assert_ne!(
            focused_pane,
            ws.tabs[0].root_pane.expect("test tab has root pane")
        );
        let terminal_id = ws.terminal_id(focused_pane).cloned().unwrap();
        app.state.terminals.get_mut(&terminal_id).unwrap().cwd = focused_cwd.clone();

        let response = app.handle_workspace_create(
            "req".into(),
            WorkspaceCreateParams {
                cwd: None,
                focus: false,
                label: None,
                env: Default::default(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::WorkspaceCreated { .. }
        ));
        let created_cwd = &app.state.workspaces[1].identity_cwd;
        assert_eq!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&focused_cwd)
        );
        assert_ne!(
            crate::worktree::canonical_or_original(created_cwd),
            crate::worktree::canonical_or_original(&root_cwd)
        );
        shutdown_test_runtimes(&mut app);
        let _ = std::fs::remove_dir_all(&focused_cwd);
    }

    fn app_with_linked_worktree() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("issue")];
        app.state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app
    }

    #[test]
    fn opening_nested_path_reuses_existing_checkout() {
        let root =
            std::env::temp_dir().join(format!("herdr-checkout-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("nested")).unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q", root.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success());

        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("root");
        workspace.identity_cwd = root.clone();
        workspace.cached_git_space = crate::workspace::git_space_metadata(&root);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app.state.reconcile_repositories();
        let existing_id = app.state.workspaces[0].id.clone();

        let response = app.handle_workspace_create(
            "req".into(),
            WorkspaceCreateParams {
                cwd: Some(root.join("nested").display().to_string()),
                focus: true,
                label: None,
                env: Default::default(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].id, existing_id);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn api_workspace_close_closes_linked_worktree_workspace_only() {
        let mut app = app_with_linked_worktree();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: app.state.workspaces[0].id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        assert_eq!(app.state.request_remove_linked_worktree, None);
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn api_workspace_close_event_includes_final_worktree_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = app_with_linked_worktree().state.workspaces;
        let workspace_id = app.state.workspaces[0].id.clone();

        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: workspace_id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(success.id, "req");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceClosed {
                    workspace_id: closed_id,
                    workspace: Some(workspace),
                } if closed_id == &workspace_id
                    && workspace
                        .worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree.is_linked_worktree)
            )
        }));
    }

    #[test]
    fn workspace_metadata_tokens_patch_clear_and_emit_snapshot() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);

        for (tokens, expected) in [
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("reviewing auth".into())),
                    ("jj_status".into(), Some("2 changes".into())),
                ]),
                std::collections::HashMap::from([
                    ("summary".into(), "reviewing auth".into()),
                    ("jj_status".into(), "2 changes".into()),
                ]),
            ),
            (
                std::collections::HashMap::from([
                    ("summary".into(), Some("done".into())),
                    ("jj_status".into(), None),
                ]),
                std::collections::HashMap::from([("summary".into(), "done".into())]),
            ),
        ] {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "req".into(),
                method: crate::api::schema::Method::WorkspaceReportMetadata(
                    WorkspaceReportMetadataParams {
                        workspace_id: workspace_id.clone(),
                        source: "user:test".into(),
                        tokens,
                        seq: None,
                        ttl_ms: None,
                    },
                ),
            });
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(success.result, ResponseResult::Ok {});
            assert_eq!(app.workspace_info(0).tokens, expected);
        }

        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace }
                if workspace.tokens.get("summary").map(String::as_str) == Some("done")
                    && !workspace.tokens.contains_key("jj_status")
        )));
    }

    #[test]
    fn workspace_token_ttl_expires_through_runtime_and_emits_update() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        let workspace_id = app.public_workspace_id(0);
        let response = app.handle_workspace_report_metadata(
            "req".into(),
            WorkspaceReportMetadataParams {
                workspace_id,
                source: "user:test".into(),
                tokens: std::collections::HashMap::from([(
                    "summary".into(),
                    Some("temporary".into()),
                )]),
                seq: None,
                ttl_ms: Some(1),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let deadline = app.agent_metadata_deadline.expect("token deadline");

        app.expire_metadata_at(deadline, deadline);

        assert!(app.workspace_info(0).tokens.is_empty());
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data,
            EventData::WorkspaceMetadataUpdated { workspace } if workspace.tokens.is_empty()
        )));
    }

    #[test]
    fn resource_report_replaces_without_dirtying_session_and_emits_event() {
        let event_hub = crate::api::EventHub::default();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one")];
        app.state.installed_plugins.insert(
            "hs.jail".into(),
            crate::api::schema::InstalledPluginInfo {
                plugin_id: "hs.jail".into(),
                name: "jail".into(),
                version: "1".into(),
                min_herdr_version: String::new(),
                description: None,
                manifest_path: String::new(),
                plugin_root: String::new(),
                enabled: true,
                platforms: None,
                build: vec![],
                startup: vec![],
                actions: vec![],
                events: vec![],
                panes: vec![],
                link_handlers: vec![],
                source: crate::api::schema::PluginSourceInfo::default(),
                warnings: vec![],
            },
        );
        let workspace_id = app.public_workspace_id(0);
        let response = app.handle_workspace_report_resources(
            "resource".into(),
            WorkspaceReportResourcesParams {
                workspace_id: workspace_id.clone(),
                plugin_id: "hs.jail".into(),
                seq: Some(1),
                ttl_ms: None,
                resources: vec![crate::api::schema::WorkspaceResourceInput {
                    resource_id: "immutable".into(),
                    label: "🔒 jail".into(),
                    detail: None,
                    data: None,
                }],
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(!app.state.session_dirty);
        assert_eq!(app.workspace_info(0).resources.len(), 1);
        assert!(event_hub.events_after(0).iter().any(|(_, event)| matches!(
            &event.data, EventData::WorkspaceResourcesUpdated { workspace }
                if workspace.workspace_id == workspace_id && workspace.resources.len() == 1
        )));
        let response = app.handle_workspace_report_resources(
            "replace".into(),
            WorkspaceReportResourcesParams {
                workspace_id,
                plugin_id: "hs.jail".into(),
                seq: Some(2),
                ttl_ms: None,
                resources: Vec::new(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(app.workspace_info(0).resources.is_empty());
    }

    #[test]
    fn api_workspace_move_reorders_workspaces() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![
            Workspace::test_new("one"),
            Workspace::test_new("two"),
            Workspace::test_new("three"),
        ];
        app.state.active = Some(0);
        app.state.selected = 0;
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 3,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[2].workspace_id, moved_id);
        assert_eq!(app.state.workspaces[2].display_name(), "one");
        let events = event_hub.events_after(0);
        assert!(events.iter().any(|(_, event)| {
            matches!(
                &event.data,
                EventData::WorkspaceMoved {
                    workspace_id,
                    insert_index: 3,
                    workspaces,
                } if workspace_id == &moved_id
                    && workspaces[2].workspace_id == moved_id
            )
        }));
    }

    #[test]
    fn api_workspace_move_block_reorders_atomically() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![
            Workspace::test_new("child"),
            Workspace::test_new("normal"),
            Workspace::test_new("parent"),
            Workspace::test_new("tail"),
        ];
        let parent_id = app.public_workspace_id(2);
        let child_id = app.public_workspace_id(0);
        let tail_id = app.public_workspace_id(3);

        let response = app.handle_workspace_move_block(
            "req".into(),
            WorkspaceMoveBlockParams {
                workspace_ids: vec![parent_id.clone(), child_id.clone()],
                before_workspace_id: Some(tail_id.clone()),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|workspace| workspace.display_name())
                .collect::<Vec<_>>(),
            ["normal", "parent", "child", "tail"]
        );
        assert_eq!(workspaces[1].workspace_id, parent_id);
        assert_eq!(workspaces[2].workspace_id, child_id);
        let events = event_hub.events_after(0);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].1.data,
            EventData::WorkspaceReordered {
                workspace_ids,
                before_workspace_id,
                workspaces,
            } if workspace_ids.first() == Some(&parent_id)
                && workspace_ids.get(1) == Some(&child_id)
                && workspace_ids.len() == 2
                && before_workspace_id.as_deref() == Some(tail_id.as_str())
                && workspaces[1].workspace_id == parent_id
        ));
    }

    #[test]
    fn api_workspace_move_noop_does_not_emit_event() {
        let event_hub = crate::api::EventHub::default();
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, api_rx, event_hub.clone());
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        let moved_id = app.public_workspace_id(0);

        let response = app.handle_workspace_move(
            "req".into(),
            WorkspaceMoveParams {
                workspace_id: moved_id.clone(),
                insert_index: 1,
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::WorkspaceList { workspaces } = success.result else {
            panic!("expected workspace list");
        };
        assert_eq!(workspaces[0].workspace_id, moved_id);
        assert!(event_hub.events_after(0).is_empty());
    }
}
