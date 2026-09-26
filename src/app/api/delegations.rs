use crate::api::schema::{
    DelegationCreateParams, DelegationInfo, DelegationReorderParams, DelegationReparentParams,
    DelegationRouteReadyParams, DelegationSiblingPosition, DelegationTarget, DelegationTreeEntry,
    EventData, EventEnvelope, EventKind, ResponseResult,
};
use crate::app::App;
use crate::delegation::{DelegationId, DelegationRecord, SiblingPosition};

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_delegation_create(
        &mut self,
        id: String,
        params: DelegationCreateParams,
    ) -> String {
        let pane_id = match params.pane_id.as_deref() {
            Some(raw) => match self.parse_pane_id(raw) {
                Some((_, pane)) => Some(pane),
                None => return encode_error(id, "pane_not_found", format!("pane {raw} not found")),
            },
            None => None,
        };
        let parent_id = match parse_optional_id(params.parent_id.as_deref()) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let delegation_id = match self.state.delegations.create(
            pane_id,
            parent_id,
            normalize_purpose(params.purpose),
        ) {
            Ok(value) => value,
            Err(err) => return delegation_error(id, err),
        };
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let delegation = self.delegation_info(
            self.state
                .delegations
                .get(delegation_id)
                .expect("created delegation exists"),
        );
        self.emit_event(EventEnvelope {
            event: EventKind::DelegationCreated,
            data: EventData::DelegationCreated {
                delegation: delegation.clone(),
            },
        });
        self.emit_all_workspace_attention_updated();
        encode_success(id, ResponseResult::DelegationInfo { delegation })
    }

    pub(super) fn handle_delegation_route_ready(
        &mut self,
        id: String,
        params: DelegationRouteReadyParams,
    ) -> String {
        let child = match parse_id(&params.child_delegation_id) {
            Ok(id) => id,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let parent = match parse_id(&params.expected_parent_delegation_id) {
            Ok(id) => id,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let Some(mut shape) = self.ready_route_shape(child, parent) else {
            self.ready_delegation_routes.remove(&child);
            return encode_error(
                id,
                "route_not_ready",
                "exact active child and parent Pi sessions are required",
            );
        };
        // A same-shape marker is not an acknowledgment if its exclusive
        // directory lease was unlinked or replaced. Quarantine every route
        // before attempting a fresh durable save under new ownership.
        if !self
            .session_writer_healthy
            .load(std::sync::atomic::Ordering::Acquire)
            || !self
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.validate(&self.session_save_path).is_ok())
        {
            self.session_writer_healthy
                .store(false, std::sync::atomic::Ordering::Release);
            self.ready_delegation_routes.clear();
        }
        if let Some(existing) = self.ready_delegation_routes.get(&child) {
            let mut same = shape.clone();
            same.epoch = existing.epoch.clone();
            if &same == existing {
                let delegation = self.delegation_info(
                    self.state
                        .delegations
                        .get(child)
                        .expect("ready child exists"),
                );
                return encode_success(
                    id,
                    ResponseResult::DelegationRouteReady {
                        delegation,
                        route_epoch: existing.epoch.clone(),
                    },
                );
            }
        }
        self.ready_delegation_routes.remove(&child);
        let Some(epoch) = crate::platform::random_route_epoch() else {
            return encode_error(id, "route_not_ready", "server route epoch unavailable");
        };
        if let Err(err) = self.durably_save_delegation_edge() {
            return encode_error(id, "route_persistence_failed", err.to_string());
        }
        let Some(mut current) = self.ready_route_shape(child, parent) else {
            return encode_error(id, "route_not_ready", "edge changed during durable save");
        };
        if current != shape {
            return encode_error(id, "route_not_ready", "edge changed during durable save");
        }
        shape.epoch = epoch.clone();
        current.epoch = epoch.clone();
        self.ready_delegation_routes.insert(child, current);
        // #159: a covered child's domain must be durable before its accepted
        // stream can offer any parent-report path on this epoch.
        if let Err(err) = self.create_covered_domain_on_route_ready(child) {
            self.ready_delegation_routes.remove(&child);
            return encode_error(id, "coverage_persistence_failed", err);
        }
        let delegation = self.delegation_info(
            self.state
                .delegations
                .get(child)
                .expect("ready child exists"),
        );
        encode_success(
            id,
            ResponseResult::DelegationRouteReady {
                delegation,
                route_epoch: epoch,
            },
        )
    }

    pub(super) fn handle_delegation_get(&self, id: String, target: DelegationTarget) -> String {
        let delegation_id = match parse_id(&target.delegation_id) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let Some(record) = self.state.delegations.get(delegation_id) else {
            return encode_error(
                id,
                "delegation_not_found",
                format!("delegation {} not found", target.delegation_id),
            );
        };
        encode_success(
            id,
            ResponseResult::DelegationInfo {
                delegation: self.delegation_info(record),
            },
        )
    }

    pub(super) fn handle_delegation_tree(&self, id: String) -> String {
        let delegations = self
            .state
            .delegations
            .preorder()
            .into_iter()
            .filter_map(|delegation_id| {
                let record = self.state.delegations.get(delegation_id)?;
                let mut depth = 0;
                let mut parent = record.parent_id;
                while let Some(parent_id) = parent {
                    depth += 1;
                    parent = self
                        .state
                        .delegations
                        .get(parent_id)
                        .and_then(|record| record.parent_id);
                    if depth > self.state.delegations.records().len() {
                        break;
                    }
                }
                Some(DelegationTreeEntry {
                    delegation: self.delegation_info(record),
                    depth,
                })
            })
            .collect();
        encode_success(id, ResponseResult::DelegationTree { delegations })
    }

    pub(super) fn handle_delegation_root(&self, id: String, target: DelegationTarget) -> String {
        let delegation_id = match parse_id(&target.delegation_id) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let root = match self.state.delegations.root(delegation_id) {
            Ok(value) => value,
            Err(err) => return delegation_error(id, err),
        };
        let delegation =
            self.delegation_info(self.state.delegations.get(root).expect("root exists"));
        encode_success(id, ResponseResult::DelegationInfo { delegation })
    }

    pub(super) fn handle_delegation_descendants(
        &self,
        id: String,
        target: DelegationTarget,
    ) -> String {
        let delegation_id = match parse_id(&target.delegation_id) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let ids = match self.state.delegations.descendants(delegation_id) {
            Ok(value) => value,
            Err(err) => return delegation_error(id, err),
        };
        let delegations = ids
            .into_iter()
            .filter_map(|value| self.state.delegations.get(value))
            .map(|record| self.delegation_info(record))
            .collect();
        encode_success(id, ResponseResult::DelegationList { delegations })
    }

    pub(super) fn handle_delegation_reparent(
        &mut self,
        id: String,
        params: DelegationReparentParams,
    ) -> String {
        let delegation_id = match parse_id(&params.delegation_id) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let parent_id = match parse_optional_id(params.parent_id.as_deref()) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let previous_parent_id = self
            .state
            .delegations
            .get(delegation_id)
            .and_then(|record| record.parent_id)
            .map(|value| value.to_string());
        self.ready_delegation_routes.remove(&delegation_id);
        if let Err(err) = self.state.delegations.reparent(delegation_id, parent_id) {
            return delegation_error(id, err);
        }
        // A route change must not carry the old managed Pi identity forward.
        if let Some(pane_id) = self
            .state
            .delegations
            .get(delegation_id)
            .and_then(|d| d.pane_id)
        {
            if let Some((ws_idx, _)) = self.find_pane(pane_id) {
                if let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane_id) {
                    self.managed_pi_launches.remove(terminal_id);
                }
            }
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let delegation = self.delegation_info(
            self.state
                .delegations
                .get(delegation_id)
                .expect("reparented delegation exists"),
        );
        self.emit_event(EventEnvelope {
            event: EventKind::DelegationReparented,
            data: EventData::DelegationReparented {
                delegation: delegation.clone(),
                previous_parent_id,
            },
        });
        self.emit_all_workspace_attention_updated();
        encode_success(id, ResponseResult::DelegationInfo { delegation })
    }

    pub(super) fn handle_delegation_reorder(
        &mut self,
        id: String,
        params: DelegationReorderParams,
    ) -> String {
        let delegation_id = match parse_id(&params.delegation_id) {
            Ok(value) => value,
            Err(message) => return encode_error(id, "invalid_delegation_id", message),
        };
        let position = match params.position {
            DelegationSiblingPosition::First => SiblingPosition::First,
            DelegationSiblingPosition::Last => SiblingPosition::Last,
            DelegationSiblingPosition::Before { delegation_id } => match parse_id(&delegation_id) {
                Ok(value) => SiblingPosition::Before(value),
                Err(message) => return encode_error(id, "invalid_delegation_id", message),
            },
            DelegationSiblingPosition::After { delegation_id } => match parse_id(&delegation_id) {
                Ok(value) => SiblingPosition::After(value),
                Err(message) => return encode_error(id, "invalid_delegation_id", message),
            },
        };
        if let Err(err) = self.state.delegations.reorder(delegation_id, position) {
            return delegation_error(id, err);
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        let delegation = self.delegation_info(
            self.state
                .delegations
                .get(delegation_id)
                .expect("reordered delegation exists"),
        );
        self.emit_event(EventEnvelope {
            event: EventKind::DelegationReordered,
            data: EventData::DelegationReordered {
                delegation: delegation.clone(),
            },
        });
        encode_success(id, ResponseResult::DelegationInfo { delegation })
    }

    pub(super) fn delegation_info(&self, record: &DelegationRecord) -> DelegationInfo {
        DelegationInfo {
            delegation_id: record.id.to_string(),
            pane_id: record.pane_id.and_then(|pane| {
                self.state
                    .workspaces
                    .iter()
                    .enumerate()
                    .find_map(|(ws_idx, ws)| {
                        ws.find_tab_index_for_pane(pane)
                            .and_then(|_| self.public_pane_id(ws_idx, pane))
                    })
            }),
            parent_id: record.parent_id.map(|value| value.to_string()),
            purpose: record.purpose.clone(),
            sibling_rank: record.sibling_rank,
            tombstone: record.tombstone,
        }
    }
}

fn parse_id(raw: &str) -> Result<DelegationId, String> {
    raw.parse::<DelegationId>().map_err(|err| err.to_string())
}
fn parse_optional_id(raw: Option<&str>) -> Result<Option<DelegationId>, String> {
    raw.map(parse_id).transpose()
}
fn normalize_purpose(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().chars().take(200).collect::<String>())
        .filter(|value| !value.is_empty())
}
fn delegation_error(id: String, err: crate::delegation::DelegationError) -> String {
    let code = match err {
        crate::delegation::DelegationError::NotFound(_)
        | crate::delegation::DelegationError::ParentNotFound(_) => "delegation_not_found",
        crate::delegation::DelegationError::SelfParent
        | crate::delegation::DelegationError::Cycle
        | crate::delegation::DelegationError::CorruptCycle => "delegation_cycle",
        crate::delegation::DelegationError::PaneAlreadyAssociated(_) => "pane_already_delegated",
        crate::delegation::DelegationError::NotSibling(_) => "delegation_not_sibling",
        _ => "delegation_mutation_failed",
    };
    encode_error(id, code, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{Method, Request};
    use crate::{config::Config, workspace::Workspace};

    fn app() -> (App, String, String) {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        let mut workspace = Workspace::test_new("delegations-api");
        let root = workspace.tabs[0].root_pane.expect("root");
        let child = workspace.test_split(ratatui::layout::Direction::Horizontal);
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let root = app.public_pane_id(0, root).expect("root public");
        let child = app.public_pane_id(0, child).expect("child public");
        (app, root, child)
    }

    fn request(app: &mut App, method: Method) -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(Request {
            id: "test".into(),
            method,
        }))
        .expect("response")
    }

    #[test]
    fn delegation_api_covers_create_get_tree_root_descendants_reparent_and_reorder() {
        let (mut app, root_pane, child_pane) = app();
        let root = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(root_pane),
                parent_id: None,
                purpose: Some("primary".into()),
            }),
        );
        let root_id = root["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("root ID")
            .to_string();
        let child = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(child_pane.clone()),
                parent_id: Some(root_id.clone()),
                purpose: Some("review".into()),
            }),
        );
        let child_id = child["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("child ID")
            .to_string();
        let (child_ws, child_local_pane) = app.parse_pane_id(&child_pane).unwrap();
        let child_terminal = app.state.workspaces[child_ws]
            .terminal_id(child_local_pane)
            .unwrap()
            .clone();
        app.managed_pi_launches.insert(
            child_terminal.clone(),
            crate::app::agents::ManagedPiLaunch {
                generation: 1,
                session_path: "/tmp/owned.jsonl".into(),
                earliest_birth_ticks: 1,
                process: None,
            },
        );
        let detached = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: None,
                parent_id: None,
                purpose: None,
            }),
        );
        let detached_id = detached["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("detached ID")
            .to_string();

        let tree = request(
            &mut app,
            Method::DelegationTree(crate::api::schema::EmptyParams::default()),
        );
        assert_eq!(
            tree["result"]["delegations"]
                .as_array()
                .expect("tree")
                .len(),
            3
        );
        let root_query = request(
            &mut app,
            Method::DelegationRoot(DelegationTarget {
                delegation_id: child_id.clone(),
            }),
        );
        assert_eq!(root_query["result"]["delegation"]["delegation_id"], root_id);
        let descendants = request(
            &mut app,
            Method::DelegationDescendants(DelegationTarget {
                delegation_id: root_id.clone(),
            }),
        );
        assert_eq!(
            descendants["result"]["delegations"][0]["delegation_id"],
            child_id
        );

        let reparented = request(
            &mut app,
            Method::DelegationReparent(DelegationReparentParams {
                delegation_id: child_id.clone(),
                parent_id: None,
            }),
        );
        assert!(reparented["result"]["delegation"]["parent_id"].is_null());
        assert!(
            !app.managed_pi_launches.contains_key(&child_terminal),
            "reparent revokes the old launch identity"
        );
        let reordered = request(
            &mut app,
            Method::DelegationReorder(DelegationReorderParams {
                delegation_id: child_id.clone(),
                position: DelegationSiblingPosition::Before {
                    delegation_id: detached_id,
                },
            }),
        );
        assert_eq!(reordered["result"]["delegation"]["sibling_rank"], 1);
        let get = request(
            &mut app,
            Method::DelegationGet(DelegationTarget {
                delegation_id: child_id,
            }),
        );
        assert_eq!(get["result"]["delegation"]["purpose"], "review");
    }

    #[test]
    fn delegation_api_rejects_cycles_and_duplicate_panes_atomically() {
        let (mut app, root_pane, child_pane) = app();
        let root = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(root_pane.clone()),
                parent_id: None,
                purpose: None,
            }),
        );
        let root_id = root["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("ID")
            .to_string();
        let child = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(child_pane),
                parent_id: Some(root_id.clone()),
                purpose: None,
            }),
        );
        let child_id = child["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("ID")
            .to_string();
        let cycle = request(
            &mut app,
            Method::DelegationReparent(DelegationReparentParams {
                delegation_id: root_id.clone(),
                parent_id: Some(child_id),
            }),
        );
        assert_eq!(cycle["error"]["code"], "delegation_cycle");
        let duplicate = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(root_pane),
                parent_id: None,
                purpose: None,
            }),
        );
        assert_eq!(duplicate["error"]["code"], "pane_already_delegated");
        let root = request(
            &mut app,
            Method::DelegationGet(DelegationTarget {
                delegation_id: root_id,
            }),
        );
        assert!(root["result"]["delegation"]["parent_id"].is_null());
    }

    #[test]
    fn closing_parent_pane_retains_queryable_tombstone_while_child_exists() {
        let (mut app, root_pane, child_pane) = app();
        let root = request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(root_pane.clone()),
                parent_id: None,
                purpose: None,
            }),
        );
        let root_id = root["result"]["delegation"]["delegation_id"]
            .as_str()
            .expect("ID")
            .to_string();
        request(
            &mut app,
            Method::DelegationCreate(DelegationCreateParams {
                pane_id: Some(child_pane),
                parent_id: Some(root_id.clone()),
                purpose: None,
            }),
        );
        let closed = request(
            &mut app,
            Method::PaneClose(crate::api::schema::PaneTarget { pane_id: root_pane }),
        );
        assert_eq!(closed["result"]["type"], "ok");
        let root = request(
            &mut app,
            Method::DelegationGet(DelegationTarget {
                delegation_id: root_id,
            }),
        );
        assert_eq!(root["result"]["delegation"]["tombstone"], true);
        assert!(root["result"]["delegation"]["pane_id"].is_null());
    }
}
