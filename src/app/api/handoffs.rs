use std::time::Duration;

use bytes::Bytes;

use crate::api::schema::{
    AgentInfo, AgentStatus, CanonicalHerdrIdentity, HandoffSendParams, HandoffTransportOutcome,
    HandoffTransportReceipt, HandoffValidateParams, ResponseResult, HANDOFF_VERSION,
};
use crate::app::App;

use super::responses::{encode_error, encode_success};

const PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);
const ATTEMPTED_PATH: &str = "pi_prompt_via_pty";

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
