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
        if expected_agent == crate::detect::Agent::Pi {
            let terminal_key = terminal_id.to_string();
            let sender = self.attribute_sender(params.send.caller_pid);
            let message = crate::app::messages::parse_structured_prompt(&params.text)
                .unwrap_or_else(|| crate::app::messages::OutgoingMessage {
                    origin: "agent_prompt",
                    subject: crate::app::messages::subject_for("Message from", &sender.label),
                    body: params.text.clone(),
                    priority: "normal".into(),
                    kind: "advisory".into(),
                    message_id: None,
                    correlation: None,
                    replace_pending: false,
                });
            match self.route_ordinary_send(&terminal_key, &sender, message, &params.send) {
                Ok(crate::app::messages::SendRoute::Pty) => {}
                Ok(crate::app::messages::SendRoute::Mailbox(delivery)) => {
                    let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
                        return agent_not_found(id, &params.target);
                    };
                    return encode_success(
                        id,
                        ResponseResult::AgentPrompted {
                            agent,
                            delivery: Some(delivery),
                        },
                    );
                }
                Err(refusal) => {
                    if let Some(json) =
                        crate::app::messages::pending_error_json(id.clone(), &refusal)
                    {
                        return json;
                    }
                    return match refusal {
                        crate::app::messages::SendRefusal::MailboxUnavailable(message) => {
                            encode_error(id, "messages_unavailable", message)
                        }
                        crate::app::messages::SendRefusal::Store(message) => {
                            encode_error(id, "mailbox_store_failed", message)
                        }
                        _ => encode_error(id, "agent_prompt_failed", "send refused"),
                    };
                }
            }
        } else if params.send.transport == Some(crate::api::schema::MessageTransport::Mailbox) {
            return encode_error(
                id,
                "messages_unavailable",
                "only Pi recipients have a Messages queue",
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
        encode_success(
            id,
            ResponseResult::AgentPrompted {
                agent,
                delivery: None,
            },
        )
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
        if params
            .expected_terminal_id
            .as_deref()
            .is_some_and(|expected| expected != terminal_id.as_str())
        {
            return encode_error(
                id,
                "agent_identity_mismatch",
                "exact terminal guard did not match; no keys were sent",
            );
        }
        if let Some(expected_name) = params.expected_name.as_deref() {
            let actual_name = self
                .state
                .terminals
                .get(&terminal_id)
                .and_then(|terminal| terminal.agent_name.as_deref());
            if actual_name != Some(expected_name) {
                return encode_error(
                    id,
                    "agent_identity_mismatch",
                    "exact managed-agent generation guard did not match; no keys were sent",
                );
            }
        }
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
        events::AppEvent,
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

    // This is the observed #90 process shape, produced by Node's real Linux
    // process.title behavior and inspected through the same /proc job reader.
    #[cfg(target_os = "linux")]
    #[test]
    fn titled_pi_uses_only_generation_bound_server_launch_provenance() {
        use std::os::unix::{fs::PermissionsExt, process::CommandExt};
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut app = app_with_agent();
        let directory = std::env::temp_dir().join(format!(
            "herdr-titled-pi-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        app.sender_authority_dir = directory.clone();
        let session = directory.join("owned.jsonl");
        std::fs::write(
            &session,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"owned\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).unwrap();
        let pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        let key = terminal_id.to_string();
        let floor = crate::platform::first_post_launch_birth_tick().unwrap();
        assert!(crate::platform::wait_until_birth_tick(floor));
        let mut command = std::process::Command::new("node");
        command
            .arg("-e")
            .arg("process.title='pi';setInterval(()=>{},1000)")
            .arg("--")
            .arg("--session")
            .arg(&session)
            .process_group(0);
        let child = ChildGuard(command.spawn().unwrap());
        let mut job = None;
        for _ in 0..100 {
            let observed = crate::platform::foreground_group_leader_job(child.0.id());
            if observed
                .as_ref()
                .and_then(|job| job.processes.first())
                .and_then(|process| process.argv.as_deref())
                == Some(&["pi".to_string()][..])
            {
                job = observed;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let job = job.expect("live Node process changed /proc argv to [pi]");
        let birth = crate::platform::process_birth_identity(child.0.id()).unwrap();
        assert!(birth.start_ticks >= floor);
        assert_eq!(
            crate::detect::identify_agent_process_in_job(&job)
                .unwrap()
                .0,
            Agent::Pi
        );
        app.install_mailbox_bootstrap_test_foreground_job(terminal_id.clone(), job);
        let store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &key).unwrap();
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: key,
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .unwrap();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .begin_managed_agent(
                "titled".into(),
                Agent::Pi,
                std::time::Instant::now(),
                Duration::from_secs(3),
                Duration::from_secs(30),
            );
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_managed_agent_generation(1);
        // The original --session selector was committed by the server; /proc
        // only shows pi. Pane launch_argv remains unused and unavailable.
        app.managed_pi_launches.insert(
            terminal_id.clone(),
            crate::app::agents::ManagedPiLaunch {
                generation: 1,
                session_path: session.display().to_string(),
                earliest_birth_ticks: floor,
                process: None,
            },
        );
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: pane,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        let target = app.public_pane_id(0, pane).unwrap();
        let get = |app: &mut App| {
            let response = app.handle_agent_get(
                "titled".into(),
                AgentTarget {
                    target: target.clone(),
                },
            );
            let result: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::AgentInfo { agent } = result.result else {
                panic!("agent get")
            };
            agent.agent_session
        };
        assert_eq!(
            get(&mut app)
                .expect("server-owned session despite titled argv")
                .value,
            session.display().to_string()
        );
        app.managed_pi_launches
            .get_mut(&terminal_id)
            .unwrap()
            .process
            .as_mut()
            .unwrap()
            .start_ticks += 1;
        assert!(
            get(&mut app).is_none(),
            "same PID with changed birth must fail closed"
        );
        app.managed_pi_launches
            .get_mut(&terminal_id)
            .unwrap()
            .process = Some(birth);
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(get(&mut app).is_none(), "private file remains mandatory");
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Adversarial replacement in the same kernel tick: a preexisting Pi
        // has this very PID and start tick. The server's strict post-launch
        // cutoff must exclude it even with a new Active generation.
        let next_floor = birth.start_ticks.checked_add(1).unwrap();
        assert!(next_floor > birth.start_ticks);
        store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: terminal_id.to_string(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 3,
                },
            )
            .unwrap();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_managed_agent_generation(2);
        app.managed_pi_launches.insert(
            terminal_id.clone(),
            crate::app::agents::ManagedPiLaunch {
                generation: 2,
                session_path: session.display().to_string(),
                earliest_birth_ticks: next_floor,
                process: None,
            },
        );
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: Agent::Pi,
            process_generation: 2,
            observed_at: std::time::Instant::now(),
        });
        assert!(app
            .managed_pi_launches
            .get(&terminal_id)
            .unwrap()
            .process
            .is_none());
        assert!(
            get(&mut app).is_none(),
            "old process cannot bind replacement generation"
        );
        // A newly restored server can carry the same pane and Active authority,
        // but not the ephemeral server-owned launch provenance.
        let mut restored = app_with_agent();
        restored.sender_authority_dir = directory.clone();
        std::mem::swap(&mut restored.state, &mut app.state);
        std::mem::swap(
            &mut restored.mailbox_bootstrap_test_foreground_jobs,
            &mut app.mailbox_bootstrap_test_foreground_jobs,
        );
        assert!(restored.managed_pi_launches.is_empty());
        assert!(
            get(&mut restored).is_none(),
            "restore cannot resurrect a managed launch"
        );
        drop(child);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_get_trusts_exact_managed_pi_session_paths_and_revokes_stale_executions() {
        use std::os::unix::fs::PermissionsExt;
        let mut app = app_with_agent();
        app.state.workspaces.push(Workspace::test_new("child"));
        app.state.ensure_test_terminals();
        let directory = std::env::temp_dir().join(format!(
            "herdr-pi-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        app.sender_authority_dir = directory.clone();
        let mut identities = Vec::new();
        for (ws_idx, name) in [(0, "parent"), (1, "child")] {
            let pane = app.state.workspaces[ws_idx].tabs[0].root_pane.unwrap();
            let terminal_id = app.state.workspaces[ws_idx]
                .terminal_id(pane)
                .unwrap()
                .clone();
            let sender = terminal_id.to_string();
            let session = directory.join(format!("{name}.jsonl"));
            std::fs::write(
                &session,
                format!(
                    "{{\"type\":\"session\",\"version\":3,\"id\":\"{name}\",\"cwd\":\"/tmp\"}}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).unwrap();
            let store =
                crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
                    .unwrap();
            store
                .cas(
                    None,
                    crate::sender_authority::SenderAuthorityRecord {
                        sender_key: sender.clone(),
                        process_generation: 1,
                        phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                        transition_revision: 1,
                    },
                )
                .unwrap();
            app.state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .begin_managed_agent(
                    name.into(),
                    Agent::Pi,
                    std::time::Instant::now(),
                    Duration::from_secs(3),
                    Duration::from_secs(30),
                );
            app.state
                .terminals
                .get_mut(&terminal_id)
                .unwrap()
                .set_managed_agent_generation(1);
            let job = |session: &std::path::Path| crate::platform::ForegroundJob {
                process_group_id: std::process::id(),
                processes: vec![crate::platform::ForegroundProcess {
                    pid: std::process::id(),
                    name: "node".into(),
                    argv0: None,
                    argv: Some(vec![
                        "node".into(),
                        "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                        "--session".into(),
                        session.display().to_string(),
                    ]),
                    cmdline: None,
                }],
            };
            app.install_mailbox_bootstrap_test_foreground_job(terminal_id.clone(), job(&session));
            let birth = crate::platform::process_birth_identity(std::process::id()).unwrap();
            app.managed_pi_launches.insert(
                terminal_id.clone(),
                crate::app::agents::ManagedPiLaunch {
                    generation: 1,
                    session_path: session.display().to_string(),
                    earliest_birth_ticks: birth.start_ticks,
                    process: None,
                },
            );
            app.handle_internal_event(AppEvent::AgentProcessDetected {
                pane_id: pane,
                agent: Agent::Pi,
                process_generation: 1,
                observed_at: std::time::Instant::now(),
            });
            app.handle_internal_event(AppEvent::StateChanged {
                pane_id: pane,
                agent: Some(Agent::Pi),
                state: AgentState::Idle,
                visible_blocker: false,
                visible_working: false,
                process_exited: false,
                observed_at: std::time::Instant::now(),
            });
            let target = app.public_pane_id(ws_idx, pane).unwrap();
            let get = |app: &mut App| {
                let response = app.handle_agent_get(
                    "identity".into(),
                    AgentTarget {
                        target: target.clone(),
                    },
                );
                let result: SuccessResponse = serde_json::from_str(&response).unwrap();
                let ResponseResult::AgentInfo { agent } = result.result else {
                    panic!("agent get")
                };
                assert_eq!(agent.agent_status, AgentStatus::Idle);
                agent.agent_session
            };
            let identity = get(&mut app).expect("current idle Pi session identity");
            assert_eq!(identity.source, "herdr:pi");
            assert_eq!(
                identity.kind,
                crate::agent_resume::AgentSessionRefKind::Path
            );
            assert_eq!(identity.value, session.display().to_string());
            identities.push((pane, terminal_id, sender, session, target));
        }
        let (child_pane, child_terminal, child_key, child_session, child_target) =
            identities[1].clone();
        let get_child = |app: &mut App| {
            let response = app.handle_agent_get(
                "child".into(),
                AgentTarget {
                    target: child_target.clone(),
                },
            );
            let result: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::AgentInfo { agent } = result.result else {
                panic!("agent get")
            };
            agent.agent_session
        };
        assert_ne!(
            identities[0].3, identities[1].3,
            "two idle panes remain distinct"
        );
        let forged = directory.join("forged.jsonl");
        std::fs::write(&forged, b"{\"type\":\"session\",\"version\":3}\n").unwrap();
        std::fs::set_permissions(&forged, std::fs::Permissions::from_mode(0o600)).unwrap();
        let wrong_job = crate::platform::ForegroundJob {
            process_group_id: std::process::id(),
            processes: vec![crate::platform::ForegroundProcess {
                pid: std::process::id(),
                name: "node".into(),
                argv0: None,
                argv: Some(vec![
                    "node".into(),
                    "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    "--session".into(),
                    forged.display().to_string(),
                    "--session".into(),
                    child_session.display().to_string(),
                ]),
                cmdline: None,
            }],
        };
        app.install_mailbox_bootstrap_test_foreground_job(child_terminal.clone(), wrong_job);
        assert!(
            get_child(&mut app).is_none(),
            "ambiguous argv must not claim identity"
        );
        let wrong_process = crate::platform::ForegroundJob {
            process_group_id: std::process::id(),
            processes: vec![
                crate::platform::ForegroundProcess {
                    pid: std::process::id(),
                    name: "node".into(),
                    argv0: None,
                    argv: Some(vec![
                        "node".into(),
                        "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    ]),
                    cmdline: None,
                },
                crate::platform::ForegroundProcess {
                    pid: std::process::id() + 1,
                    name: "sh".into(),
                    argv0: None,
                    argv: Some(vec![
                        "sh".into(),
                        "--session".into(),
                        child_session.display().to_string(),
                    ]),
                    cmdline: None,
                },
            ],
        };
        app.install_mailbox_bootstrap_test_foreground_job(child_terminal.clone(), wrong_process);
        assert_eq!(
            get_child(&mut app).unwrap().value,
            child_session.display().to_string(),
            "another process's argv cannot replace the bound Pi launch session"
        );
        let link = directory.join("link.jsonl");
        std::os::unix::fs::symlink(&child_session, &link).unwrap();
        let job_for = |path: &std::path::Path| crate::platform::ForegroundJob {
            process_group_id: std::process::id(),
            processes: vec![crate::platform::ForegroundProcess {
                pid: std::process::id(),
                name: "node".into(),
                argv0: None,
                argv: Some(vec![
                    "node".into(),
                    "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    "--session".into(),
                    path.display().to_string(),
                ]),
                cmdline: None,
            }],
        };
        app.install_mailbox_bootstrap_test_foreground_job(child_terminal.clone(), job_for(&link));
        app.managed_pi_launches
            .get_mut(&child_terminal)
            .unwrap()
            .session_path = link.display().to_string();
        assert!(
            get_child(&mut app).is_none(),
            "even a matching server launch cannot validate a symlink path"
        );
        app.managed_pi_launches
            .get_mut(&child_terminal)
            .unwrap()
            .session_path = child_session.display().to_string();
        app.install_mailbox_bootstrap_test_foreground_job(
            child_terminal.clone(),
            job_for(&child_session),
        );
        std::fs::set_permissions(&child_session, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            get_child(&mut app).is_none(),
            "world-readable session is not trusted"
        );
        std::fs::set_permissions(&child_session, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            get_child(&mut app).unwrap().value,
            child_session.display().to_string()
        );
        let child_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &child_key)
                .unwrap();
        child_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: child_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        assert!(
            get_child(&mut app).is_none(),
            "a newer Active record cannot authorize an old terminal generation"
        );
        child_store
            .cas(
                Some(3),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: child_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 4,
                },
            )
            .unwrap();
        assert!(get_child(&mut app).is_none(), "preparing is not authority");
        let next = directory.join("child-next.jsonl");
        std::fs::write(
            &next,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"next\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o600)).unwrap();
        app.state
            .terminals
            .get_mut(&child_terminal)
            .unwrap()
            .set_managed_agent_generation(2);
        app.install_mailbox_bootstrap_test_foreground_job(child_terminal.clone(), job_for(&next));
        app.managed_pi_launches.insert(
            child_terminal.clone(),
            crate::app::agents::ManagedPiLaunch {
                generation: 2,
                session_path: next.display().to_string(),
                earliest_birth_ticks: crate::platform::process_birth_identity(std::process::id())
                    .unwrap()
                    .start_ticks,
                process: None,
            },
        );
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id: child_pane,
            agent: Agent::Pi,
            process_generation: 2,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(
            get_child(&mut app).unwrap().value,
            next.display().to_string()
        );
        child_store.invalidate_active(&child_key, 2).unwrap();
        assert!(
            get_child(&mut app).is_none(),
            "stopped execution cannot retain identity"
        );
        let stopped = app.state.terminals.get_mut(&child_terminal).unwrap();
        stopped.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::path(next.display().to_string())
                .unwrap(),
        });
        stopped.clear_agent_name();
        // #173 (Option A): while the pane still runs Pi, agent.get shows the
        // pane-reported session as 0.8.4 did, but only as `reported`; the
        // revoked execution never regains trusted identity or authority.
        assert!(app
            .trusted_managed_pi_session(app.state.terminals.get(&child_terminal).unwrap())
            .is_none());
        let response = app.handle_agent_get(
            "child".into(),
            AgentTarget {
                target: child_target.clone(),
            },
        );
        let result: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = result.result else {
            panic!("agent get")
        };
        assert_eq!(
            agent.agent_session_trust,
            Some(crate::api::schema::AgentSessionTrust::Reported),
            "a revoked managed execution is never shown as managed"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    // #173: the trusted managed launch always wins over a pane report; a live
    // Pi without trust shows its reported session (as 0.8.4 did), marked
    // `reported`; a stopped managed Pi shows neither.
    #[cfg(target_os = "linux")]
    #[test]
    fn trusted_launch_wins_over_report_and_live_untrusted_pi_falls_back_to_reported() {
        use crate::api::schema::{AgentSessionTrust, PaneReportAgentSessionParams};
        use std::os::unix::fs::PermissionsExt;
        let mut app = app_with_agent();
        let directory = std::env::temp_dir().join(format!(
            "herdr-pi-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        app.sender_authority_dir = directory.clone();
        let pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        let sender = terminal_id.to_string();
        let session = directory.join("managed.jsonl");
        std::fs::write(
            &session,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"managed\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&session, std::fs::Permissions::from_mode(0o600)).unwrap();
        crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .unwrap()
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.begin_managed_agent(
            "managed".into(),
            Agent::Pi,
            std::time::Instant::now(),
            Duration::from_secs(3),
            Duration::from_secs(30),
        );
        terminal.set_managed_agent_generation(1);
        app.install_mailbox_bootstrap_test_foreground_job(
            terminal_id.clone(),
            crate::platform::ForegroundJob {
                process_group_id: std::process::id(),
                processes: vec![crate::platform::ForegroundProcess {
                    pid: std::process::id(),
                    name: "node".into(),
                    argv0: None,
                    argv: Some(vec![
                        "node".into(),
                        "/opt/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                        "--session".into(),
                        session.display().to_string(),
                    ]),
                    cmdline: None,
                }],
            },
        );
        let birth = crate::platform::process_birth_identity(std::process::id()).unwrap();
        app.managed_pi_launches.insert(
            terminal_id.clone(),
            crate::app::agents::ManagedPiLaunch {
                generation: 1,
                session_path: session.display().to_string(),
                earliest_birth_ticks: birth.start_ticks,
                process: None,
            },
        );
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: pane,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        let target = app.public_pane_id(0, pane).unwrap();
        // The Pi's own herdr-agent-state integration reports a different path.
        let reported_path = directory.join("reported.jsonl").display().to_string();
        app.handle_pane_report_agent_session(
            "report".into(),
            PaneReportAgentSessionParams {
                pane_id: target.clone(),
                source: "herdr:pi".into(),
                agent: "pi".into(),
                seq: Some(1),
                agent_session_id: None,
                agent_session_path: Some(reported_path.clone()),
                session_start_source: Some("startup".into()),
            },
        );
        let get = |app: &mut App| {
            let response = app.handle_agent_get(
                "trust".into(),
                AgentTarget {
                    target: target.clone(),
                },
            );
            // A pane whose agent has exited may no longer resolve as an agent.
            let Ok(result) = serde_json::from_str::<SuccessResponse>(&response) else {
                return (None, None);
            };
            let ResponseResult::AgentInfo { agent } = result.result else {
                panic!("agent get")
            };
            (
                agent.agent_session.map(|s| s.value),
                agent.agent_session_trust,
            )
        };
        assert_eq!(
            get(&mut app),
            (
                Some(session.display().to_string()),
                Some(AgentSessionTrust::Managed)
            ),
            "a trusted managed launch wins over any pane report"
        );
        // Trust lost (e.g. restored server) while the same Pi stays live: the
        // identity falls back to the report and is marked as such.
        app.managed_pi_launches.remove(&terminal_id);
        assert!(app
            .trusted_managed_pi_session(app.state.terminals.get(&terminal_id).unwrap())
            .is_none());
        assert_eq!(
            get(&mut app),
            (Some(reported_path), Some(AgentSessionTrust::Reported))
        );
        // A stopped managed Pi never falls back to a stale report.
        app.handle_internal_event(AppEvent::StateChanged {
            pane_id: pane,
            agent: None,
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(get(&mut app), (None, None));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn managed_start_commits_only_one_valid_selector_after_successful_input() {
        let path = std::env::temp_dir()
            .join("herdr-managed-commit.jsonl")
            .display()
            .to_string();
        for (args, close_input, expect_committed) in [
            (vec!["--session".into(), path.clone()], false, true),
            (
                vec![
                    "--session".into(),
                    path.clone(),
                    "--session".into(),
                    "/tmp/forged.jsonl".into(),
                ],
                false,
                false,
            ),
            (vec!["--session".into(), path.clone()], true, false),
        ] {
            let mut app = app_with_agent();
            let pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
            let terminal = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
            let authority_dir = std::env::temp_dir().join(format!(
                "herdr-managed-commit-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            app.sender_authority_dir = authority_dir.clone();
            let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
            app.terminal_runtimes.insert(terminal.clone(), runtime);
            if close_input {
                drop(input);
            }
            let response = app.handle_agent_start(
                "start".into(),
                crate::api::schema::AgentStartParams {
                    name: "owner".into(),
                    kind: "pi".into(),
                    pane_id: app.public_pane_id(0, pane).unwrap(),
                    args,
                    env: Vec::new(),
                    timeout_ms: None,
                },
            );
            assert_eq!(
                serde_json::from_str::<SuccessResponse>(&response).is_ok(),
                !close_input
            );
            let committed = app.managed_pi_launches.get(&terminal);
            assert_eq!(committed.is_some(), expect_committed);
            if let Some(launch) = committed {
                assert_eq!(launch.session_path, path);
                assert_eq!(launch.generation, 1);
                assert!(
                    launch.process.is_none(),
                    "birth must bind only after Active observation"
                );
            }
            std::fs::remove_dir_all(authority_dir).unwrap();
        }
    }

    #[tokio::test]
    async fn headless_published_discovery_is_injected_before_pi_extension_initialization() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal id")
            .clone();
        let authority_dir = std::env::temp_dir().join(format!(
            "herdr-agent-start-bootstrap-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        app.sender_authority_dir = authority_dir.clone();
        // No listener means explicit unavailable discovery, even when a caller
        // attempts to provide its own socket location.
        assert_eq!(
            app.pi_mailbox_bootstrap_launch_environment(&[
                "HERDR_MAILBOX_BOOTSTRAP_ADDRESS=/tmp/attacker.sock".into(),
            ]),
            vec!["HERDR_MAILBOX_BOOTSTRAP_ADDRESS="]
        );
        // This is the exact Headless startup seam: bind the owned listener,
        // publish its address, then form the Pi shell command. Pi extension
        // initialization occurs only after that shell command starts Pi.
        let listener_path = authority_dir.join("mailbox-bootstrap.sock");
        let listener = crate::server::mailbox_bootstrap::MailboxBootstrapListener::bind_at(
            listener_path.clone(),
        )
        .expect("bind owned mailbox bootstrap listener");
        crate::server::mailbox_bootstrap::publish_owned_mailbox_bootstrap_discovery(
            &mut app, &listener,
        );
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id, runtime);

        let response = app.handle_agent_start(
            "start".into(),
            crate::api::schema::AgentStartParams {
                name: "reviewer".into(),
                kind: "pi".into(),
                pane_id: app.public_pane_id(0, pane_id).expect("public pane"),
                args: Vec::new(),
                // A client cannot replace host discovery with an attacker path.
                env: vec!["HERDR_MAILBOX_BOOTSTRAP_ADDRESS=/tmp/attacker.sock".into()],
                timeout_ms: None,
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        let command = String::from_utf8(
            input
                .try_recv()
                .expect("Pi command follows host-owned discovery injection")
                .to_vec(),
        )
        .expect("shell command is UTF-8");
        assert!(command.contains(&format!(
            "HERDR_MAILBOX_BOOTSTRAP_ADDRESS={} pi",
            listener_path.display()
        )));
        assert!(!command.contains("/tmp/attacker.sock"));
        drop(listener);
        std::fs::remove_dir_all(authority_dir).expect("remove authority directory");
    }

    #[tokio::test]
    async fn agent_start_persists_generation_before_input_and_rejects_stale_detection() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal id")
            .clone();
        let authority_dir = std::env::temp_dir().join(format!(
            "herdr-agent-start-authority-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        app.sender_authority_dir = authority_dir.clone();
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);

        let response = app.handle_agent_start(
            "start".into(),
            crate::api::schema::AgentStartParams {
                name: "reviewer".into(),
                kind: "pi".into(),
                pane_id: app.public_pane_id(0, pane_id).expect("public pane"),
                args: Vec::new(),
                env: Vec::new(),
                timeout_ms: None,
            },
        );
        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        assert!(input.try_recv().is_ok(), "input follows durable intent");

        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &authority_dir,
            &terminal_id.to_string(),
        )
        .expect("open authority store");
        assert_eq!(
            store.load().expect("read authority record"),
            Some(crate::sender_authority::SenderAuthorityRecord {
                sender_key: terminal_id.to_string(),
                process_generation: 1,
                phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                transition_revision: 1,
            })
        );

        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Codex,
            process_generation: 2,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(
            app.state.terminals[&terminal_id].detected_agent, None,
            "a replaced generation cannot claim the managed launch"
        );

        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        assert_eq!(
            app.state.terminals[&terminal_id].detected_agent,
            Some(Agent::Pi)
        );

        std::fs::remove_dir_all(authority_dir).expect("remove authority test directory");
    }

    #[tokio::test]
    async fn agent_start_aborts_before_input_when_authority_persistence_fails() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("root pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("terminal id")
            .clone();
        let authority_path = std::env::temp_dir().join(format!(
            "herdr-agent-start-authority-file-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        std::fs::write(&authority_path, b"not a directory").expect("create authority blocker");
        app.sender_authority_dir = authority_path.clone();
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal_id.clone(), runtime);

        let response = app.handle_agent_start(
            "start".into(),
            crate::api::schema::AgentStartParams {
                name: "reviewer".into(),
                kind: "pi".into(),
                pane_id: app.public_pane_id(0, pane_id).expect("public pane"),
                args: Vec::new(),
                env: Vec::new(),
                timeout_ms: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_start_authority_persistence_failed");
        assert!(
            input.try_recv().is_err(),
            "persistence failure must precede input"
        );
        assert!(!app.state.terminals[&terminal_id].is_agent_terminal());

        std::fs::remove_file(authority_path).expect("remove authority blocker");
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
                send: Default::default(),
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
                expected_terminal_id: None,
                expected_name: None,
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
                expected_terminal_id: None,
                expected_name: None,
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
                send: Default::default(),
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
                send: Default::default(),
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
                send: Default::default(),
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
                    send: Default::default(),
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
                send: Default::default(),
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
                send: Default::default(),
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
                send: Default::default(),
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

        for (request, expected_terminal_id, expected_name) in [
            (
                "wrong-terminal",
                Some("term_wrong".into()),
                Some("reviewer".into()),
            ),
            (
                "wrong-generation",
                Some(terminal_id.to_string()),
                Some("older-generation".into()),
            ),
        ] {
            let rejected = app.handle_agent_send_keys(
                request.into(),
                AgentSendKeysParams {
                    target: "reviewer".into(),
                    keys: vec!["ctrl+d".into()],
                    expected_terminal_id,
                    expected_name,
                },
            );
            let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
            assert_eq!(error.error.code, "agent_identity_mismatch");
            assert!(rx.try_recv().is_err());
        }

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
                expected_terminal_id: None,
                expected_name: None,
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
                expected_terminal_id: Some(terminal_id.to_string()),
                expected_name: Some("reviewer".into()),
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
                send: Default::default(),
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
