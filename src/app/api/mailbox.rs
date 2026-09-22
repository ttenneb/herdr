use crate::api::schema::{MailboxOfflineSubmitParams, ResponseResult};
use crate::app::App;

use super::responses::{encode_error, encode_success};

impl App {
    pub(super) fn handle_mailbox_offline_submit(
        &mut self,
        id: String,
        params: MailboxOfflineSubmitParams,
    ) -> String {
        match self.offline_mailbox_authority_current() {
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
            .offline_mailbox_authority
            .as_mut()
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

    pub(super) fn handle_mailbox_claim(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxClaimParams,
    ) -> String {
        match self.offline_mailbox_authority_current() {
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
            .offline_mailbox_authority
            .as_ref()
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

    pub(super) fn handle_mailbox_resolve(
        &mut self,
        id: String,
        params: crate::api::schema::MailboxResolveParams,
    ) -> String {
        match self.offline_mailbox_authority_current() {
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
            .offline_mailbox_authority
            .as_ref()
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
        let store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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
        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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

        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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
    fn mailbox_authority_exit_invalidates_exact_active_generation() {
        let (mut app, pane_id, sender_key, directory) = app_with_active_sender();
        app.handle_internal_event(AppEvent::PaneDied { pane_id });
        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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
        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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

    #[test]
    fn mailbox_authority_recovery_invalidates_unconfirmed_sender() {
        let directory = sender_directory();
        let store = crate::sender_authority::SenderAuthorityStore::open(&directory)
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
