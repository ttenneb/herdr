use crate::api::schema::{MailboxOfflineSubmitParams, ResponseResult, SuccessResponse};
use crate::app::{App, MailboxBootstrapError, MailboxBootstrapSession};
use serde_json::Value;

use super::responses::{encode_error, encode_success};

impl App {
    /// Dispatches only the mailbox operations for an already verified,
    /// server-issued accepted-stream binding. Request payloads intentionally do
    /// not carry caller, grant, or recipient selectors; this method installs the
    /// descriptor scope after the exact Active generation recheck.
    pub(crate) fn dispatch_mailbox_bootstrap(
        &mut self,
        session: &MailboxBootstrapSession,
        method: &str,
        params: Value,
    ) -> Result<Value, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        let id = "mailbox-bootstrap".to_owned();
        let response = match method {
            "mailbox.offline_submit" | "report_submit" => {
                let submit: crate::mailbox_v1::Submit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                if method == "report_submit" && submit.kind != "report" {
                    return Err(MailboxBootstrapError::InvalidRequest);
                }
                self.handle_mailbox_offline_submit(
                    id,
                    MailboxOfflineSubmitParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        submit,
                    },
                )
            }
            "report_submit_parent" => {
                let submit: crate::mailbox_v1::Submit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                if submit.kind != "report" {
                    return Err(MailboxBootstrapError::InvalidRequest);
                }
                let route = self.bound_parent_report_current(session)?;
                self.handle_mailbox_server_scoped_submit(
                    id,
                    MailboxOfflineSubmitParams {
                        caller: session.caller.clone(),
                        grant_id: route.grant_id.clone(),
                        recipient: route.recipient.clone(),
                        submit,
                    },
                )
            }
            "mailbox.provision_recipient" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct ProvisionRecipientParams {
                    target: String,
                }
                let params: ProvisionRecipientParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                let grant = self.provision_mailbox_bootstrap_recipient(session, &params.target)?;
                encode_success(id, ResponseResult::MailboxGrantProvisioned { grant })
            }
            "mailbox.snapshot" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct SnapshotParams {
                    protocol: String,
                }
                let params: SnapshotParams = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_snapshot(
                    id,
                    crate::api::schema::MailboxSnapshotParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        protocol: params.protocol,
                    },
                )
            }
            "mailbox.claim" => {
                let claim = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_claim(
                    id,
                    crate::api::schema::MailboxClaimParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        claim,
                    },
                )
            }
            "mailbox.edit" => {
                let edit = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_edit(
                    id,
                    crate::api::schema::MailboxEditParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        edit,
                    },
                )
            }
            "mailbox.resolve" => {
                let resolve = serde_json::from_value(params)
                    .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
                self.handle_mailbox_resolve(
                    id,
                    crate::api::schema::MailboxResolveParams {
                        caller: session.caller.clone(),
                        grant_id: session.grant_id.clone(),
                        recipient: session.recipient.clone(),
                        resolve,
                    },
                )
            }
            _ => return Err(MailboxBootstrapError::InvalidRequest),
        };
        let success: SuccessResponse =
            serde_json::from_str(&response).map_err(|_| MailboxBootstrapError::InvalidRequest)?;
        serde_json::to_value(success.result).map_err(|_| MailboxBootstrapError::InvalidRequest)
    }

    pub(crate) fn handle_mailbox_offline_submit(
        &mut self,
        id: String,
        params: MailboxOfflineSubmitParams,
    ) -> String {
        // Durable bound-parent grants remain in old journals, but cannot be
        // exercised through the generic selector-bearing API, even after a
        // delegation reparent, parent replacement, or server restart. Older
        // `mailbox:` grants cannot be classified: typed and explicitly
        // provisioned grants previously used the identical durable ID.
        if params.grant_id.starts_with("bound-parent-report:") {
            return encode_error(
                id,
                "mailbox_capability_mismatch",
                "bound-parent report grants require their accepted stream",
            );
        }
        self.handle_mailbox_server_scoped_submit(id, params)
    }

    fn handle_mailbox_server_scoped_submit(
        &mut self,
        id: String,
        params: MailboxOfflineSubmitParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get_mut(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.submit(params) {
            Ok(receipt) => encode_success(id, ResponseResult::MailboxOfflineSubmitted { receipt }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Replay) => encode_error(
                id,
                "mailbox_replay_rejected",
                "delivery digest was already admitted for this sender generation",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(crate::app::mailbox::OfflineMailboxError::ReceiptMissing) => encode_error(
                id,
                "mailbox_receipt_missing",
                "server admission did not read back its durable receipt",
            ),
        }
    }

    pub(crate) fn handle_mailbox_claim(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxClaimParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.claim(params) {
            Ok(claim) => encode_success(id, ResponseResult::MailboxClaimed { claim }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(id, "mailbox_claim_failed", "offline mailbox claim rejected"),
        }
    }

    pub(crate) fn handle_mailbox_snapshot(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxSnapshotParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.snapshot(params) {
            Ok(snapshot) => encode_success(id, ResponseResult::MailboxSnapshot { snapshot }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(
                id,
                "mailbox_snapshot_failed",
                "offline mailbox snapshot rejected",
            ),
        }
    }

    pub(crate) fn handle_mailbox_edit(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxEditParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.edit(params) {
            Ok(snapshot) => encode_success(id, ResponseResult::MailboxEdited { snapshot }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => {
                encode_error(id, error.code(), "offline mailbox edit validation rejected")
            }
            Err(crate::app::mailbox::OfflineMailboxError::Store(
                crate::mailbox::MailboxError::EditConflict,
            )) => encode_error(
                id,
                "mailbox_edit_conflict",
                "stableId, revision, or digest no longer matches the authoritative head",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(
                crate::mailbox::MailboxError::HeadClaimed,
            )) => encode_error(
                id,
                "mailbox_edit_claimed",
                "the mailbox head is already claimed and immutable",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(id, "mailbox_edit_failed", "offline mailbox edit rejected"),
        }
    }

    pub(crate) fn handle_mailbox_resolve(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxResolveParams,
    ) -> String {
        match self.offline_mailbox_authority_current(&params.caller) {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                return encode_error(
                    id,
                    "mailbox_authority_unavailable",
                    "the server has no current authenticated local mailbox sender route",
                )
            }
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&params.caller)
            .expect("current route must remain installed during serialized dispatch");
        match authority.resolve(params) {
            Ok(resolution) => encode_success(id, ResponseResult::MailboxResolved { resolution }),
            Err(crate::app::mailbox::OfflineMailboxError::CallerMismatch) => encode_error(
                id,
                "mailbox_caller_mismatch",
                "caller selector does not match the authenticated local sender",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::CapabilityMismatch) => encode_error(
                id,
                "mailbox_capability_mismatch",
                "recipient or grant selector is outside the server-issued capability",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox request validation rejected",
            ),
            Err(crate::app::mailbox::OfflineMailboxError::Store(error)) => {
                encode_error(id, "mailbox_store_failed", error.to_string())
            }
            Err(_) => encode_error(
                id,
                "mailbox_resolve_failed",
                "offline mailbox resolve rejected",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::app::Mode;
    use crate::config::Config;
    use crate::detect::Agent;
    use crate::events::AppEvent;
    use crate::mailbox::RecipientKey;
    use crate::mailbox_v1::{ClaimRequest, Resolve, ResolveOutcome, Submit, PROTOCOL};
    use crate::workspace::Workspace;

    use super::*;

    fn sender_directory() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "herdr-active-offline-mailbox-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ))
    }

    fn submit(
        caller: String,
        grant_id: String,
        recipient: RecipientKey,
        delivery_digest: String,
    ) -> MailboxOfflineSubmitParams {
        MailboxOfflineSubmitParams {
            caller,
            grant_id,
            recipient,
            submit: Submit {
                protocol: PROTOCOL.into(),
                stable_id: "stable-1".into(),
                revision: 1,
                digest: "a".repeat(64),
                delivery_digest,
                subject: "offline subject".into(),
                body: "offline body".into(),
                message_id: "message-1".into(),
                kind: "report".into(),
                priority: "normal".into(),
                original_sequence: 1,
            },
        }
    }

    fn app_with_active_sender() -> (App, crate::layout::PaneId, String, std::path::PathBuf) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("sender")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("sender pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("sender terminal")
            .clone();
        let directory = sender_directory();
        app.sender_authority_dir = directory.clone();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &directory,
            &terminal_id.to_string(),
        )
        .expect("sender authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: terminal_id.to_string(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist preparing sender");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("terminal state");
        terminal.begin_managed_agent(
            "sender".into(),
            Agent::Pi,
            std::time::Instant::now(),
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(30),
        );
        terminal.set_managed_agent_generation(1);
        app.handle_internal_event(AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        (app, pane_id, terminal_id.to_string(), directory)
    }

    fn active_recipient(sender_key: &str) -> RecipientKey {
        RecipientKey {
            recipient_id: sender_key.into(),
            generation: "1".into(),
        }
    }

    fn active_submit(sender_key: String, delivery_digest: String) -> MailboxOfflineSubmitParams {
        submit(
            sender_key.clone(),
            format!("offline:{sender_key}:1"),
            active_recipient(&sender_key),
            delivery_digest,
        )
    }

    fn active_claim(sender_key: String) -> crate::api::schema::MailboxClaimParams {
        crate::api::schema::MailboxClaimParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            claim: ClaimRequest {
                protocol: PROTOCOL.into(),
            },
        }
    }

    fn mailbox_snapshot(
        caller: String,
        grant_id: String,
        recipient: RecipientKey,
    ) -> crate::api::schema::MailboxSnapshotParams {
        crate::api::schema::MailboxSnapshotParams {
            caller,
            grant_id,
            recipient,
            protocol: PROTOCOL.into(),
        }
    }

    fn active_edit(
        sender_key: String,
        revision: u64,
        digest: String,
        subject: &str,
        body: &str,
    ) -> crate::api::schema::MailboxEditParams {
        crate::api::schema::MailboxEditParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            edit: crate::mailbox_v1::Edit {
                protocol: PROTOCOL.into(),
                stable_id: "stable-1".into(),
                revision,
                digest,
                subject: subject.into(),
                body: body.into(),
            },
        }
    }

    fn active_resolve(
        sender_key: String,
        claim_id: String,
        outcome: ResolveOutcome,
    ) -> crate::api::schema::MailboxResolveParams {
        crate::api::schema::MailboxResolveParams {
            caller: sender_key.clone(),
            grant_id: format!("offline:{sender_key}:1"),
            recipient: active_recipient(&sender_key),
            resolve: Resolve {
                protocol: PROTOCOL.into(),
                claim_id,
                outcome,
            },
        }
    }

    #[test]
    fn mailbox_authority_promotes_active_and_installs_generation_bound_capability() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        assert_eq!(
            sender_store.load().expect("read sender authority"),
            Some(crate::sender_authority::SenderAuthorityRecord {
                sender_key: sender_key.clone(),
                process_generation: 1,
                phase: crate::sender_authority::SenderAuthorityPhase::Active,
                transition_revision: 2,
            })
        );
        let response = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "b".repeat(64))),
        });
        let success: SuccessResponse = serde_json::from_str(&response).expect("success response");
        assert!(matches!(
            success.result,
            ResponseResult::MailboxOfflineSubmitted { .. }
        ));
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_rejects_replay_and_replaced_generation() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let first = active_submit(sender_key.clone(), "b".repeat(64));
        let first_response = app.handle_api_request(Request {
            id: "first".into(),
            method: Method::MailboxOfflineSubmit(first.clone()),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&first_response).is_ok());
        let replay_response = app.handle_api_request(Request {
            id: "replay".into(),
            method: Method::MailboxOfflineSubmit(first),
        });
        let replay: ErrorResponse = serde_json::from_str(&replay_response).expect("replay error");
        assert_eq!(replay.error.code, "mailbox_replay_rejected");

        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace sender generation");
        let stale_response = app.handle_api_request(Request {
            id: "stale".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "c".repeat(64))),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale_response).expect("stale error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn stale_pane_died_after_replacement_keeps_current_authority_and_claim() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "c".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let claimed = app.handle_api_request(Request {
            id: "claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let claimed: SuccessResponse = serde_json::from_str(&claimed).expect("claim response");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = claimed.result else {
            panic!("expected durable claim")
        };

        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace active sender generation");
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal")
            .set_managed_agent_generation(2);
        app.install_offline_mailbox_authority(
            sender_store
                .load()
                .expect("read replacement")
                .expect("active sender"),
        )
        .expect("install replacement authority");

        let lifecycle_sequence = app.event_hub.current_sequence();
        let terminal = app
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal");
        terminal.set_detected_state(Some(Agent::Pi), crate::detect::AgentState::Working);
        terminal.respawn_shell_on_exit = true;

        // This event was queued by generation 1 before generation 2 became
        // Active; it must not revoke the replacement authority or affect the
        // live generation-2 pane lifecycle.
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            process_generation: Some(1),
        });

        assert!(
            app.find_pane(pane_id).is_some(),
            "stale exit must not close pane"
        );
        let terminal = app
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == sender_key)
            .expect("sender terminal");
        assert!(terminal.accepts_managed_agent_generation(2));
        assert_eq!(terminal.state, crate::detect::AgentState::Working);
        assert!(
            terminal.respawn_shell_on_exit,
            "stale exit must not consume respawn state"
        );
        let lifecycle_events = app.event_hub.events_after(lifecycle_sequence);
        assert!(!lifecycle_events
            .iter()
            .any(|(_, event)| matches!(event.event, crate::api::schema::EventKind::PaneExited)));
        assert!(!lifecycle_events.iter().any(|(_, event)| matches!(
            event.data,
            crate::api::schema::EventData::PaneAgentDetected { released: true, .. }
        )));
        assert!(app
            .offline_mailbox_authority_current(&sender_key)
            .expect("read current authority"));
        let replay = app.handle_api_request(Request {
            id: "claim-after-stale-exit".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: sender_key.clone(),
                grant_id: format!("offline:{sender_key}:2"),
                recipient: active_recipient(&sender_key),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let replay: SuccessResponse = serde_json::from_str(&replay).expect("replay claim response");
        assert_eq!(
            replay.result,
            ResponseResult::MailboxClaimed { claim: Some(claim) }
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_exit_invalidates_exact_active_generation() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        app.handle_internal_event(AppEvent::PaneDied {
            pane_id,
            process_generation: Some(1),
        });
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        assert_eq!(
            sender_store
                .load()
                .expect("read sender authority")
                .expect("sender record")
                .phase,
            crate::sender_authority::SenderAuthorityPhase::Invalidated
        );
        let response = app.handle_api_request(Request {
            id: "after-exit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key, "d".repeat(64))),
        });
        let error: ErrorResponse = serde_json::from_str(&response).expect("exit error");
        assert_eq!(error.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_edit_cas_returns_a_fsynced_authoritative_refreshed_head() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "e".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let edited = app.handle_api_request(Request {
            id: "edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key.clone(),
                1,
                "a".repeat(64),
                "edited subject",
                "edited body",
            )),
        });
        let edited: SuccessResponse = serde_json::from_str(&edited).expect("edit response");
        let ResponseResult::MailboxEdited { snapshot } = edited.result else {
            panic!("expected authoritative edit snapshot")
        };
        assert_eq!(snapshot.heads.len(), 1);
        let head = &snapshot.heads[0];
        assert_eq!(head.revision, 2);
        assert_ne!(head.digest, "a".repeat(64));
        assert_eq!(head.subject, "edited subject");
        assert_eq!(head.body, "edited body");
        assert_eq!(head.sender, sender_key);
        assert_eq!(head.target, sender_key);
        assert_eq!(head.grant_id, format!("offline:{sender_key}:1"));
        assert_eq!(head.message_id, "message-1");
        let durable = crate::mailbox::MailboxStore::open(&directory)
            .expect("open durable mailbox")
            .load()
            .expect("reload durable mailbox");
        assert_eq!(durable.heads["stable-1"], *head);
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_edit_rejects_stale_exact_version() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "1".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let first = app.handle_api_request(Request {
            id: "first-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key.clone(),
                1,
                "a".repeat(64),
                "subject two",
                "body two",
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&first).is_ok());
        let stale = app.handle_api_request(Request {
            id: "stale-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key,
                1,
                "a".repeat(64),
                "stale subject",
                "stale body",
            )),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale edit error");
        assert_eq!(stale.error.code, "mailbox_edit_conflict");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_edit_rejects_post_claim_head() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "2".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let claimed = app.handle_api_request(Request {
            id: "claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&claimed).is_ok());
        let rejected = app.handle_api_request(Request {
            id: "claimed-edit".into(),
            method: Method::MailboxEdit(active_edit(
                sender_key,
                1,
                "a".repeat(64),
                "late subject",
                "late body",
            )),
        });
        let rejected: ErrorResponse = serde_json::from_str(&rejected).expect("claimed edit error");
        assert_eq!(rejected.error.code, "mailbox_edit_claimed");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn admitted_high_claim_remains_outstanding_until_settled_before_normal_claim() {
        let (mut app, _pane_id, sender, directory) = app_with_active_sender();
        for (stable_id, priority, digest) in [
            ("a-high", "high", "a".repeat(64)),
            ("z-normal", "normal", "b".repeat(64)),
        ] {
            let mut submit = active_submit(sender.clone(), digest);
            submit.submit.stable_id = stable_id.into();
            submit.submit.priority = priority.into();
            let response = app.handle_api_request(Request {
                id: stable_id.into(),
                method: Method::MailboxOfflineSubmit(submit),
            });
            assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        }
        let claim = |app: &mut App, id: &str| {
            let response = app.handle_api_request(Request {
                id: id.into(),
                method: Method::MailboxClaim(active_claim(sender.clone())),
            });
            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::MailboxClaimed { claim: Some(claim) } = response.result else {
                panic!("expected claim")
            };
            claim
        };
        let high = claim(&mut app, "high");
        assert_eq!(high.stable_id, "a-high");
        let resolve = |app: &mut App, outcome| {
            app.handle_api_request(Request {
                id: "resolve".into(),
                method: Method::MailboxResolve(active_resolve(
                    sender.clone(),
                    high.claim_id.clone(),
                    outcome,
                )),
            })
        };
        let admitted = resolve(&mut app, ResolveOutcome::Admitted);
        assert!(serde_json::from_str::<SuccessResponse>(&admitted).is_ok());
        assert_eq!(claim(&mut app, "after-admitted"), high);
        let snapshot = |app: &mut App| {
            let response = app.handle_api_request(Request {
                id: "snapshot".into(),
                method: Method::MailboxSnapshot(mailbox_snapshot(
                    sender.clone(),
                    format!("offline:{sender}:1"),
                    active_recipient(&sender),
                )),
            });
            let response: SuccessResponse = serde_json::from_str(&response).unwrap();
            let ResponseResult::MailboxSnapshot { snapshot } = response.result else {
                panic!("expected snapshot")
            };
            snapshot.claim
        };
        assert_eq!(snapshot(&mut app), Some(high.clone()));
        let settled = resolve(&mut app, ResolveOutcome::Settled);
        assert!(
            serde_json::from_str::<SuccessResponse>(&settled).is_ok(),
            "{settled}"
        );
        assert_eq!(snapshot(&mut app), None);
        let normal = claim(&mut app, "after-settled");
        assert_eq!(normal.stable_id, "z-normal");
        assert_eq!(snapshot(&mut app), Some(normal.clone()));
        assert_eq!(claim(&mut app, "normal-replay"), normal);
        let backwards: ErrorResponse =
            serde_json::from_str(&resolve(&mut app, ResolveOutcome::Admitted)).unwrap();
        assert_eq!(backwards.error.code, "mailbox_store_failed");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_claim_replay_returns_one_durable_claim_and_resolve_is_idempotent() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "e".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let first = app.handle_api_request(Request {
            id: "claim-first".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let first: SuccessResponse = serde_json::from_str(&first).expect("claim response");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = first.result else {
            panic!("expected durable claim")
        };
        let replay = app.handle_api_request(Request {
            id: "claim-replay".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let replay: SuccessResponse = serde_json::from_str(&replay).expect("replay claim response");
        assert_eq!(
            replay.result,
            ResponseResult::MailboxClaimed {
                claim: Some(claim.clone())
            }
        );
        let resolved = app.handle_api_request(Request {
            id: "resolve".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key.clone(),
                claim.claim_id.clone(),
                ResolveOutcome::Settled,
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&resolved).is_ok());
        let resolve_replay = app.handle_api_request(Request {
            id: "resolve-replay".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key,
                claim.claim_id,
                ResolveOutcome::Settled,
            )),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&resolve_replay).is_ok());
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_claim_and_resolve_fail_closed_after_sender_replacement_or_recovery() {
        let (mut app, _pane_id, sender_key, directory) = app_with_active_sender();
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(active_submit(sender_key.clone(), "f".repeat(64))),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&submitted).is_ok());
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender_key)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace sender generation");
        let stale = app.handle_api_request(Request {
            id: "stale-claim".into(),
            method: Method::MailboxClaim(active_claim(sender_key.clone())),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale claim error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        sender_store
            .cas(
                Some(3),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 4,
                },
            )
            .expect("persist unconfirmed replacement");
        sender_store.recover().expect("recover replacement");
        let recovered = app.handle_api_request(Request {
            id: "recovered-resolve".into(),
            method: Method::MailboxResolve(active_resolve(
                sender_key,
                "claim-unavailable".into(),
                ResolveOutcome::Settled,
            )),
        });
        let recovered: ErrorResponse = serde_json::from_str(&recovered).expect("recovery error");
        assert_eq!(recovered.error.code, "mailbox_authority_unavailable");
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    fn install_committed_active(app: &mut App, sender_key: &str, generation: u64) {
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &app.sender_authority_dir,
            sender_key,
        )
        .expect("authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender_key.into(),
                    process_generation: generation,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 1,
                },
            )
            .expect("committed active record");
        app.install_offline_mailbox_authority(
            store.load().expect("read active").expect("active record"),
        )
        .expect("install active authority");
    }

    #[test]
    fn cross_recipient_offline_delivery_survives_fresh_consumer_execution() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        let recipient_b = RecipientKey {
            recipient_id: "recipient-b".into(),
            generation: "1".into(),
        };
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, recipient_b.clone())
            .expect("server provisioned A to B grant");
        let submitted = app.handle_api_request(Request {
            id: "a-to-offline-b".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a.clone(),
                grant_id.clone(),
                recipient_b.clone(),
                "a".repeat(64),
            )),
        });
        let receipt: SuccessResponse = serde_json::from_str(&submitted).expect("receipt");
        let ResponseResult::MailboxOfflineSubmitted { receipt } = receipt.result else {
            panic!("expected durable receipt")
        };
        install_committed_active(&mut app, "recipient-b", 1);
        let snapshot = app.handle_api_request(Request {
            id: "fresh-b-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b.clone(),
            )),
        });
        let snapshot: SuccessResponse = serde_json::from_str(&snapshot).expect("snapshot");
        let ResponseResult::MailboxSnapshot { snapshot } = snapshot.result else {
            panic!("expected B snapshot")
        };
        assert_eq!(snapshot.heads.len(), 1);
        assert_eq!(snapshot.heads[0].subject, "offline subject");
        assert_eq!(snapshot.heads[0].body, "offline body");
        assert_eq!(snapshot.heads[0].recipient_generation, "1");
        assert_eq!(snapshot.heads[0].sender, sender_a);
        assert_eq!(snapshot.heads[0].target, "recipient-b");
        assert_eq!(snapshot.heads[0].grant_id, grant_id);
        assert_eq!(snapshot.heads[0].message_id, "message-1");
        assert_eq!(snapshot.heads[0].kind, "report");
        assert_eq!(snapshot.heads[0].priority, "normal");
        assert_eq!(snapshot.heads[0].original_sequence, 1);
        assert!(snapshot.heads[0].enqueue_epoch > 0);
        assert!(snapshot.heads[0].accepted_at > 0);
        assert_eq!(snapshot.receipts, vec![receipt.clone()]);
        let claimed = app.handle_api_request(Request {
            id: "fresh-b-claim".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:1".into(),
                recipient: recipient_b.clone(),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let claimed: SuccessResponse = serde_json::from_str(&claimed).expect("claim");
        let ResponseResult::MailboxClaimed { claim: Some(claim) } = claimed.result else {
            panic!("expected B claim")
        };
        assert_eq!(claim.stable_id, receipt.stable_id);
        assert_eq!(claim.digest, receipt.digest);
        assert_eq!(claim.recipient, recipient_b.clone());
        let claimed_snapshot = app.handle_api_request(Request {
            id: "claimed-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b,
            )),
        });
        let claimed_snapshot: SuccessResponse =
            serde_json::from_str(&claimed_snapshot).expect("claimed snapshot");
        let ResponseResult::MailboxSnapshot { snapshot } = claimed_snapshot.result else {
            panic!("expected claimed snapshot")
        };
        assert_eq!(
            snapshot.claim.as_ref().map(|claim| &claim.digest),
            Some(&receipt.digest)
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn cross_recipient_rejects_unprovisioned_sender_and_stale_consumer_execution() {
        let (mut app, _pane_id, sender_a, directory) = app_with_active_sender();
        let recipient_b = RecipientKey {
            recipient_id: "recipient-b".into(),
            generation: "1".into(),
        };
        let unauthorized = app.handle_api_request(Request {
            id: "unauthorized".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a.clone(),
                "offline:unauthorized:1".into(),
                recipient_b.clone(),
                "b".repeat(64),
            )),
        });
        let unauthorized: ErrorResponse =
            serde_json::from_str(&unauthorized).expect("unauthorized error");
        assert_eq!(unauthorized.error.code, "mailbox_capability_mismatch");
        let grant_id = app
            .provision_cross_recipient_mailbox_grant(&sender_a, recipient_b.clone())
            .expect("server provisioned grant");
        let submitted = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(submit(
                sender_a,
                grant_id,
                recipient_b.clone(),
                "c".repeat(64),
            )),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&submitted).is_ok(),
            "{submitted}"
        );
        install_committed_active(&mut app, "recipient-b", 1);
        let b_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, "recipient-b")
                .expect("B authority store");
        b_store
            .cas(
                Some(1),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "recipient-b".into(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 2,
                },
            )
            .expect("replace B execution");
        let stale = app.handle_api_request(Request {
            id: "stale-b".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:1".into(),
                recipient: recipient_b.clone(),
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        let stale: ErrorResponse = serde_json::from_str(&stale).expect("stale B error");
        assert_eq!(stale.error.code, "mailbox_authority_unavailable");
        let stale_snapshot = app.handle_api_request(Request {
            id: "stale-b-snapshot".into(),
            method: Method::MailboxSnapshot(mailbox_snapshot(
                "recipient-b".into(),
                "offline:recipient-b:1".into(),
                recipient_b.clone(),
            )),
        });
        let stale_snapshot: ErrorResponse =
            serde_json::from_str(&stale_snapshot).expect("stale snapshot error");
        assert_eq!(stale_snapshot.error.code, "mailbox_authority_unavailable");
        app.install_offline_mailbox_authority(b_store.load().expect("read B").expect("B active"))
            .expect("fresh B authority");
        let fresh = app.handle_api_request(Request {
            id: "fresh-b".into(),
            method: Method::MailboxClaim(crate::api::schema::MailboxClaimParams {
                caller: "recipient-b".into(),
                grant_id: "offline:recipient-b:2".into(),
                recipient: recipient_b,
                claim: ClaimRequest {
                    protocol: PROTOCOL.into(),
                },
            }),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&fresh).is_ok());
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }

    #[test]
    fn mailbox_authority_recovery_invalidates_unconfirmed_sender() {
        let directory = sender_directory();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, "sender")
            .expect("sender authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "sender".into(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist preparing sender");
        let recovered = store
            .recover()
            .expect("recover sender record")
            .expect("record remains for audit");
        assert_eq!(
            recovered.phase,
            crate::sender_authority::SenderAuthorityPhase::Invalidated
        );
        assert!(!recovered.authoritative());
        std::fs::remove_dir_all(directory).expect("remove mailbox directory");
    }
}
