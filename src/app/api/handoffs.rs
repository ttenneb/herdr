use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentInfo, AgentStatus, CanonicalHerdrIdentity, HandoffSendParams, HandoffTransportOutcome,
    HandoffTransportReceipt, HandoffValidateParams, ResponseResult, HANDOFF_VERSION,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

const PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);
const ATTEMPTED_PATH: &str = "agent_prompt_via_pty";

impl App {
    pub(super) fn handle_handoff_validate(
        &mut self,
        id: String,
        params: HandoffValidateParams,
    ) -> String {
        if let Err(err) = params.envelope.validate() {
            return encode_error(id, "invalid_handoff", err.to_string());
        }
        let encoded_bytes = serde_json::to_vec(&params.envelope)
            .expect("validated handoff serializes")
            .len();
        encode_success(
            id,
            ResponseResult::HandoffValidated {
                version: HANDOFF_VERSION,
                message_id: params.envelope.message_id,
                encoded_bytes,
            },
        )
    }

    pub(super) fn handle_handoff_send(&mut self, id: String, params: HandoffSendParams) -> String {
        let envelope = params.envelope;
        if let Err(err) = envelope.validate() {
            return encode_error(id, "invalid_handoff", err.to_string());
        }

        let recipient_info = self.current_identity_info(&envelope.recipient);
        let target_status = recipient_info
            .as_ref()
            .map_or(AgentStatus::Unknown, |agent| agent.agent_status);
        let target_interactive_ready = recipient_info
            .as_ref()
            .is_some_and(|agent| agent.interactive_ready);
        let target_state_change_seq = recipient_info
            .as_ref()
            .map_or(0, |agent| agent.state_change_seq);
        let receipt = |outcome, detail: String| HandoffTransportReceipt {
            version: HANDOFF_VERSION,
            message_id: envelope.message_id.clone(),
            sender: envelope.sender.clone(),
            recipient: envelope.recipient.clone(),
            attempted_path: ATTEMPTED_PATH.into(),
            target_status,
            target_interactive_ready,
            target_state_change_seq,
            outcome,
            detail,
            delivery: None,
        };

        if !self.identity_matches_current(&envelope.sender) {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::SenderIdentityMismatch,
                        "sender no longer matches the current pane agent session".into(),
                    ),
                },
            );
        }
        // A recipient Herdr put to sleep is not refused: the handoff is queued
        // in its Messages and the queued head wakes the pane.
        if let Some(terminal_id) = self.sleeping_recipient_terminal(&envelope.recipient) {
            let asleep = || {
                encode_success(
                    id.clone(),
                    ResponseResult::HandoffTransport {
                        receipt: receipt(
                            HandoffTransportOutcome::RecipientNotReady,
                            "recipient is asleep; only a Messages handoff can reach it".into(),
                        ),
                    },
                )
            };
            if params.send.transport == Some(crate::api::schema::MessageTransport::Pty) {
                return asleep();
            }
            let Some(response) = self.handoff_via_messages(&id, &envelope, &params.send, &receipt)
            else {
                return asleep();
            };
            let stable_id = serde_json::from_str::<serde_json::Value>(&response)
                .ok()
                .and_then(|value| {
                    value["result"]["receipt"]["delivery"]["stable_id"]
                        .as_str()
                        .map(str::to_string)
                });
            if let Some(stable_id) = stable_id {
                self.request_pane_wake_if_detached(&terminal_id.to_string(), &stable_id);
            }
            return response;
        }
        if recipient_info
            .as_ref()
            .is_none_or(|agent| !identity_matches(agent, &envelope.recipient))
        {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientIdentityMismatch,
                        "recipient no longer matches the current pane agent session".into(),
                    ),
                },
            );
        }

        let Some((ws_idx, pane_id)) =
            self.parse_current_public_pane_id(&envelope.recipient.pane_id)
        else {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientIdentityMismatch,
                        "recipient pane is no longer present".into(),
                    ),
                },
            );
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
        else {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientIdentityMismatch,
                        "recipient pane is no longer attached to a terminal".into(),
                    ),
                },
            );
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientIdentityMismatch,
                        "recipient terminal is no longer present".into(),
                    ),
                },
            );
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientBlocked,
                        "recipient requires interactive input".into(),
                    ),
                },
            );
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientNotReady,
                        "recipient agent is not ready".into(),
                    ),
                },
            );
        };
        if terminal.managed_agent_launch_pending() {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientNotReady,
                        "recipient managed-agent launch is still pending".into(),
                    ),
                },
            );
        }
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::TransportClosed,
                        "recipient runtime is unavailable".into(),
                    ),
                },
            );
        };
        if !crate::app::agents::runtime_hosts_agent(runtime, expected_agent) {
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(
                        HandoffTransportOutcome::RecipientNotForeground,
                        "recipient agent is no longer the pane foreground process".into(),
                    ),
                },
            );
        }
        if expected_agent == crate::detect::Agent::Pi
            && params.send.transport != Some(crate::api::schema::MessageTransport::Pty)
            && self.pane_takes_messages(&terminal_id.to_string())
        {
            if let Some(response) =
                self.handoff_via_messages(&id, &envelope, &params.send, &receipt)
            {
                if let Some(stable_id) = serde_json::from_str::<serde_json::Value>(&response)
                    .ok()
                    .and_then(|value| {
                        value["result"]["receipt"]["delivery"]["stable_id"]
                            .as_str()
                            .map(str::to_string)
                    })
                {
                    self.request_pane_wake_if_detached(&terminal_id.to_string(), &stable_id);
                }
                if let Some(restore) = self.begin_archived_member_input(ws_idx, pane_id) {
                    self.commit_archived_member_input(restore);
                }
                return response;
            }
        } else if params.send.transport == Some(crate::api::schema::MessageTransport::Mailbox) {
            return encode_error(
                id,
                "messages_unavailable",
                "the recipient has no live Messages connection",
            );
        }
        if expected_agent == crate::detect::Agent::GithubCopilot {
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return encode_success(
                        id,
                        ResponseResult::HandoffTransport {
                            receipt: receipt(
                                HandoffTransportOutcome::TransportClosed,
                                err.to_string(),
                            ),
                        },
                    )
                }
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return encode_success(
                    id,
                    ResponseResult::HandoffTransport {
                        receipt: receipt(HandoffTransportOutcome::TransportClosed, err.to_string()),
                    },
                );
            }
        }

        if envelope.kind == crate::api::schema::HandoffKind::Report {
            if let Some(route) = self.legacy_child_parent_report_identity(
                &envelope.sender.terminal_id,
                &envelope.recipient.terminal_id,
            ) {
                if route.child_session == envelope.sender.agent_session
                    && route.parent_session == envelope.recipient.agent_session
                    && route.child_pane_id == envelope.sender.pane_id
                    && route.parent_pane_id == envelope.recipient.pane_id
                {
                    let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir);
                    if store
                        .and_then(|store| {
                            store.append_child_report_event(
                                crate::child_report::ChildReportEvent::Bypass {
                                    route,
                                    path: crate::child_report::ReportBypassPath::HandoffPty,
                                    message_id: envelope.message_id.clone(),
                                },
                            )
                        })
                        .is_err()
                    {
                        return encode_error(
                            id,
                            "report_visibility_failed",
                            "legacy report path could not be recorded before delivery",
                        );
                    }
                }
            }
        }
        let prompt = envelope.prompt_text();
        let (text, enter) = crate::app::api_helpers::encode_api_submission_parts(runtime, &prompt);
        let result = runtime.try_send_prompt_transaction(
            Bytes::from(text),
            Bytes::from(enter),
            PROMPT_SUBMIT_DELAY,
        );
        if let Err(err) = result {
            let outcome = match err {
                crate::pane::PromptTransactionAdmissionError::Full => {
                    HandoffTransportOutcome::QueueFull
                }
                crate::pane::PromptTransactionAdmissionError::PayloadTooLarge => {
                    HandoffTransportOutcome::PayloadTooLarge
                }
                crate::pane::PromptTransactionAdmissionError::InputFull
                | crate::pane::PromptTransactionAdmissionError::Closed => {
                    HandoffTransportOutcome::TransportClosed
                }
            };
            return encode_success(
                id,
                ResponseResult::HandoffTransport {
                    receipt: receipt(outcome, err.to_string()),
                },
            );
        }

        if let Some(restore) = self.begin_archived_member_input(ws_idx, pane_id) {
            self.commit_archived_member_input(restore);
        }
        self.acknowledge_terminal_input(&terminal_id);
        encode_success(id, ResponseResult::HandoffTransport {
            receipt: receipt(HandoffTransportOutcome::RuntimeTransactionAdmitted, "Herdr runtime admitted the complete prompt transaction; Pi/gate/agent acknowledgement is unknown".into()),
        })
    }

    /// Queues a validated handoff in the recipient's Messages. Returns `None`
    /// to continue on the unchanged PTY path (for example, text outside
    /// Messages limits). The legacy report-bypass journaling above runs first.
    fn handoff_via_messages(
        &self,
        id: &str,
        envelope: &crate::api::schema::HerdrHandoff,
        options: &crate::api::schema::MessageSendOptions,
        receipt: &dyn Fn(HandoffTransportOutcome, String) -> HandoffTransportReceipt,
    ) -> Option<String> {
        let sender_label = self
            .current_identity_info(&envelope.sender)
            .and_then(|agent| agent.name)
            .unwrap_or_else(|| envelope.sender.pane_id.clone());
        let sender = crate::app::messages::SenderAttribution {
            terminal: Some(envelope.sender.terminal_id.clone()),
            label: sender_label.clone(),
            session: Some(envelope.sender.agent_session.value.clone()),
        };
        let kind = match envelope.kind {
            crate::api::schema::HandoffKind::Assignment => "assignment",
            crate::api::schema::HandoffKind::Blocker => "blocker",
            _ => "advisory",
        };
        let summary_line = envelope.summary.lines().next().unwrap_or_default();
        let message = crate::app::messages::OutgoingMessage {
            origin: "handoff",
            subject: crate::app::messages::subject_for(
                &format!("Handoff ({kind}) from"),
                &format!("{sender_label}: {summary_line}"),
            ),
            body: envelope.prompt_text(),
            priority: "normal".into(),
            kind: kind.into(),
            message_id: Some(format!("handoff:{}", envelope.message_id)),
            correlation: envelope.correlation_id.as_ref().map(|key| {
                crate::mailbox::SendCorrelation {
                    namespace: "handoff".into(),
                    key: key.clone(),
                    revision: 1,
                }
            }),
            replace_pending: false,
        };
        let options = crate::api::schema::MessageSendOptions {
            transport: Some(crate::api::schema::MessageTransport::Mailbox),
            ..options.clone()
        };
        let recipient_terminal = envelope.recipient.terminal_id.clone();
        match self.route_ordinary_send(&recipient_terminal, &sender, message, &options) {
            Ok(crate::app::messages::SendRoute::Mailbox(delivery)) => {
                let mut receipt = receipt(
                    HandoffTransportOutcome::MailboxAdmitted,
                    "queued durably in the recipient's Messages; the recipient runs it when it next picks up work".into(),
                );
                receipt.delivery = Some(delivery);
                Some(encode_success(
                    id.to_string(),
                    ResponseResult::HandoffTransport { receipt },
                ))
            }
            Ok(crate::app::messages::SendRoute::Pty) => None,
            Err(crate::app::messages::SendRefusal::MailboxUnavailable(_)) => None,
            Err(refusal) => crate::app::messages::pending_error_json(id.to_string(), &refusal)
                .or_else(|| match refusal {
                    crate::app::messages::SendRefusal::Store(message) => Some(encode_error(
                        id.to_string(),
                        "mailbox_store_failed",
                        message,
                    )),
                    _ => None,
                }),
        }
    }

    fn current_identity_info(&self, identity: &CanonicalHerdrIdentity) -> Option<AgentInfo> {
        let (ws_idx, pane_id) = self.parse_current_public_pane_id(&identity.pane_id)?;
        self.agent_info(ws_idx, pane_id)
    }

    fn identity_matches_current(&self, identity: &CanonicalHerdrIdentity) -> bool {
        self.current_identity_info(identity)
            .as_ref()
            .is_some_and(|agent| identity_matches(agent, identity))
    }
}

fn identity_matches(agent: &AgentInfo, identity: &CanonicalHerdrIdentity) -> bool {
    agent.workspace_id == identity.workspace_id
        && agent.pane_id == identity.pane_id
        && agent.terminal_id == identity.terminal_id
        && agent.agent_session.as_ref() == Some(&identity.agent_session)
}
