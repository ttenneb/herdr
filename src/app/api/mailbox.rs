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
            Err(crate::app::mailbox::OfflineMailboxError::Transport(error)) => encode_error(
                id,
                error.code(),
                "offline mailbox route authorization rejected",
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
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::HashSet;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use crate::api::schema::{ErrorResponse, Method, Request, SuccessResponse};
    use crate::app::mailbox::OfflineMailboxAuthority;
    use crate::config::Config;
    use crate::delegation::{DelegationId, DelegationRecord, Delegations};
    use crate::direct_transport::{
        CanonicalTopology, ConfigProvenance, EdgeKind, Effect, GrantAuthority, GrantClass,
        GrantRequest, LiveManifest, PrincipalBinding, RecipientChannelRegistry, RouteScope,
        SessionGeneration, TrustedMailboxChannelContext, FIXTURE_DIGEST, GATE_MAILBOX_VERSION,
        PROTOCOL_VERSION,
    };
    use crate::mailbox::RecipientKey;
    use crate::mailbox_v1::{Submit, PROTOCOL};

    use super::*;

    fn delegation_id(raw: u64) -> DelegationId {
        DelegationId::from_raw(raw).expect("valid delegation id")
    }

    fn scope() -> RouteScope {
        RouteScope {
            repository: "repo".into(),
            worktree: "/repo/worktree".into(),
            branch: "offline-mailbox".into(),
        }
    }

    fn principal(raw: u64, generation: u64) -> SessionGeneration {
        SessionGeneration {
            session: format!("session-{raw}"),
            generation,
            delegation_id: delegation_id(raw),
        }
    }

    fn topology(caller: &SessionGeneration, recipient: &SessionGeneration) -> CanonicalTopology {
        let delegations = Delegations::from_records([
            DelegationRecord {
                id: caller.delegation_id,
                pane_id: Some(crate::layout::PaneId::from_raw(1)),
                parent_id: None,
                purpose: None,
                sibling_rank: 0,
                tombstone: false,
            },
            DelegationRecord {
                id: recipient.delegation_id,
                pane_id: Some(crate::layout::PaneId::from_raw(2)),
                parent_id: Some(caller.delegation_id),
                purpose: None,
                sibling_rank: 0,
                tombstone: false,
            },
        ])
        .expect("valid persisted topology");
        let scope = scope();
        CanonicalTopology::from_persisted(
            &delegations,
            [caller, recipient]
                .into_iter()
                .cloned()
                .map(|identity| PrincipalBinding {
                    identity,
                    scope: scope.clone(),
                }),
            9,
        )
        .expect("complete principal bindings")
    }

    fn submit(caller: &str, recipient: RecipientKey) -> MailboxOfflineSubmitParams {
        MailboxOfflineSubmitParams {
            caller: caller.into(),
            grant_id: "grant-1".into(),
            recipient,
            submit: Submit {
                protocol: PROTOCOL.into(),
                stable_id: "stable-1".into(),
                revision: 1,
                digest: "a".repeat(64),
                delivery_digest: "b".repeat(64),
                subject: "offline subject".into(),
                body: "offline body".into(),
            },
        }
    }

    fn app_with_authenticated_mailbox() -> (App, RecipientKey, std::path::PathBuf) {
        let caller = principal(1, 1);
        let recipient = principal(2, 2);
        let topology = topology(&caller, &recipient);
        let (server_socket, _client_socket) = UnixStream::pair().expect("local socket pair");
        let context = TrustedMailboxChannelContext::from_verified_local_socket(
            server_socket.as_raw_fd(),
            recipient.clone(),
            "recipient-channel".into(),
            std::process::id(),
            recipient.generation,
        )
        .expect("OS-authenticated local recipient route");
        let manifest = LiveManifest {
            recipient: recipient.clone(),
            protocol_min: PROTOCOL_VERSION,
            protocol_max: PROTOCOL_VERSION,
            fixture_digest: FIXTURE_DIGEST.into(),
            features: HashSet::from(["direct-v1".into()]),
            gate_mailbox_version: GATE_MAILBOX_VERSION,
            package_revision: "test".into(),
            effective_config_digest: "a".repeat(64),
            config_provenance: ConfigProvenance::LinkedLocalOverride {
                path: "/repo/worktree/config.json".into(),
            },
            registration_epoch: 9,
            expires_at: 100,
            channel_binding: context.binding().id().into(),
        };
        let mut recipients = RecipientChannelRegistry::default();
        recipients
            .register(context, manifest, topology.clone(), 10)
            .expect("registered local recipient route");
        let mut grants = GrantAuthority::new(9);
        recipients
            .with_authenticated_route(&recipient, 10, |_, _manifests, _topology, route| {
                grants.issue(
                    GrantRequest {
                        grant_id: "grant-1".into(),
                        task_id: "task-1".into(),
                        delegation_id: recipient.delegation_id,
                        edge_kind: EdgeKind::OwnerHelper,
                        class: GrantClass::Standard,
                        issuer: caller.clone(),
                        recipient: recipient.clone(),
                        authorized_by: None,
                        scope: scope(),
                        message_kinds: HashSet::from([
                            crate::direct_transport::MessageKind::Report,
                        ]),
                        effects: HashSet::from([Effect::Send]),
                        issued_at: 9,
                        expires_at: 100,
                        topology_revision: 9,
                        grant_revision: 1,
                        route,
                    },
                    &topology,
                    _manifests,
                    10,
                )
            })
            .expect("issue authorized grant");
        let directory = std::env::temp_dir().join(format!(
            "herdr-offline-mailbox-api-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let store = crate::mailbox::MailboxStore::open(&directory).expect("mailbox store");
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.sender_authority_dir = directory.clone();
        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
            .expect("sender authority store");
        sender_store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "sender-1".into(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 1,
                },
            )
            .expect("persist active sender authority");
        app.install_offline_mailbox_authority(OfflineMailboxAuthority {
            sender_key: "sender-1".into(),
            sender_generation: 1,
            caller_selector: caller.session.clone(),
            caller,
            scope: scope(),
            recipients,
            grants,
            store,
            now: 10,
        })
        .expect("install current authenticated authority");
        (
            app,
            RecipientKey {
                recipient_id: recipient.session,
                generation: recipient.generation.to_string(),
            },
            directory,
        )
    }

    #[test]
    fn offline_mailbox_endpoint_persists_and_reads_back_server_receipt() {
        let (mut app, recipient, directory) = app_with_authenticated_mailbox();
        let response = app.handle_api_request(Request {
            id: "submit".into(),
            method: Method::MailboxOfflineSubmit(submit("session-1", recipient)),
        });
        let success: SuccessResponse = serde_json::from_str(&response).expect("success response");
        let ResponseResult::MailboxOfflineSubmitted { receipt } = success.result else {
            panic!("expected mailbox receipt")
        };
        let authority = app
            .offline_mailbox_authority
            .as_ref()
            .expect("authenticated authority retained");
        assert_eq!(
            authority
                .store
                .load()
                .expect("read durable store")
                .receipts
                .get(&receipt.delivery_digest),
            Some(&receipt)
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox test directory");
    }

    #[test]
    fn offline_mailbox_endpoint_rejects_spoofed_caller_selector() {
        let (mut app, recipient, directory) = app_with_authenticated_mailbox();
        let response = app.handle_api_request(Request {
            id: "spoof".into(),
            method: Method::MailboxOfflineSubmit(submit("session-spoof", recipient)),
        });
        let error: ErrorResponse = serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "mailbox_caller_mismatch");
        let authority = app
            .offline_mailbox_authority
            .as_ref()
            .expect("authenticated authority retained");
        assert!(
            authority
                .store
                .load()
                .expect("read store")
                .receipts
                .is_empty(),
            "spoofed caller must not reach durable admission"
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox test directory");
    }

    #[test]
    fn offline_mailbox_endpoint_fails_closed_after_sender_generation_replacement() {
        let (mut app, recipient, directory) = app_with_authenticated_mailbox();
        let sender_store = crate::sender_authority::SenderAuthorityStore::open(&directory)
            .expect("sender authority store");
        sender_store
            .cas(
                Some(1),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "sender-1".into(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 2,
                },
            )
            .expect("replace sender generation");
        let response = app.handle_api_request(Request {
            id: "replaced".into(),
            method: Method::MailboxOfflineSubmit(submit("session-1", recipient)),
        });
        let error: ErrorResponse = serde_json::from_str(&response).expect("error response");
        assert_eq!(error.error.code, "mailbox_authority_unavailable");
        assert!(
            app.offline_mailbox_authority
                .as_ref()
                .expect("old authority remains only as a stale local value")
                .store
                .load()
                .expect("read store")
                .receipts
                .is_empty(),
            "a replaced sender generation cannot reach durable admission"
        );
        drop(app);
        std::fs::remove_dir_all(directory).expect("remove mailbox test directory");
    }
}
