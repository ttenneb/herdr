use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentPromptParams, AgentRenameParams, AgentSendKeysParams, AgentStartParams, AgentTarget,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(super) fn handle_agent_prompt(&mut self, id: String, params: AgentPromptParams) -> String {
        if params.text.is_empty() {
            return encode_error(id, "empty_agent_prompt", "agent prompt must not be empty");
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return agent_not_found(id, &params.target);
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            );
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return agent_not_ready(id, &params.target);
        };
        if terminal.managed_agent_launch_pending() {
            return agent_not_ready(id, &params.target);
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            );
        }
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => return encode_error(id, "agent_prompt_failed", err.to_string()),
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return encode_error(id, "agent_prompt_failed", err.to_string());
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        let result = self
            .lookup_runtime_sender(resolved.ws_idx, resolved.pane_id)
            .expect("runtime was just verified")
            .try_send_prompt_transaction(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
            );
        if let Err(err) = result {
            let code = match err {
                crate::pane::PromptTransactionAdmissionError::Full => "agent_prompt_queue_full",
                crate::pane::PromptTransactionAdmissionError::PayloadTooLarge => {
                    "agent_prompt_payload_too_large"
                }
                crate::pane::PromptTransactionAdmissionError::InputFull
                | crate::pane::PromptTransactionAdmissionError::Closed => "agent_prompt_failed",
            };
            return encode_error(id, code, err.to_string());
        }
        // A response means this runtime admitted the complete transaction. Only then
        // may prompt input restore an archived collection member.
        if let Some(restore) = self.begin_archived_member_input(resolved.ws_idx, resolved.pane_id) {
            self.commit_archived_member_input(restore);
        }
        self.acknowledge_terminal_input(&terminal_id);
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        encode_success(id, ResponseResult::AgentPrompted { agent })
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        let accepted = !bytes.is_empty();
        let restore = accepted
            .then(|| self.begin_archived_member_input(resolved.ws_idx, resolved.pane_id))
            .flatten();
        let result = self
            .lookup_runtime_sender(resolved.ws_idx, resolved.pane_id)
            .expect("runtime was just verified")
            .try_send_bytes(Bytes::from(bytes));
        if let Err(err) = result {
            if let Some(restore) = restore {
                self.rollback_archived_member_input(restore);
            }
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }
        if let Some(restore) = restore {
            self.commit_archived_member_input(restore);
        }
        if accepted {
            self.acknowledge_terminal_input(&terminal_id);
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    #[tokio::test]
    async fn agent_input_restores_archived_members_only_after_delivery() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let collection = app.state.workspaces[0]
            .create_collection_near(
                0,
                crate::layout::LayoutLeaf::Pane(pane_id),
                ratatui::layout::Direction::Horizontal,
                0.5,
                None,
            )
            .expect("create collection");
        app.state.workspaces[0]
            .collect_pane(pane_id, collection)
            .expect("collect pane");
        app.state.workspaces[0]
            .set_collection_member_archived(pane_id, collection, true)
            .expect("archive member");
        let archive_revision = app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .revision();
        let archived_at = std::time::SystemTime::now();
        app.state
            .collection_archive_times
            .insert(pane_id, archived_at);
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).expect("terminal");
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let public_pane_id = app.public_pane_id(0, pane_id).expect("public pane");
        let events_before = app.event_hub.current_sequence();

        let (runtime, _rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 16);
        for _ in 0..8 {
            runtime
                .try_send_prompt_transaction(
                    Bytes::from_static(b"occupied"),
                    Bytes::from_static(b"\r"),
                    Duration::from_secs(1),
                )
                .expect("fill the runtime-owned prompt transaction queue");
        }
        app.state.insert_test_runtime(pane_id, runtime);
        let failed = app.handle_agent_prompt(
            "failed".into(),
            AgentPromptParams {
                target: public_pane_id.clone(),
                text: "resume".into(),
                wait: None,
            },
        );
        let failed: crate::api::schema::ErrorResponse = serde_json::from_str(&failed).unwrap();
        assert_eq!(failed.error.code, "agent_prompt_queue_full");
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(pane_id));
        assert_eq!(app.state.collection_archive_times[&pane_id], archived_at);
        assert_eq!(
            app.state.workspaces[0].tabs[0]
                .collection(collection)
                .expect("collection")
                .revision(),
            archive_revision
        );
        assert_eq!(app.event_hub.current_sequence(), events_before);

        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
        app.state.insert_test_runtime(pane_id, runtime);
        let empty = app.handle_agent_send_keys(
            "empty".into(),
            AgentSendKeysParams {
                target: public_pane_id.clone(),
                keys: Vec::new(),
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&empty).is_ok());
        assert_eq!(rx.try_recv().expect("empty input accepted"), Bytes::new());
        assert!(app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(pane_id));
        assert_eq!(app.state.collection_archive_times[&pane_id], archived_at);
        assert_eq!(app.event_hub.current_sequence(), events_before);

        let sent = app.handle_agent_send_keys(
            "sent".into(),
            AgentSendKeysParams {
                target: public_pane_id,
                keys: vec!["enter".into()],
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&sent).is_ok());
        assert_eq!(
            rx.try_recv().expect("input accepted"),
            Bytes::from_static(b"\r")
        );
        assert!(!app.state.workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection")
            .is_archived(pane_id));
        assert!(!app.state.collection_archive_times.contains_key(&pane_id));
        assert_eq!(app.event_hub.current_sequence(), events_before + 1);
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 1,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw_response = app.handle_agent_prompt(
            "req-raw".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw_response: SuccessResponse = serde_json::from_str(&raw_response).unwrap();
        assert!(matches!(
            raw_response.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert!(rx.try_recv().is_err());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = app.handle_agent_prompt(
            "req-label".into(),
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn concurrent_agent_prompts_keep_each_text_and_delayed_enter_together() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
        app.state.insert_test_runtime(pane_id, runtime);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for (id, text) in [("one", "first"), ("two", "second")] {
            let response = app.handle_agent_prompt(
                id.into(),
                AgentPromptParams {
                    target: target.clone(),
                    text: text.into(),
                    wait: None,
                },
            );
            assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        }

        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"first"));
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"second"));
        assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn prompt_close_or_runtime_replacement_cancels_delayed_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .unwrap()
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut old_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
        app.state.insert_test_runtime(pane_id, runtime);
        let target = app.public_pane_id(0, pane_id).unwrap();
        let response = app.handle_agent_prompt(
            "close".into(),
            AgentPromptParams {
                target,
                text: "first".into(),
                wait: None,
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        assert_eq!(old_rx.recv().await.unwrap(), Bytes::from_static(b"first"));

        // Replacing the runtime drops the old identity and cancels its queue.
        let (replacement, mut replacement_rx) =
            crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 2);
        app.state.workspaces[0].insert_test_runtime(pane_id, replacement);
        assert_ne!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                old_rx.recv()
            )
            .await
            .ok()
            .flatten(),
            Some(Bytes::from_static(b"\r")),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), replacement_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap(),
            Bytes::from_static(b"\r")
        );
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = app.handle_agent_prompt(
            "req-pending".into(),
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_does_not_acknowledge_primary_agent() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Done);
        assert!(!app.state.workspaces[0].tabs[0].panes[&pane_id].seen);
    }

    #[test]
    fn agent_focus_does_not_acknowledge_delegated_completion() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);
        let root = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let child = app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        let child_terminal = app.state.workspaces[0].tabs[0].panes[&child]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&child_terminal)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&child)
            .unwrap()
            .seen = false;
        let parent = app
            .state
            .delegations
            .create(Some(root), None, None)
            .unwrap();
        app.state
            .delegations
            .create(Some(child), Some(parent), Some("review".into()))
            .unwrap();

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, child).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Done);
        assert!(!app.state.workspaces[0].tabs[0].panes[&child].seen);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }
}
