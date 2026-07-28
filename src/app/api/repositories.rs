use crate::api::schema::{
    CheckoutMoveParams, EventData, EventEnvelope, EventKind, RepositoryInfo, RepositoryMoveParams,
    RepositoryRenameParams, RepositoryTarget, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(crate) fn repository_info(&self, repository_id: &str) -> Option<RepositoryInfo> {
        let repository = self.state.repository(repository_id)?;
        let members = repository
            .checkout_workspace_ids
            .iter()
            .filter_map(|id| {
                self.state
                    .workspaces
                    .iter()
                    .find(|workspace| &workspace.id == id)
            })
            .collect::<Vec<_>>();
        let (state, seen) = members
            .iter()
            .map(|workspace| workspace.aggregate_state(&self.state.terminals))
            .max_by_key(|(state, seen)| match (state, seen) {
                (crate::detect::AgentState::Blocked, _) => 4,
                (crate::detect::AgentState::Idle, false) => 3,
                (crate::detect::AgentState::Working, _) => 2,
                (crate::detect::AgentState::Idle, true) => 1,
                (crate::detect::AgentState::Unknown, _) => 0,
            })
            .unwrap_or((crate::detect::AgentState::Unknown, true));
        let pane_count = members
            .iter()
            .map(|workspace| workspace.public_pane_numbers.len())
            .sum();
        let active_agent_count = members
            .iter()
            .flat_map(|workspace| workspace.tabs.iter())
            .flat_map(|tab| tab.panes.values())
            .filter(|pane| {
                self.state
                    .terminals
                    .get(&pane.attached_terminal_id)
                    .is_some_and(|terminal| {
                        terminal.effective_agent_label().is_some()
                            && terminal.state != crate::detect::AgentState::Unknown
                    })
            })
            .count();
        Some(RepositoryInfo {
            repository_id: repository.id.clone(),
            label: repository.display_label().to_string(),
            git_common_dir: repository.git_common_dir.display().to_string(),
            checkout_workspace_ids: repository.checkout_workspace_ids.clone(),
            last_focused_workspace_id: repository.last_focused_workspace_id.clone(),
            preferred_base: repository.preferred_base.clone(),
            focused: self
                .state
                .active
                .and_then(|idx| self.state.workspaces.get(idx))
                .and_then(|workspace| workspace.checkout.as_ref())
                .is_some_and(|checkout| checkout.repository_id == repository.id),
            pane_count,
            active_agent_count,
            agent_status: super::super::api_helpers::pane_agent_status(state, seen),
        })
    }

    fn repository_list_info(&self) -> Vec<RepositoryInfo> {
        self.state
            .space_order
            .iter()
            .filter_map(|space| match space {
                crate::repository::SpaceRef::Repository(id) => self.repository_info(id),
                crate::repository::SpaceRef::StandaloneWorkspace(_) => None,
            })
            .collect()
    }

    pub(super) fn handle_repository_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::RepositoryList {
                repositories: self.repository_list_info(),
            },
        )
    }

    pub(super) fn handle_repository_get(&mut self, id: String, target: RepositoryTarget) -> String {
        match self.repository_info(&target.repository_id) {
            Some(repository) => encode_success(id, ResponseResult::RepositoryInfo { repository }),
            None => encode_error(
                id,
                "repository_not_found",
                format!("repository {} not found", target.repository_id),
            ),
        }
    }

    pub(super) fn handle_repository_focus(
        &mut self,
        id: String,
        target: RepositoryTarget,
    ) -> String {
        if !self.state.focus_repository(&target.repository_id) {
            return encode_error(
                id,
                "repository_not_found",
                format!("repository {} not found", target.repository_id),
            );
        }
        self.emit_event(EventEnvelope {
            event: EventKind::RepositoryFocused,
            data: EventData::RepositoryFocused {
                repository_id: target.repository_id.clone(),
            },
        });
        self.handle_repository_get(id, target)
    }

    pub(super) fn handle_repository_rename(
        &mut self,
        id: String,
        params: RepositoryRenameParams,
    ) -> String {
        let Some(repository) = self
            .state
            .repositories
            .iter_mut()
            .find(|repository| repository.id == params.repository_id)
        else {
            return encode_error(
                id,
                "repository_not_found",
                format!("repository {} not found", params.repository_id),
            );
        };
        repository.custom_name = Some(params.label.clone());
        self.state.mark_session_dirty();
        self.emit_event(EventEnvelope {
            event: EventKind::RepositoryRenamed,
            data: EventData::RepositoryRenamed {
                repository_id: params.repository_id.clone(),
                label: params.label,
            },
        });
        self.handle_repository_get(
            id,
            RepositoryTarget {
                repository_id: params.repository_id,
            },
        )
    }

    pub(super) fn handle_repository_move(
        &mut self,
        id: String,
        params: RepositoryMoveParams,
    ) -> String {
        let Some(source) = self.state.space_order.iter().position(|space| {
            *space == crate::repository::SpaceRef::Repository(params.repository_id.clone())
        }) else {
            return encode_error(
                id,
                "repository_not_found",
                format!("repository {} not found", params.repository_id),
            );
        };
        if params.insert_index > self.state.space_order.len() {
            return encode_error(
                id,
                "repository_move_failed",
                "insert_index is out of bounds",
            );
        }
        let item = self.state.space_order.remove(source);
        let target = if source < params.insert_index {
            params.insert_index.saturating_sub(1)
        } else {
            params.insert_index
        };
        self.state.space_order.insert(target, item);
        self.state.mark_session_dirty();
        self.emit_event(EventEnvelope {
            event: EventKind::RepositoryMoved,
            data: EventData::RepositoryMoved {
                repository_id: params.repository_id,
                insert_index: params.insert_index,
            },
        });
        encode_success(
            id,
            ResponseResult::RepositoryList {
                repositories: self.repository_list_info(),
            },
        )
    }

    pub(super) fn handle_checkout_move(
        &mut self,
        id: String,
        params: CheckoutMoveParams,
    ) -> String {
        let Some(checkout) = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == params.workspace_id)
            .and_then(|workspace| workspace.checkout.clone())
        else {
            return encode_error(
                id,
                "checkout_not_found",
                format!("checkout {} not found", params.workspace_id),
            );
        };
        if checkout.kind == crate::repository::CheckoutKind::Primary {
            return encode_error(
                id,
                "checkout_move_failed",
                "the live primary checkout is the repository root and cannot be reordered",
            );
        }
        let Some(repository) = self
            .state
            .repositories
            .iter_mut()
            .find(|repository| repository.id == checkout.repository_id)
        else {
            return encode_error(id, "repository_not_found", "checkout repository not found");
        };
        let Some(source) = repository
            .checkout_workspace_ids
            .iter()
            .position(|workspace_id| workspace_id == &params.workspace_id)
        else {
            return encode_error(id, "checkout_not_found", "checkout membership not found");
        };
        if params.insert_index > repository.checkout_workspace_ids.len() {
            return encode_error(id, "checkout_move_failed", "insert_index is out of bounds");
        }
        let item = repository.checkout_workspace_ids.remove(source);
        // checkout.move uses an insertion coordinate in the complete checkout
        // membership, including the primary at index zero. The primary is a
        // pinned root, so linked checkouts can never be inserted ahead of it.
        let requested = params.insert_index.max(1);
        let target = if source < requested {
            requested.saturating_sub(1)
        } else {
            requested
        };
        repository.checkout_workspace_ids.insert(target, item);
        self.state.mark_session_dirty();
        let response = self.handle_repository_get(
            id,
            RepositoryTarget {
                repository_id: checkout.repository_id,
            },
        );
        self.emit_event(EventEnvelope {
            event: EventKind::CheckoutMoved,
            data: EventData::CheckoutMoved {
                workspace_id: params.workspace_id,
                insert_index: target,
            },
        });
        response
    }

    pub(super) fn handle_repository_close(
        &mut self,
        id: String,
        target: RepositoryTarget,
    ) -> String {
        let Some(repository) = self.state.repository(&target.repository_id).cloned() else {
            return encode_error(
                id,
                "repository_not_found",
                format!("repository {} not found", target.repository_id),
            );
        };
        // Capture stable identities before the first close: each singleton close
        // reconciles membership and shifts workspace indices.
        let checkout_workspace_ids = repository.checkout_workspace_ids.clone();
        for workspace_id in checkout_workspace_ids {
            let Some(index) = self
                .state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == workspace_id)
            else {
                continue;
            };
            // Each member must take the same complete lifecycle path as a
            // direct workspace/checkout close. Suppress the final-member
            // repository event until every captured member has closed.
            self.close_workspace_with_lifecycle(index, false);
        }
        self.emit_event(EventEnvelope {
            event: EventKind::RepositoryClosed,
            data: EventData::RepositoryClosed {
                repository_id: target.repository_id,
            },
        });
        encode_success(id, ResponseResult::Ok {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{
        CollectionCreateMemberParams, CollectionCreateParams, SplitDirection, SuccessResponse,
        WorkspaceTarget,
    };
    use crate::config::Config;
    use crate::repository::{CheckoutKind, CheckoutProvenance, Repository, SpaceRef};
    use crate::workspace::Workspace;

    fn app_with_repository() -> App {
        let event_hub = crate::api::EventHub::default();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(&Config::default(), true, None, rx, event_hub);
        app.state.workspaces = vec![Workspace::test_new("main"), Workspace::test_new("feature")];
        let ids = app
            .state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        for (idx, workspace) in app.state.workspaces.iter_mut().enumerate() {
            workspace.checkout = Some(CheckoutProvenance {
                repository_id: "rtest".into(),
                checkout_path: format!("/repo/{idx}").into(),
                kind: if idx == 0 {
                    CheckoutKind::Primary
                } else {
                    CheckoutKind::Linked
                },
            });
        }
        app.state.repositories = vec![Repository {
            id: "rtest".into(),
            git_common_dir: "/repo/.git".into(),
            label: "repo".into(),
            custom_name: None,
            preferred_base: Some("origin/main".into()),
            checkout_workspace_ids: ids.clone(),
            last_focused_workspace_id: Some(ids[1].clone()),
        }];
        app.state.space_order = vec![SpaceRef::Repository("rtest".into())];
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.ensure_test_terminals();
        app
    }

    fn add_linked_checkout(app: &mut App, name: &str) -> String {
        let mut workspace = Workspace::test_new(name);
        workspace.checkout = Some(CheckoutProvenance {
            repository_id: "rtest".into(),
            checkout_path: format!("/repo/{name}").into(),
            kind: CheckoutKind::Linked,
        });
        let id = workspace.id.clone();
        app.state.workspaces.push(workspace);
        app.state.repositories[0]
            .checkout_workspace_ids
            .push(id.clone());
        app.state.ensure_test_terminals();
        id
    }

    fn add_delegated_collection(app: &mut App, workspace_id: &str) {
        let index = app
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == workspace_id)
            .expect("checkout exists");
        let root = app.state.workspaces[index].tabs[0]
            .root_pane
            .expect("test checkout root");
        let target_pane_id = app.public_pane_id(index, root).expect("public root");
        let created: serde_json::Value = serde_json::from_str(&app.handle_collection_create(
            "collection".into(),
            CollectionCreateParams {
                target_pane_id,
                direction: SplitDirection::Right,
                ratio: Some(0.4),
                label: Some("delegated".into()),
                focus: false,
            },
        ))
        .expect("collection response");
        let collection_id = created["result"]["collection"]["collection_id"]
            .as_str()
            .expect("collection id")
            .to_owned();
        let member: serde_json::Value = serde_json::from_str(&app.handle_collection_create_member(
            "member".into(),
            CollectionCreateMemberParams {
                collection_id,
                cwd: None,
                env: Default::default(),
                delegation_parent_id: None,
                purpose: Some("adversarial lifecycle".into()),
            },
        ))
        .expect("member response");
        assert!(member.get("error").is_none(), "{member}");
    }

    #[test]
    fn repository_focus_uses_mru_checkout() {
        let mut app = app_with_repository();
        let response = app.handle_repository_focus(
            "req".into(),
            RepositoryTarget {
                repository_id: "rtest".into(),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::RepositoryInfo { .. }
        ));
        assert_eq!(app.state.active, Some(1));
    }

    #[test]
    fn checkout_move_uses_full_membership_coordinates_with_pinned_primary() {
        let mut app = app_with_repository();
        for name in ["second", "third"] {
            let mut workspace = Workspace::test_new(name);
            workspace.checkout = Some(CheckoutProvenance {
                repository_id: "rtest".into(),
                checkout_path: format!("/repo/{name}").into(),
                kind: CheckoutKind::Linked,
            });
            app.state.repositories[0]
                .checkout_workspace_ids
                .push(workspace.id.clone());
            app.state.workspaces.push(workspace);
        }
        let primary = app.state.workspaces[0].id.clone();
        let first_linked = app.state.workspaces[1].id.clone();
        let second_linked = app.state.workspaces[2].id.clone();
        let third_linked = app.state.workspaces[3].id.clone();

        let _: SuccessResponse = serde_json::from_str(&app.handle_checkout_move(
            "down".into(),
            CheckoutMoveParams {
                workspace_id: first_linked.clone(),
                insert_index: 4,
            },
        ))
        .unwrap();
        assert_eq!(
            app.state.repositories[0].checkout_workspace_ids,
            vec![
                primary.clone(),
                second_linked.clone(),
                third_linked.clone(),
                first_linked.clone()
            ]
        );

        let _: SuccessResponse = serde_json::from_str(&app.handle_checkout_move(
            "up".into(),
            CheckoutMoveParams {
                workspace_id: first_linked.clone(),
                insert_index: 1,
            },
        ))
        .unwrap();
        assert_eq!(
            app.state.repositories[0].checkout_workspace_ids,
            vec![primary, first_linked, second_linked, third_linked]
        );
    }

    #[test]
    fn compatibility_workspace_close_never_closes_sibling_checkout() {
        let mut app = app_with_repository();
        let first = app.state.workspaces[0].id.clone();
        let second = app.state.workspaces[1].id.clone();
        let response = app.handle_workspace_close(
            "req".into(),
            WorkspaceTarget {
                workspace_id: first,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(app.state.workspaces.len(), 1);
        assert_eq!(app.state.workspaces[0].id, second);
    }

    #[test]
    fn repository_and_checkout_moves_update_only_their_own_order() {
        let mut app = app_with_repository();
        let first = app.state.workspaces[0].id.clone();
        let second = app.state.workspaces[1].id.clone();
        let mut notes = Workspace::test_new("notes");
        let notes_id = notes.id.clone();
        notes.checkout = None;
        app.state.workspaces.push(notes);
        app.state
            .space_order
            .push(SpaceRef::StandaloneWorkspace(notes_id.clone()));

        let response = app.handle_checkout_move(
            "checkout".into(),
            CheckoutMoveParams {
                workspace_id: second.clone(),
                insert_index: 0,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            app.state.repositories[0].checkout_workspace_ids,
            vec![first, second],
            "a full-membership coordinate of zero cannot unpin the primary"
        );
        assert!(app
            .event_hub
            .events_after(0)
            .iter()
            .any(|(_, event)| { event.event == EventKind::CheckoutMoved }));
        assert_eq!(
            app.state.space_order[0],
            SpaceRef::Repository("rtest".into())
        );

        let response = app.handle_repository_move(
            "repository".into(),
            RepositoryMoveParams {
                repository_id: "rtest".into(),
                insert_index: 2,
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(
            app.state.space_order,
            vec![
                SpaceRef::StandaloneWorkspace(notes_id),
                SpaceRef::Repository("rtest".into()),
            ]
        );
    }

    #[test]
    fn workspace_lifecycle_events_emit_checkout_aliases() {
        let mut app = app_with_repository();
        let workspace_id = app.state.workspaces[0].id.clone();
        let response = app.handle_workspace_rename(
            "rename".into(),
            crate::api::schema::WorkspaceRenameParams {
                workspace_id,
                label: "renamed".into(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        let kinds = app
            .event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            vec![EventKind::WorkspaceRenamed, EventKind::CheckoutRenamed]
        );
    }

    #[tokio::test]
    async fn workspace_close_preserves_peer_checkouts_with_nested_lifecycle_state() {
        let mut app = app_with_repository();
        let third = add_linked_checkout(&mut app, "third");
        let first = app.state.workspaces[0].id.clone();
        let target = app.state.workspaces[1].id.clone();
        add_delegated_collection(&mut app, &first);
        add_delegated_collection(&mut app, &target);
        add_delegated_collection(&mut app, &third);
        let sequence = app.event_hub.current_sequence();

        let _: SuccessResponse = serde_json::from_str(&app.handle_workspace_close(
            "close".into(),
            WorkspaceTarget {
                workspace_id: target.clone(),
            },
        ))
        .unwrap();

        assert_eq!(
            app.state
                .workspaces
                .iter()
                .map(|workspace| workspace.id.clone())
                .collect::<Vec<_>>(),
            vec![first.clone(), third.clone()]
        );
        let events = app.event_hub.events_after(sequence);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| event.event == EventKind::WorkspaceClosed)
                .count(),
            1
        );
        assert!(events.iter().any(|(_, event)| {
            matches!(&event.data, EventData::WorkspaceClosed { workspace_id, .. } if workspace_id == &target)
        }));
        assert!(!events.iter().any(|(_, event)| {
            matches!(&event.data, EventData::PaneClosed { workspace_id, .. } if workspace_id == &first || workspace_id == &third)
        }));
        for kind in [
            EventKind::CollectionClosed,
            EventKind::DelegationTombstoned,
            EventKind::DelegationGarbageCollected,
            EventKind::TabClosed,
            EventKind::WorkspaceClosed,
            EventKind::CheckoutClosed,
        ] {
            assert!(
                events.iter().any(|(_, event)| event.event == kind),
                "missing {kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn repository_close_closes_every_checkout_explicitly() {
        let mut app = app_with_repository();
        add_linked_checkout(&mut app, "third");
        let checkout_ids = app
            .state
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        for checkout_id in &checkout_ids {
            add_delegated_collection(&mut app, checkout_id);
        }
        let sequence = app.event_hub.current_sequence();
        let response = app.handle_repository_close(
            "req".into(),
            RepositoryTarget {
                repository_id: "rtest".into(),
            },
        );
        let _: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(app.state.workspaces.is_empty());
        assert!(app.state.repositories.is_empty());
        assert!(app.state.space_order.is_empty());
        let events = app.event_hub.events_after(sequence);
        let mut expected = Vec::new();
        for _ in &checkout_ids {
            expected.extend([
                EventKind::CollectionMemberRemoved,
                EventKind::CollectionClosed,
                EventKind::PaneClosed,
                EventKind::PaneClosed,
                EventKind::DelegationTombstoned,
                EventKind::DelegationGarbageCollected,
                EventKind::TabClosed,
                EventKind::WorkspaceClosed,
                EventKind::CheckoutClosed,
            ]);
        }
        expected.push(EventKind::RepositoryClosed);
        assert_eq!(
            events
                .into_iter()
                .map(|(_, event)| event.event)
                .collect::<Vec<_>>(),
            expected,
            "repository close emits each singleton lifecycle once, then its container close"
        );
    }
}
