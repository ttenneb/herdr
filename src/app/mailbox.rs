use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::fd::RawFd;
// No accepted mailbox stream exists off Unix; the socket verifier refuses it.
#[cfg(not(unix))]
type RawFd = i32;

use crate::api::schema::MailboxOfflineSubmitParams;
use crate::app::App;
use crate::direct_transport::TransportError;
use crate::direct_transport::{SessionGeneration, TrustedMailboxChannelContext};

/// Host-owned discovery input for Pi extension bootstrap. The address grants no
/// scope or authority; the accepted server stream authenticates both separately.
pub(crate) const PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV: &str = "HERDR_MAILBOX_BOOTSTRAP_ADDRESS";

/// Server-minted offline capability. Its grant and recipient identifiers are
/// selectors on the wire; the exact sender key/generation remains server-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfflineMailboxCapability {
    pub(crate) grant_id: String,
    pub(crate) recipient: crate::mailbox::RecipientKey,
}

/// Server-owned authority installed only from an Active sender record. It has
/// no Pi attachment, consumer socket, process ID, manifest, or client-supplied
/// identity dependency.
pub(crate) struct OfflineMailboxAuthority {
    pub(crate) sender_key: String,
    pub(crate) sender_generation: u64,
    pub(crate) caller_selector: String,
    capabilities: BTreeMap<String, OfflineMailboxCapability>,
    pub(crate) store: crate::mailbox::MailboxStore,
}

/// A server-issued scope attached to one verified accepted Unix stream.
/// The values are selected from the current Active record and are intentionally
/// not decoded from bootstrap or dispatch frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxBootstrapSession {
    pub(crate) caller: String,
    pub(crate) recipient: crate::mailbox::RecipientKey,
    pub(crate) grant_id: String,
    pub(crate) active_execution_generation: u64,
    pub(crate) binding_generation: String,
    pub(crate) parent_report: Option<BoundParentReportRoute>,
    pub(crate) history_only: bool,
    history_edge: Option<(
        crate::delegation::DelegationId,
        Option<crate::delegation::DelegationId>,
    )>,
    context: TrustedMailboxChannelContext,
    /// Set only for a Pi without a trusted managed launch (hand-typed or a
    /// Collection helper), accepted when `[experimental]
    /// unmanaged_pi_messages` is on. Such a session may read, claim, edit and
    /// resolve only its own inbox; it never gets sender, grant, bound-report
    /// or route authority.
    pub(crate) recipient_only: Option<RecipientOnlyBinding>,
    /// The pane's durable Messages queue (`pane:<queueKey>`), advertised to Pi.
    pub(crate) pane_inbox: Option<crate::mailbox::RecipientKey>,
}

/// The exact foreground Pi execution a recipient-only session is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecipientOnlyBinding {
    pub(crate) foreground_pid: u32,
    pub(crate) start_ticks: u64,
}

/// Ephemeral readiness earned only by a durably acknowledged graph snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadyDelegationRoute {
    pub(crate) child: crate::delegation::DelegationId,
    pub(crate) parent: crate::delegation::DelegationId,
    pub(crate) child_pane: crate::layout::PaneId,
    pub(crate) parent_pane: crate::layout::PaneId,
    pub(crate) child_terminal: crate::terminal::TerminalId,
    pub(crate) parent_terminal: crate::terminal::TerminalId,
    pub(crate) child_generation: u64,
    pub(crate) parent_generation: u64,
    pub(crate) child_session: crate::api::schema::AgentSessionInfo,
    pub(crate) parent_session: crate::api::schema::AgentSessionInfo,
    pub(crate) child_revision: u64,
    pub(crate) parent_revision: u64,
    pub(crate) epoch: String,
}

/// Frozen to the exact delegation edge and parent execution at stream accept.
/// Only the server derives these fields; a request supplies just a typed report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundParentReportRoute {
    pub(crate) recipient: crate::mailbox::RecipientKey,
    pub(crate) grant_id: String,
    parent_generation: u64,
    child_delegation: crate::delegation::DelegationId,
    parent_delegation: crate::delegation::DelegationId,
    parent_pane: crate::layout::PaneId,
    route_epoch: String,
}

impl BoundParentReportRoute {
    pub(crate) fn route_epoch(&self) -> &str {
        &self.route_epoch
    }
}

impl MailboxBootstrapSession {
    pub(crate) fn context(&self) -> &TrustedMailboxChannelContext {
        &self.context
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailboxBootstrapError {
    GrantMissing,
    GrantRevoked,
    PeerRejected,
    InvalidRequest,
    /// A well-formed request the server handler refused (or failed after
    /// admission), with its specific error code.
    Refused(&'static str),
    /// The named head or claim exists but is outside this session's own
    /// pane inbox; nothing was changed.
    HeadOutOfScope,
    /// The head's claim belongs to another execution that is still alive;
    /// only a gone execution's claim may be dropped, retried or recovered.
    ClaimExecutionAlive,
    /// provision_recipient for an agent outside the caller's own delegation
    /// edges (its parent or a direct child).
    RecipientNotAllowed,
    /// A gone execution's claim that was never admitted cannot be settled
    /// as recovered; it needs an explicit Drop or Retry.
    RecoveryNeedsDropOrRetry,
}

/// Error codes a mailbox handler may return that are passed through to the
/// bootstrap wire unchanged; anything else becomes `mailbox_request_refused`.
const PASSTHROUGH_CODES: &[&str] = &[
    "mailbox_authority_unavailable",
    "mailbox_caller_mismatch",
    "mailbox_capability_mismatch",
    "mailbox_replay_rejected",
    "mailbox_store_failed",
    "mailbox_receipt_missing",
    "mailbox_claim_failed",
    "mailbox_edit_failed",
    "mailbox_edit_claimed",
    "mailbox_edit_conflict",
    "mailbox_head_out_of_scope",
    "mailbox_claim_execution_alive",
    "mailbox_recipient_not_allowed",
    "mailbox_caller_unauthenticated",
    "mailbox_recovery_needs_drop_or_retry",
    "mailbox_resolve_failed",
    "mailbox_snapshot_failed",
    "report_route_required",
    "authority_unconfirmed",
    "channel_generation_mismatch",
    "channel_peer_mismatch",
    "correlation_conflict",
    "grant_expired",
    "grant_missing",
    "grant_revoked",
    "grant_scope_mismatch",
    "message_id_collision",
    "operation_not_allowed",
    "protocol_incompatible",
    "rate_limited",
    "recipient_session_replaced",
];

impl MailboxBootstrapError {
    /// The typed wire error for a handler's JSON error response.
    pub(crate) fn from_handler_error(code: &str) -> Self {
        Self::Refused(
            PASSTHROUGH_CODES
                .iter()
                .find(|known| **known == code)
                .copied()
                .unwrap_or("mailbox_request_refused"),
        )
    }
}

#[derive(Debug)]
pub(crate) enum OfflineMailboxInstallError {
    SenderRecordUnavailable,
    SenderRecordMismatch,
    Store(std::io::Error),
    MailboxStore(crate::mailbox::MailboxError),
}

pub(crate) enum OfflineMailboxError {
    CallerMismatch,
    CapabilityMismatch,
    /// The head exists but is outside what this capability may touch: not
    /// in the caller's own inbox, and not a head the caller itself sent to
    /// the named recipient.
    HeadOutOfScope,
    /// A send grant to another recipient cannot claim or resolve that
    /// recipient's messages.
    NotRecipient,
    Replay,
    Transport(TransportError),
    Store(crate::mailbox::MailboxError),
    ReceiptMissing,
}

impl OfflineMailboxAuthority {
    /// Mints the narrow default capability for the durable sender generation.
    /// Future server-side recipient provisioning may add separately scoped
    /// capabilities; neither wire selectors nor attachments can do so.
    pub(crate) fn from_active_sender(
        record: &crate::sender_authority::SenderAuthorityRecord,
        store: crate::mailbox::MailboxStore,
        cross_grants: impl IntoIterator<Item = crate::mailbox::MailboxGrant>,
    ) -> Option<Self> {
        if !record.authoritative() {
            return None;
        }
        // Logical identity stays stable across execution replacement; the
        // Active sender record below remains the separate execution binding.
        let recipient = crate::mailbox::RecipientKey {
            recipient_id: record.sender_key.clone(),
            generation: "1".into(),
        };
        let grant_id = format!(
            "offline:{}:{}",
            record.sender_key, record.process_generation
        );
        let capability = OfflineMailboxCapability {
            grant_id: grant_id.clone(),
            recipient: recipient.clone(),
        };
        let mut capabilities = BTreeMap::from([(grant_id, capability)]);
        for grant in cross_grants {
            if grant.sender == recipient {
                capabilities.insert(
                    grant.grant_id.clone(),
                    OfflineMailboxCapability {
                        grant_id: grant.grant_id,
                        recipient: grant.recipient,
                    },
                );
            }
        }
        Some(Self {
            sender_key: record.sender_key.clone(),
            sender_generation: record.process_generation,
            caller_selector: record.sender_key.clone(),
            capabilities,
            store,
        })
    }

    pub(crate) fn matches_sender_record(
        &self,
        record: &crate::sender_authority::SenderAuthorityRecord,
    ) -> bool {
        record.authoritative()
            && record.sender_key == self.sender_key
            && record.process_generation == self.sender_generation
    }

    fn capability_for(
        &self,
        caller: &str,
        grant_id: &str,
        recipient: &crate::mailbox::RecipientKey,
    ) -> Result<&OfflineMailboxCapability, OfflineMailboxError> {
        if caller != self.caller_selector {
            return Err(OfflineMailboxError::CallerMismatch);
        }
        let Some(capability) = self.capabilities.get(grant_id) else {
            return Err(OfflineMailboxError::CapabilityMismatch);
        };
        if &capability.recipient != recipient {
            return Err(OfflineMailboxError::CapabilityMismatch);
        }
        Ok(capability)
    }

    pub(crate) fn submit(
        &mut self,
        params: MailboxOfflineSubmitParams,
    ) -> Result<crate::mailbox::AdmissionReceipt, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Submit(
            params.submit.clone(),
        ))
        .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        if self
            .store
            .load()
            .map_err(OfflineMailboxError::Store)?
            .receipts
            .contains_key(&params.submit.delivery_digest)
        {
            return Err(OfflineMailboxError::Replay);
        }
        let accepted_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| OfflineMailboxError::Store(crate::mailbox::MailboxError::InvalidRecord))?
            .as_secs();
        let receipt = crate::mailbox_v1::submit_offline(
            &self.store,
            capability.recipient.clone(),
            crate::mailbox::MailboxProvenance {
                sender: self.sender_key.clone(),
                target: capability.recipient.recipient_id.clone(),
                grant_id: capability.grant_id.clone(),
                accepted_at,
            },
            params.submit,
        )
        .map_err(OfflineMailboxError::Store)?;
        match self
            .store
            .load()
            .map_err(OfflineMailboxError::Store)?
            .receipts
            .get(&receipt.delivery_digest)
        {
            Some(persisted) if persisted == &receipt => Ok(receipt),
            _ => Err(OfflineMailboxError::ReceiptMissing),
        }
    }

    /// Through a send grant a caller sees only the heads it sent itself;
    /// through its own inbox capability, everything in its inbox.
    fn sender_view(
        &self,
        mut snapshot: crate::mailbox_v1::Snapshot,
        recovered: &crate::mailbox::RecoveredMailbox,
        own_inbox: bool,
    ) -> crate::mailbox_v1::Snapshot {
        if own_inbox {
            return snapshot;
        }
        let mine = |stable_id: &str| {
            recovered
                .heads
                .get(stable_id)
                .is_some_and(|head| head.sender == self.sender_key)
        };
        snapshot.heads.retain(|head| head.sender == self.sender_key);
        snapshot.head_states.retain(|state| mine(&state.stable_id));
        snapshot.receipts.retain(|receipt| mine(&receipt.stable_id));
        snapshot.claim = None;
        snapshot
    }

    /// The capability for the caller's own inbox (as opposed to a send grant
    /// to another recipient).
    fn owns_inbox(&self, capability: &OfflineMailboxCapability) -> bool {
        capability.recipient.recipient_id == self.sender_key
    }

    /// Edit scope: a recipient edits heads in its own inbox; a sender edits
    /// only heads it sent itself, to the recipient named in the request.
    fn may_edit(
        &self,
        capability: &OfflineMailboxCapability,
        head: &crate::mailbox::MailboxHead,
    ) -> bool {
        head.recipient == capability.recipient
            && (self.owns_inbox(capability) || head.sender == self.sender_key)
    }

    pub(crate) fn claim(
        &self,
        params: crate::api::schema::MailboxClaimParams,
        _current_session: Option<&str>,
    ) -> Result<Option<crate::mailbox::Claim>, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Claim(params.claim))
            .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        if !self.owns_inbox(capability) {
            return Err(OfflineMailboxError::NotRecipient);
        }
        self.store
            .claim_next(&capability.recipient)
            .map_err(OfflineMailboxError::Store)
    }

    pub(crate) fn snapshot(
        &self,
        params: crate::api::schema::MailboxSnapshotParams,
        current_session: Option<&str>,
    ) -> Result<crate::mailbox_v1::Snapshot, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::List(
            crate::mailbox_v1::List {
                protocol: params.protocol,
                recipient: params.recipient.clone(),
            },
        ))
        .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        let recovered = self.store.load().map_err(OfflineMailboxError::Store)?;
        let own_inbox = self.owns_inbox(capability);
        crate::mailbox_v1::snapshot(&recovered, &capability.recipient)
            .map(|snapshot| self.sender_view(snapshot, &recovered, own_inbox))
            .map(|snapshot| {
                crate::app::messages::filter_recipient_snapshot(
                    snapshot,
                    &recovered,
                    current_session,
                )
            })
            .map_err(OfflineMailboxError::Store)
    }

    pub(crate) fn edit(
        &self,
        params: crate::api::schema::MailboxEditParams,
        current_session: Option<&str>,
    ) -> Result<crate::mailbox_v1::Snapshot, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Edit(params.edit.clone()))
            .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        if let Some(head) = self
            .store
            .load()
            .map_err(OfflineMailboxError::Store)?
            .heads
            .get(&params.edit.stable_id)
        {
            if !self.may_edit(capability, head) {
                return Err(OfflineMailboxError::HeadOutOfScope);
            }
        }
        crate::mailbox_v1::edit_unclaimed(&self.store, params.edit)
            .map_err(OfflineMailboxError::Store)?;
        // Read from the durable stream after the edit's sync before responding;
        // callers receive server authority rather than an optimistic local edit.
        let recovered = self.store.load().map_err(OfflineMailboxError::Store)?;
        let own_inbox = self.owns_inbox(capability);
        crate::mailbox_v1::snapshot(&recovered, &capability.recipient)
            .map(|snapshot| self.sender_view(snapshot, &recovered, own_inbox))
            .map(|snapshot| {
                crate::app::messages::filter_recipient_snapshot(
                    snapshot,
                    &recovered,
                    current_session,
                )
            })
            .map_err(OfflineMailboxError::Store)
    }

    pub(crate) fn resolve(
        &self,
        params: crate::api::schema::MailboxResolveParams,
    ) -> Result<crate::mailbox::ClaimResolution, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Resolve(
            params.resolve.clone(),
        ))
        .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        if !self.owns_inbox(capability) {
            return Err(OfflineMailboxError::NotRecipient);
        }
        let claim = self
            .store
            .load()
            .map_err(OfflineMailboxError::Store)?
            .claims
            .values()
            .find(|claim| claim.claim_id == params.resolve.claim_id)
            .cloned()
            .ok_or(OfflineMailboxError::CapabilityMismatch)?;
        if claim.recipient != capability.recipient {
            return Err(OfflineMailboxError::CapabilityMismatch);
        }
        let outcome = match params.resolve.outcome {
            crate::mailbox_v1::ResolveOutcome::Admitted => {
                crate::mailbox::ClaimResolutionOutcome::Admitted
            }
            crate::mailbox_v1::ResolveOutcome::Settled => {
                crate::mailbox::ClaimResolutionOutcome::Settled
            }
        };
        self.store
            .resolve_claim(&claim.claim_id, outcome)
            .map_err(OfflineMailboxError::Store)
    }
}

impl App {
    pub(crate) fn install_offline_mailbox_authority(
        &mut self,
        record: crate::sender_authority::SenderAuthorityRecord,
    ) -> Result<(), OfflineMailboxInstallError> {
        let sender_store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &record.sender_key,
        )
        .map_err(OfflineMailboxInstallError::Store)?;
        let current = sender_store
            .load()
            .map_err(OfflineMailboxInstallError::Store)?
            .ok_or(OfflineMailboxInstallError::SenderRecordUnavailable)?;
        if current != record || !record.authoritative() {
            return Err(OfflineMailboxInstallError::SenderRecordMismatch);
        }
        let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir)
            .map_err(OfflineMailboxInstallError::MailboxStore)?;
        let grants = store
            .load()
            .map_err(OfflineMailboxInstallError::MailboxStore)?
            .grants
            .into_values();
        let authority = OfflineMailboxAuthority::from_active_sender(&record, store, grants)
            .ok_or(OfflineMailboxInstallError::SenderRecordMismatch)?;
        self.offline_mailbox_authorities
            .insert(record.sender_key.clone(), authority);
        Ok(())
    }

    pub(crate) fn offline_mailbox_authority_current(
        &self,
        caller: &str,
    ) -> Result<bool, OfflineMailboxInstallError> {
        let Some(authority) = self.offline_mailbox_authorities.get(caller) else {
            return Ok(false);
        };
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &authority.sender_key,
        )
        .map_err(OfflineMailboxInstallError::Store)?;
        Ok(store
            .load()
            .map_err(OfflineMailboxInstallError::Store)?
            .is_some_and(|record| authority.matches_sender_record(&record)))
    }

    /// Server-only provisioning seam. It requires a committed Active A binding,
    /// persists recipient policy separately, and refreshes A's capability set.
    pub(crate) fn provision_cross_recipient_mailbox_grant(
        &mut self,
        sender_key: &str,
        recipient: crate::mailbox::RecipientKey,
    ) -> Result<String, OfflineMailboxInstallError> {
        let grant_id = format!(
            "mailbox:{sender_key}:1:{}:{}",
            recipient.recipient_id, recipient.generation
        );
        self.provision_cross_recipient_mailbox_grant_with_id(sender_key, recipient, grant_id)
    }

    fn provision_cross_recipient_mailbox_grant_with_id(
        &mut self,
        sender_key: &str,
        recipient: crate::mailbox::RecipientKey,
        grant_id: String,
    ) -> Result<String, OfflineMailboxInstallError> {
        if !self.offline_mailbox_authority_current(sender_key)? {
            return Err(OfflineMailboxInstallError::SenderRecordMismatch);
        }
        let sender = crate::mailbox::RecipientKey {
            recipient_id: sender_key.into(),
            generation: "1".into(),
        };
        let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir)
            .map_err(OfflineMailboxInstallError::MailboxStore)?;
        store
            .provision_grant(crate::mailbox::MailboxGrant {
                grant_id: grant_id.clone(),
                sender,
                recipient,
            })
            .map_err(OfflineMailboxInstallError::MailboxStore)?;
        let record = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            sender_key,
        )
        .map_err(OfflineMailboxInstallError::Store)?
        .load()
        .map_err(OfflineMailboxInstallError::Store)?
        .ok_or(OfflineMailboxInstallError::SenderRecordUnavailable)?;
        self.install_offline_mailbox_authority(record)?;
        Ok(grant_id)
    }

    pub(crate) fn ready_route_shape(
        &self,
        child_id: crate::delegation::DelegationId,
        parent_id: crate::delegation::DelegationId,
    ) -> Option<ReadyDelegationRoute> {
        let child = self.state.delegations.get(child_id)?;
        let parent = self.state.delegations.get(parent_id)?;
        if child.parent_id != Some(parent_id) || child.tombstone || parent.tombstone {
            return None;
        }
        let child_pane = child.pane_id?;
        let parent_pane = parent.pane_id?;
        if child_pane == parent_pane {
            return None;
        }
        let (child_ws, _) = self.find_pane(child_pane)?;
        let (parent_ws, _) = self.find_pane(parent_pane)?;
        let child_terminal = self.state.workspaces[child_ws]
            .terminal_id(child_pane)?
            .clone();
        let parent_terminal = self.state.workspaces[parent_ws]
            .terminal_id(parent_pane)?
            .clone();
        if child_terminal == parent_terminal {
            return None;
        }
        let child_generation = self.active_pi_sender_generation(&child_terminal.to_string())?;
        let parent_generation = self.active_pi_sender_generation(&parent_terminal.to_string())?;
        if !self.exact_active_mailbox_authority(&child_terminal.to_string(), child_generation)
            || !self.exact_active_mailbox_authority(&parent_terminal.to_string(), parent_generation)
        {
            return None;
        }
        let child_session =
            self.trusted_managed_pi_session(self.state.terminals.get(&child_terminal)?)?;
        let parent_session =
            self.trusted_managed_pi_session(self.state.terminals.get(&parent_terminal)?)?;
        Some(ReadyDelegationRoute {
            child: child_id,
            parent: parent_id,
            child_pane,
            parent_pane,
            child_terminal,
            parent_terminal,
            child_generation,
            parent_generation,
            child_session,
            parent_session,
            child_revision: self.state.delegations.route_revision(child_id),
            parent_revision: self.state.delegations.route_revision(parent_id),
            epoch: String::new(),
        })
    }

    fn bound_parent_report_candidate(
        &self,
        sender_key: &str,
        sender_generation: u64,
    ) -> Option<BoundParentReportRoute> {
        if !self
            .session_writer_healthy
            .load(std::sync::atomic::Ordering::Acquire)
            || !self
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.validate(&self.session_save_path).is_ok())
        {
            return None;
        }
        let (child, child_terminal) =
            self.state
                .delegations
                .records()
                .values()
                .find_map(|record| {
                    let pane = record.pane_id?;
                    let (ws_idx, _) = self.find_pane(pane)?;
                    let terminal = self.state.workspaces[ws_idx].terminal_id(pane)?;
                    (terminal.to_string() == sender_key).then_some((record, terminal))
                })?;
        let parent_delegation = child.parent_id?;
        let ready = self.ready_delegation_routes.get(&child.id)?;
        let mut current = self.ready_route_shape(child.id, parent_delegation)?;
        current.epoch = ready.epoch.clone();
        if &current != ready {
            return None;
        }
        let parent = self.state.delegations.get(parent_delegation)?;
        let parent_pane = parent.pane_id?;
        if parent.tombstone
            || child.tombstone
            || self
                .state
                .terminals
                .get(child_terminal)?
                .managed_agent_kind()
                != Some(crate::detect::Agent::Pi)
            || !self
                .state
                .terminals
                .get(child_terminal)?
                .accepts_managed_agent_generation(sender_generation)
        {
            return None;
        }
        let (ws_idx, _) = self.find_pane(parent_pane)?;
        let parent_terminal_id = self.state.workspaces[ws_idx].terminal_id(parent_pane)?;
        let parent_key = parent_terminal_id.to_string();
        if parent_key == sender_key
            || self
                .state
                .terminals
                .get(parent_terminal_id)?
                .managed_agent_kind()
                != Some(crate::detect::Agent::Pi)
        {
            return None;
        }
        let parent_record = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &parent_key,
        )
        .ok()?
        .load()
        .ok()??;
        let parent_generation = parent_record.process_generation;
        if !parent_record.authoritative()
            || !self
                .state
                .terminals
                .get(parent_terminal_id)?
                .accepts_managed_agent_generation(parent_generation)
            || !self.exact_active_mailbox_authority(&parent_key, parent_generation)
        {
            return None;
        }
        let recipient = crate::mailbox::RecipientKey {
            recipient_id: parent_key,
            generation: "1".into(),
        };
        Some(BoundParentReportRoute {
            // Separate grant namespace: a bound-parent grant is never an
            // unrestricted cross-recipient capability on the generic API.
            grant_id: format!(
                "bound-parent-report:{sender_key}:1:{}:1:{}",
                recipient.recipient_id, ready.epoch
            ),
            recipient,
            parent_generation,
            child_delegation: child.id,
            parent_delegation,
            parent_pane,
            route_epoch: ready.epoch.clone(),
        })
    }

    pub(crate) fn bound_parent_report_current(
        &self,
        session: &MailboxBootstrapSession,
    ) -> Result<BoundParentReportRoute, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        let route = session
            .parent_report
            .as_ref()
            .ok_or(MailboxBootstrapError::GrantMissing)?;
        if self
            .bound_parent_report_candidate(&session.caller, session.active_execution_generation)
            .as_ref()
            != Some(route)
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let authority = self
            .offline_mailbox_authorities
            .get(&session.caller)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        let grant = authority
            .store
            .load()
            .map_err(|_| MailboxBootstrapError::GrantMissing)?
            .grants
            .get(&route.grant_id)
            .cloned();
        if grant
            != Some(crate::mailbox::MailboxGrant {
                grant_id: route.grant_id.clone(),
                sender: session.recipient.clone(),
                recipient: route.recipient.clone(),
            })
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(route.clone())
    }

    /// Only the server selects these fields from its already revalidated
    /// accepted child stream; a wire frame can never supply an identity.
    pub(crate) fn child_report_route_identity(
        &self,
        session: &MailboxBootstrapSession,
        route: &BoundParentReportRoute,
    ) -> Option<crate::child_report::RouteIdentity> {
        let ready = self.ready_delegation_routes.get(&route.child_delegation)?;
        if ready.parent != route.parent_delegation
            || ready.epoch != route.route_epoch
            || ready.child_terminal.to_string() != session.caller
            || ready.child_generation != session.active_execution_generation
        {
            return None;
        }
        self.ready_report_identity(ready)
    }

    fn ready_report_identity(
        &self,
        ready: &ReadyDelegationRoute,
    ) -> Option<crate::child_report::RouteIdentity> {
        let child_ws = self.find_pane(ready.child_pane)?.0;
        let parent_ws = self.find_pane(ready.parent_pane)?.0;
        Some(crate::child_report::RouteIdentity {
            child_delegation_id: ready.child.to_string(),
            parent_delegation_id: ready.parent.to_string(),
            child_pane_id: self.public_pane_id(child_ws, ready.child_pane)?,
            parent_pane_id: self.public_pane_id(parent_ws, ready.parent_pane)?,
            child_terminal_id: ready.child_terminal.to_string(),
            parent_terminal_id: ready.parent_terminal.to_string(),
            child_process_generation: ready.child_generation,
            parent_process_generation: ready.parent_generation,
            child_session: ready.child_session.clone(),
            parent_session: ready.parent_session.clone(),
            child_route_revision: ready.child_revision,
            parent_route_revision: ready.parent_revision,
            route_epoch: ready.epoch.clone(),
        })
    }

    /// C2: remember when the server committed a child's done TodoState (first
    /// commit wins; an identical repeat keeps the original time). Entries for
    /// routes that are no longer ready are dropped.
    pub(crate) fn record_child_done_at(
        &mut self,
        event: &crate::child_report::ChildReportEvent,
        recovered: &crate::mailbox::RecoveredMailbox,
    ) {
        let crate::child_report::ChildReportEvent::TodoState {
            route,
            state: crate::child_report::LocalTodoState::Done,
            ..
        } = event
        else {
            return;
        };
        let Some(position) = recovered
            .child_report_events
            .iter()
            .position(|recorded| recorded == event)
            .and_then(|index| recovered.child_report_event_cursors.get(index).copied())
        else {
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let ready = &self.ready_delegation_routes;
        self.child_report_done_at.retain(|(child, epoch, _), _| {
            ready
                .values()
                .any(|route| &route.child.to_string() == child && &route.epoch == epoch)
        });
        self.child_report_done_at
            .entry((
                route.child_delegation_id.clone(),
                route.route_epoch.clone(),
                position,
            ))
            .or_insert(now_ms);
    }

    /// C2 `child_report_signals`: a snapshot of this parent's open
    /// "child reported done, no report seen yet" facts, over its current ready
    /// bound routes only. `marker` is the journal marker read before the load.
    pub(crate) fn child_report_signals_for_parent(
        &self,
        session: &MailboxBootstrapSession,
        marker: u64,
    ) -> Result<serde_json::Value, MailboxBootstrapError> {
        const MAX_SIGNALS: usize = 64;
        self.mailbox_bootstrap_session_current(session)?;
        if session.history_only || session.recipient_only.is_some() {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let mut routes: Vec<_> = self
            .ready_delegation_routes
            .values()
            .filter(|ready| {
                ready.parent_terminal.to_string() == session.caller
                    && ready.parent_generation == session.active_execution_generation
            })
            .filter(|ready| {
                self.ready_route_shape(ready.child, ready.parent)
                    .is_some_and(|mut current| {
                        current.epoch = ready.epoch.clone();
                        &current == *ready
                    })
            })
            .filter_map(|ready| self.ready_report_identity(ready))
            .collect();
        let mut signals = Vec::new();
        if !routes.is_empty() {
            let recovered = crate::mailbox::MailboxStore::existing(&self.sender_authority_dir)
                .load()
                .map_err(|_| MailboxBootstrapError::GrantMissing)?;
            routes.sort_by(|a, b| a.child_delegation_id.cmp(&b.child_delegation_id));
            for identity in &routes {
                let Some(fact) =
                    crate::child_report::done_without_admitted_report(identity, &recovered)
                else {
                    continue;
                };
                let done_at = self
                    .child_report_done_at
                    .get(&(
                        identity.child_delegation_id.clone(),
                        identity.route_epoch.clone(),
                        fact.todo_state_cursor,
                    ))
                    .copied();
                signals.push(serde_json::json!({
                    "type": "report_unknown",
                    "reason": "coverage_unqualified",
                    "delegationId": identity.child_delegation_id,
                    "routeEpoch": identity.route_epoch,
                    "doneAt": done_at,
                    "child": {
                        "paneId": identity.child_pane_id,
                        "terminalId": identity.child_terminal_id,
                        "session": identity.child_session,
                    },
                    "todo": {
                        "localRoot": fact.local_root,
                        "localRevision": fact.local_revision,
                        "stateDigest": fact.state_digest,
                        "state": "done",
                        "todoStateCursor": fact.todo_state_cursor,
                    },
                }));
            }
        }
        signals.sort_by_key(|signal| signal["todo"]["todoStateCursor"].as_u64());
        let truncated = signals.len() > MAX_SIGNALS;
        signals.truncate(MAX_SIGNALS);
        Ok(serde_json::json!({
            "type": "child_report_signals",
            "throughCursor": marker,
            "signals": signals,
            "truncated": truncated,
        }))
    }

    /// Parent-scoped, read-only observation. A local CLI selector alone is
    /// insufficient: the accepted stream must be the current parent Pi.
    pub(crate) fn child_report_disposition_for_parent(
        &self,
        session: &MailboxBootstrapSession,
        child: crate::delegation::DelegationId,
    ) -> Result<crate::child_report::ReportDisposition, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        if !self
            .session_writer_healthy
            .load(std::sync::atomic::Ordering::Acquire)
            || !self
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.validate(&self.session_save_path).is_ok())
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let ready = self
            .ready_delegation_routes
            .get(&child)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        if ready.parent_terminal.to_string() != session.caller
            || ready.parent_generation != session.active_execution_generation
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let mut current = self
            .ready_route_shape(ready.child, ready.parent)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        current.epoch = ready.epoch.clone();
        if &current != ready {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let identity = self
            .ready_report_identity(ready)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        let recovered = crate::mailbox::MailboxStore::existing(&self.sender_authority_dir)
            .load()
            .map_err(|_| MailboxBootstrapError::GrantMissing)?;
        Ok(crate::child_report::project(&identity, &recovered))
    }

    /// Detect only a verified current child→parent edge for legacy generic
    /// paths. This cannot confer bound-report authority on those paths; it
    /// exists solely to make any such report permanently uncertain.
    pub(crate) fn legacy_child_parent_report_identity(
        &self,
        sender_terminal: &str,
        recipient_terminal: &str,
    ) -> Option<crate::child_report::RouteIdentity> {
        if !self
            .session_writer_healthy
            .load(std::sync::atomic::Ordering::Acquire)
            || !self
                .session_writer
                .as_ref()
                .is_some_and(|writer| writer.validate(&self.session_save_path).is_ok())
        {
            return None;
        }
        self.ready_delegation_routes.values().find_map(|ready| {
            if ready.child_terminal.to_string() != sender_terminal
                || ready.parent_terminal.to_string() != recipient_terminal
            {
                return None;
            }
            let mut current = self.ready_route_shape(ready.child, ready.parent)?;
            current.epoch = ready.epoch.clone();
            (&current == ready)
                .then(|| self.ready_report_identity(ready))
                .flatten()
        })
    }

    /// Provisions a cross-recipient grant only for an accepted, current Pi
    /// bootstrap channel. The stream session supplies A; the request can name
    /// only a currently managed recipient target, never a caller, grant, or
    /// durable recipient selector.
    pub(crate) fn provision_mailbox_bootstrap_recipient(
        &mut self,
        session: &MailboxBootstrapSession,
        recipient_target: &str,
    ) -> Result<crate::mailbox::MailboxGrant, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        let sender_terminal_id = self
            .state
            .terminals
            .keys()
            .find(|terminal_id| terminal_id.to_string() == session.caller)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        let sender_terminal = self
            .state
            .terminals
            .get(sender_terminal_id)
            .ok_or(MailboxBootstrapError::GrantRevoked)?;
        if sender_terminal.managed_agent_kind() != Some(crate::detect::Agent::Pi)
            || !sender_terminal
                .accepts_managed_agent_generation(session.active_execution_generation)
            || !self.exact_active_mailbox_authority(
                &session.caller,
                session.active_execution_generation,
            )
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }

        let recipient = self
            .resolve_agent_target(recipient_target)
            .map_err(|_| MailboxBootstrapError::InvalidRequest)?;
        let recipient_terminal_id = self
            .state
            .workspaces
            .get(recipient.ws_idx)
            .and_then(|workspace| workspace.terminal_id(recipient.pane_id))
            .ok_or(MailboxBootstrapError::InvalidRequest)?;
        if recipient_terminal_id == sender_terminal_id {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        // Policy: a managed Pi may provision a send grant only along its own
        // delegation edges (its parent or a direct child). Everyone else is
        // reached through server-side routing (agent prompt, handoff).
        let sender_pane = self.state.workspaces.iter().find_map(|workspace| {
            workspace.tabs.iter().find_map(|tab| {
                tab.panes
                    .iter()
                    .find(|(_, pane)| &pane.attached_terminal_id == sender_terminal_id)
                    .map(|(pane_id, _)| *pane_id)
            })
        });
        let live = |record: &&crate::delegation::DelegationRecord| !record.tombstone;
        let sender_edge = sender_pane
            .and_then(|pane| self.state.delegations.delegation_for_pane(pane))
            .filter(live);
        let recipient_edge = self
            .state
            .delegations
            .delegation_for_pane(recipient.pane_id)
            .filter(live);
        let related = match (sender_edge, recipient_edge) {
            (Some(sender), Some(recipient)) => {
                sender.parent_id == Some(recipient.id) || recipient.parent_id == Some(sender.id)
            }
            _ => false,
        };
        if !related {
            return Err(MailboxBootstrapError::RecipientNotAllowed);
        }
        let recipient_terminal = self
            .state
            .terminals
            .get(recipient_terminal_id)
            .ok_or(MailboxBootstrapError::InvalidRequest)?;
        let recipient_generation = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &recipient_terminal_id.to_string(),
        )
        .map_err(|_| MailboxBootstrapError::GrantMissing)?
        .load()
        .map_err(|_| MailboxBootstrapError::GrantMissing)?
        .filter(|record| {
            record.authoritative()
                && recipient_terminal.managed_agent_kind().is_some()
                && recipient_terminal.accepts_managed_agent_generation(record.process_generation)
                && self.exact_active_mailbox_authority(
                    &recipient_terminal_id.to_string(),
                    record.process_generation,
                )
        })
        .map(|record| record.process_generation)
        .ok_or(MailboxBootstrapError::GrantRevoked)?;

        let recipient = crate::mailbox::RecipientKey {
            recipient_id: recipient_terminal_id.to_string(),
            // Recipient identity is stable across active executions; the
            // matching Active record above is the separate execution guard.
            generation: "1".into(),
        };
        let grant_id = self
            .provision_cross_recipient_mailbox_grant(&session.caller, recipient.clone())
            .map_err(|error| match error {
                OfflineMailboxInstallError::SenderRecordMismatch
                | OfflineMailboxInstallError::SenderRecordUnavailable => {
                    MailboxBootstrapError::GrantRevoked
                }
                OfflineMailboxInstallError::Store(_)
                | OfflineMailboxInstallError::MailboxStore(_) => {
                    MailboxBootstrapError::GrantMissing
                }
            })?;
        // Keep the execution check material to this route even though the
        // durable recipient selector intentionally remains stable.
        if !self.exact_active_mailbox_authority(&recipient.recipient_id, recipient_generation) {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(crate::mailbox::MailboxGrant {
            grant_id,
            sender: crate::mailbox::RecipientKey {
                recipient_id: session.caller.clone(),
                generation: "1".into(),
            },
            recipient,
        })
    }

    pub(crate) fn invalidate_offline_mailbox_authority_for_pane(
        &mut self,
        pane_id: crate::layout::PaneId,
        process_generation: Option<u64>,
    ) {
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane_id).cloned() else {
            return;
        };
        let sender_key = terminal_id.to_string();
        if self
            .managed_pi_launches
            .get(&terminal_id)
            .is_some_and(|launch| {
                process_generation.is_none_or(|generation| generation == launch.generation)
            })
        {
            self.managed_pi_launches.remove(&terminal_id);
        }
        let Some(authority) = self.offline_mailbox_authorities.get(&sender_key) else {
            return;
        };
        if process_generation.is_some_and(|generation| generation != authority.sender_generation) {
            return;
        }
        let sender_store = match crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &sender_key,
        ) {
            Ok(store) => store,
            Err(_) => return,
        };
        if sender_store
            .invalidate_active(&authority.sender_key, authority.sender_generation)
            .is_ok()
        {
            self.offline_mailbox_authorities.remove(&sender_key);
        }
    }

    /// Publishes discovery only after Headless owns a bound listener. A blank
    /// value is injected for Pi when unavailable so inherited/user values cannot
    /// become an authority substitute.
    pub(crate) fn publish_mailbox_bootstrap_discovery_address(
        &mut self,
        listener_path: &std::path::Path,
    ) {
        self.mailbox_bootstrap_discovery_address = listener_path
            .is_absolute()
            .then(|| listener_path.display().to_string());
        // With unmanaged Pi Messages on, every pane shell carries the
        // (authority-free) address so a typed `pi` can find the listener.
        crate::integration::set_pane_mailbox_bootstrap_address(
            self.unmanaged_pi_messages
                .then(|| self.mailbox_bootstrap_discovery_address.clone())
                .flatten(),
        );
    }

    pub(crate) fn pi_mailbox_bootstrap_launch_environment(
        &self,
        requested: &[String],
    ) -> Vec<String> {
        let mut environment = requested
            .iter()
            .filter(|entry| {
                entry
                    .split_once('=')
                    .is_none_or(|(name, _)| name != PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV)
            })
            .cloned()
            .collect::<Vec<_>>();
        // Always override inherited or client-provided discovery. Empty means
        // explicitly unavailable; Pi must not synthesize a local fallback.
        environment.push(format!(
            "{PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV}={}",
            self.mailbox_bootstrap_discovery_address
                .as_deref()
                .unwrap_or_default()
        ));
        environment
    }

    pub(crate) fn pi_mailbox_bootstrap_pane_environment(
        &self,
        requested: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        let mut environment = requested
            .into_iter()
            .filter(|(name, _)| name != PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV)
            .collect::<Vec<_>>();
        environment.push((
            PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV.into(),
            self.mailbox_bootstrap_discovery_address
                .clone()
                .unwrap_or_default(),
        ));
        environment
    }

    /// The counter is only a process-local ordinal. Both the process namespace
    /// and each accepted stream need independent kernel entropy: a fresh App
    /// must not mint the old `mailbox-binding-1` again. An ID is never a peer
    /// credential; FD and current execution/route checks remain mandatory.
    pub(crate) fn mailbox_bootstrap_binding_candidate(
        &self,
        stream_nonce: Option<&str>,
    ) -> Result<String, MailboxBootstrapError> {
        fn valid_nonce(value: &str) -> bool {
            value.len() == 32
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }
        let boot = self
            .mailbox_bootstrap_boot_nonce
            .as_deref()
            .filter(|value| valid_nonce(value))
            .ok_or(MailboxBootstrapError::GrantMissing)?;
        let nonce = stream_nonce
            .filter(|value| valid_nonce(value))
            .ok_or(MailboxBootstrapError::GrantMissing)?;
        self.next_mailbox_bootstrap_binding
            .checked_add(1)
            .ok_or(MailboxBootstrapError::GrantMissing)?;
        if self.used_mailbox_bootstrap_nonces.contains(nonce) {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        let id = format!(
            "mailbox-binding-{}-{}-{}",
            boot, self.next_mailbox_bootstrap_binding, nonce
        );
        if self.mailbox_bootstrap_bindings.contains_key(&id) {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        Ok(id)
    }

    /// Accept a bootstrap stream only by verifying it against a current live
    /// foreground Pi process and then re-reading the exact Active record before
    /// publishing a server-selected binding.
    pub(crate) fn accept_mailbox_bootstrap_stream(
        &mut self,
        socket_fd: RawFd,
    ) -> Result<MailboxBootstrapSession, MailboxBootstrapError> {
        let trusted = self.accept_trusted_mailbox_bootstrap_stream(socket_fd);
        match trusted {
            Err(MailboxBootstrapError::GrantMissing | MailboxBootstrapError::PeerRejected)
                if self.unmanaged_pi_messages =>
            {
                self.accept_recipient_only_mailbox_bootstrap_stream(socket_fd)
                    .map_err(|_| trusted.unwrap_err())
            }
            other => other,
        }
    }

    /// Recipient-only acceptance: the peer (same UID) must be, or descend
    /// from, the Pi process in a pane's current foreground job, and that pane
    /// must have no trusted managed sender authority of its own.
    fn accept_recipient_only_mailbox_bootstrap_stream(
        &mut self,
        socket_fd: RawFd,
    ) -> Result<MailboxBootstrapSession, MailboxBootstrapError> {
        let candidates: Vec<(String, u32)> = self
            .state
            .terminals
            .iter()
            .filter(|(_, terminal)| {
                terminal.effective_known_agent() == Some(crate::detect::Agent::Pi)
            })
            .filter_map(|(terminal_id, _)| {
                let key = terminal_id.to_string();
                if self.active_pi_sender_generation(&key).is_some() {
                    return None;
                }
                let job = self.mailbox_bootstrap_foreground_job(terminal_id)?;
                let (agent, process) = crate::detect::identify_agent_process_in_job(&job)?;
                (agent == crate::detect::Agent::Pi && process.pid != 0)
                    .then_some((key, process.pid))
            })
            .collect();
        for (terminal_key, foreground_pid) in candidates {
            let entropy = crate::platform::random_route_epoch();
            let binding_generation =
                self.mailbox_bootstrap_binding_candidate(entropy.as_deref())?;
            let recipient = SessionGeneration {
                session: terminal_key.clone(),
                generation: 0,
                delegation_id: crate::delegation::DelegationId::alloc()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?,
            };
            let Ok(context) = TrustedMailboxChannelContext::from_verified_local_socket(
                socket_fd,
                recipient,
                binding_generation.clone(),
                foreground_pid,
                0,
            ) else {
                continue;
            };
            let Some(birth) = self.managed_pi_process_birth(foreground_pid) else {
                continue;
            };
            self.next_mailbox_bootstrap_binding = self
                .next_mailbox_bootstrap_binding
                .checked_add(1)
                .ok_or(MailboxBootstrapError::GrantMissing)?;
            if !self
                .used_mailbox_bootstrap_nonces
                .insert(entropy.ok_or(MailboxBootstrapError::GrantMissing)?)
            {
                return Err(MailboxBootstrapError::GrantMissing);
            }
            let session = MailboxBootstrapSession {
                caller: terminal_key.clone(),
                parent_report: None,
                history_only: false,
                history_edge: None,
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: terminal_key.clone(),
                    generation: "1".into(),
                },
                grant_id: format!(
                    "recipient-only:{}",
                    self.pane_queue_key(&terminal_key)
                        .map(|key| format!("pane:{key}"))
                        .unwrap_or_else(|| terminal_key.clone())
                ),
                active_execution_generation: 0,
                binding_generation: binding_generation.clone(),
                context,
                recipient_only: Some(RecipientOnlyBinding {
                    foreground_pid,
                    start_ticks: birth.start_ticks,
                }),
                pane_inbox: self
                    .pane_queue_key(&terminal_key)
                    .map(|key| crate::app::messages::pane_recipient(&key)),
            };
            self.mailbox_bootstrap_bindings
                .insert(binding_generation, session.clone());
            self.mark_pane_messages_capable(&session.caller);
            return Ok(session);
        }
        Err(MailboxBootstrapError::PeerRejected)
    }

    /// A recipient-only session stays current only while its exact Pi
    /// execution is still the pane's foreground Pi and the switch is on.
    fn recipient_only_binding_current(&self, session: &MailboxBootstrapSession) -> bool {
        let Some(binding) = session.recipient_only else {
            return false;
        };
        self.unmanaged_pi_messages
            && self.active_pi_sender_generation(&session.caller).is_none()
            && self
                .state
                .terminals
                .keys()
                .find(|terminal_id| terminal_id.to_string() == session.caller)
                .and_then(|terminal_id| self.mailbox_bootstrap_foreground_job(terminal_id))
                .is_some_and(|job| {
                    crate::detect::identify_agent_process_in_job(&job).is_some_and(
                        |(agent, process)| {
                            agent == crate::detect::Agent::Pi
                                && process.pid == binding.foreground_pid
                        },
                    )
                })
            && self
                .managed_pi_process_birth(binding.foreground_pid)
                .is_some_and(|birth| birth.start_ticks == binding.start_ticks)
    }

    fn mark_pane_messages_capable(&mut self, terminal_key: &str) {
        // Remember that this exact Pi process attached (the typed fallback
        // never applies to it, even while its stream reconnects).
        if let Some(pi) = self.foreground_pi_identity(terminal_key) {
            self.messages_attached_pis.insert(pi, None);
        }
        let mut flipped = false;
        if let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == terminal_key)
        {
            if !terminal.messages_capable {
                terminal.messages_capable = true;
                flipped = true;
            }
        }
        if flipped {
            // Durable at once, not on the debounced save: after a crash the
            // pane must still queue for its next Pi.
            self.state.mark_session_dirty();
            if self.no_session {
                return;
            }
            if let Err(err) = self.durably_save_delegation_edge() {
                tracing::warn!(terminal = terminal_key, %err, "messages: could not persist the Messages-capable pane at once; the next session save will");
            }
        }
    }

    /// The pane's current foreground Pi process as (PID, birth tick).
    pub(crate) fn foreground_pi_identity(&self, terminal_key: &str) -> Option<(u32, u64)> {
        let terminal_id = self
            .state
            .terminals
            .keys()
            .find(|terminal_id| terminal_id.to_string() == terminal_key)?;
        let job = self.mailbox_bootstrap_foreground_job(terminal_id)?;
        let (agent, process) = crate::detect::identify_agent_process_in_job(&job)?;
        if agent != crate::detect::Agent::Pi || process.pid == 0 {
            return None;
        }
        let pid = process.pid;
        let birth = self.managed_pi_process_birth(pid)?;
        Some((pid, birth.start_ticks))
    }

    /// Releases the binding of a closed bootstrap stream so "has Messages"
    /// reflects live connections only.
    pub(crate) fn release_mailbox_bootstrap_binding(&mut self, binding_generation: &str) {
        let Some(session) = self.mailbox_bootstrap_bindings.remove(binding_generation) else {
            return;
        };
        // The pane's Pi closed its last stream: remember when, so a Pi whose
        // Messages extension died falls back to typed input after 30 s.
        let still_open = self
            .mailbox_bootstrap_bindings
            .values()
            .any(|other| other.caller == session.caller);
        if !still_open {
            let pi = session
                .recipient_only
                .map(|binding| (binding.foreground_pid, binding.start_ticks))
                .or_else(|| self.foreground_pi_identity(&session.caller));
            if let Some(closed) = pi.and_then(|pi| self.messages_attached_pis.get_mut(&pi)) {
                closed.get_or_insert_with(std::time::Instant::now);
            }
        }
    }

    fn accept_trusted_mailbox_bootstrap_stream(
        &mut self,
        socket_fd: RawFd,
    ) -> Result<MailboxBootstrapSession, MailboxBootstrapError> {
        let candidates = self.live_mailbox_bootstrap_candidates();
        if candidates.is_empty() {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        for candidate in candidates {
            let entropy = crate::platform::random_route_epoch();
            let binding_generation =
                self.mailbox_bootstrap_binding_candidate(entropy.as_deref())?;
            let recipient = SessionGeneration {
                session: candidate.sender_key.clone(),
                generation: candidate.process_generation,
                delegation_id: crate::delegation::DelegationId::alloc()
                    .map_err(|_| MailboxBootstrapError::GrantMissing)?,
            };
            let context = match TrustedMailboxChannelContext::from_verified_local_socket(
                socket_fd,
                recipient,
                binding_generation.clone(),
                candidate.foreground_pi_pid,
                candidate.process_generation,
            ) {
                Ok(context) => context,
                Err(_) => continue,
            };
            // The accepted FD was authenticated. Recheck the exact persisted
            // Active execution now, before exposing its descriptor.
            if candidate.history_only {
                if self.active_pi_sender_generation(&candidate.sender_key)
                    != Some(candidate.process_generation)
                {
                    continue;
                }
            } else if !self
                .exact_active_mailbox_authority(&candidate.sender_key, candidate.process_generation)
            {
                continue;
            }
            self.next_mailbox_bootstrap_binding = self
                .next_mailbox_bootstrap_binding
                .checked_add(1)
                .ok_or(MailboxBootstrapError::GrantMissing)?;
            if !self
                .used_mailbox_bootstrap_nonces
                .insert(entropy.ok_or(MailboxBootstrapError::GrantMissing)?)
            {
                return Err(MailboxBootstrapError::GrantMissing);
            }
            let parent_report = (!candidate.history_only)
                .then(|| {
                    self.bound_parent_report_candidate(
                        &candidate.sender_key,
                        candidate.process_generation,
                    )
                })
                .flatten();
            if let Some(route) = &parent_report {
                self.provision_cross_recipient_mailbox_grant_with_id(
                    &candidate.sender_key,
                    route.recipient.clone(),
                    route.grant_id.clone(),
                )
                .map_err(|_| MailboxBootstrapError::GrantMissing)?;
            }
            let session = MailboxBootstrapSession {
                caller: candidate.sender_key.clone(),
                parent_report,
                history_only: candidate.history_only,
                history_edge: self.history_delegation_edge(&candidate.sender_key),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: candidate.sender_key.clone(),
                    generation: "1".into(),
                },
                grant_id: if candidate.history_only {
                    String::new()
                } else {
                    format!(
                        "offline:{}:{}",
                        candidate.sender_key, candidate.process_generation
                    )
                },
                active_execution_generation: candidate.process_generation,
                binding_generation: binding_generation.clone(),
                context,
                recipient_only: None,
                pane_inbox: self
                    .pane_queue_key(&candidate.sender_key)
                    .map(|key| crate::app::messages::pane_recipient(&key)),
            };
            self.mailbox_bootstrap_bindings
                .insert(binding_generation, session.clone());
            self.mark_pane_messages_capable(&session.caller);
            return Ok(session);
        }
        Err(MailboxBootstrapError::PeerRejected)
    }

    pub(crate) fn mailbox_bootstrap_session_current(
        &self,
        session: &MailboxBootstrapSession,
    ) -> Result<(), MailboxBootstrapError> {
        let Some(issued) = self
            .mailbox_bootstrap_bindings
            .get(&session.binding_generation)
        else {
            return Err(MailboxBootstrapError::GrantMissing);
        };
        if session.recipient_only.is_some() {
            return if issued == session
                && issued.context().binding().id() == session.binding_generation
                && self.recipient_only_binding_current(session)
            {
                Ok(())
            } else {
                Err(MailboxBootstrapError::GrantRevoked)
            };
        }
        if issued != session
            || issued.context().binding().id() != session.binding_generation
            || (session.history_only
                && (self.active_pi_sender_generation(&session.caller)
                    != Some(session.active_execution_generation)
                    || self.history_delegation_edge(&session.caller) != session.history_edge
                    || !self.history_foreground_matches(session)))
            || (!session.history_only
                && !self.exact_active_mailbox_authority(
                    &session.caller,
                    session.active_execution_generation,
                ))
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(())
    }

    pub(crate) fn exact_active_mailbox_authority_for(
        &self,
        sender_key: &str,
        generation: u64,
    ) -> bool {
        self.exact_active_mailbox_authority(sender_key, generation)
    }

    fn exact_active_mailbox_authority(&self, sender_key: &str, generation: u64) -> bool {
        let Some(authority) = self.offline_mailbox_authorities.get(sender_key) else {
            return false;
        };
        if authority.sender_generation != generation {
            return false;
        }
        crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            sender_key,
        )
        .and_then(|store| store.load())
        .ok()
        .flatten()
        .is_some_and(|record| authority.matches_sender_record(&record))
    }

    fn active_pi_sender_generation(&self, sender_key: &str) -> Option<u64> {
        let terminal_id = self
            .state
            .terminals
            .keys()
            .find(|terminal_id| terminal_id.to_string() == sender_key)?;
        let terminal = self.state.terminals.get(terminal_id)?;
        let record = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            sender_key,
        )
        .ok()?
        .load()
        .ok()??;
        (record.authoritative()
            && record.sender_key == sender_key
            && terminal.managed_agent_kind() == Some(crate::detect::Agent::Pi)
            && terminal.accepts_managed_agent_generation(record.process_generation))
        .then_some(record.process_generation)
    }

    fn history_delegation_edge(
        &self,
        sender_key: &str,
    ) -> Option<(
        crate::delegation::DelegationId,
        Option<crate::delegation::DelegationId>,
    )> {
        self.state
            .delegations
            .records()
            .values()
            .find_map(|record| {
                let pane = record.pane_id?;
                let (ws_idx, _) = self.find_pane(pane)?;
                (self.state.workspaces[ws_idx].terminal_id(pane)?.to_string() == sender_key)
                    .then_some((record.id, record.parent_id))
            })
    }

    fn history_foreground_matches(&self, session: &MailboxBootstrapSession) -> bool {
        self.state
            .terminals
            .keys()
            .find(|terminal_id| terminal_id.to_string() == session.caller)
            .and_then(|terminal_id| self.mailbox_bootstrap_foreground_job(terminal_id))
            .is_some_and(|job| {
                job.processes
                    .iter()
                    .any(|process| process.pid == session.context.foreground_pi_pid())
            })
    }

    pub(crate) fn mailbox_bootstrap_history_snapshot(
        &self,
        session: &MailboxBootstrapSession,
        protocol: &str,
    ) -> Result<crate::mailbox_v1::Snapshot, MailboxBootstrapError> {
        self.mailbox_bootstrap_session_current(session)?;
        if protocol != crate::mailbox_v1::PROTOCOL {
            return Err(MailboxBootstrapError::InvalidRequest);
        }
        if session.recipient_only.is_none()
            && (self.history_delegation_edge(&session.caller) != session.history_edge
                || self.active_pi_sender_generation(&session.caller)
                    != Some(session.active_execution_generation)
                || !self.history_foreground_matches(session))
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir)
            .map_err(|_| MailboxBootstrapError::GrantMissing)?;
        let recovered = store
            .load()
            .map_err(|_| MailboxBootstrapError::GrantMissing)?;
        // The whole pane inbox; withdrawn (dropped) heads are never history.
        let current = self.current_agent_session_value(&session.caller);
        let mut snapshot = crate::app::messages::inbox_snapshot(
            &recovered,
            &self.inbox_recipients(&session.caller),
            &crate::app::messages::session_execution(session),
            current.as_deref(),
            &|execution| self.execution_alive(execution, &session.caller),
        )
        .map_err(|_| MailboxBootstrapError::GrantMissing)?;
        let settled: std::collections::HashSet<_> = snapshot
            .head_states
            .iter()
            .filter(|state| state.lifecycle == crate::mailbox_v1::HeadLifecycle::Settled)
            .map(|state| state.stable_id.clone())
            .collect();
        // The view cannot authorize current work: it carries only exact terminal
        // heads and their receipts, not a claim or any unresolved head.
        snapshot
            .heads
            .retain(|head| settled.contains(&head.stable_id));
        snapshot
            .receipts
            .retain(|receipt| settled.contains(&receipt.stable_id));
        snapshot
            .head_states
            .retain(|state| settled.contains(&state.stable_id));
        snapshot.claim = None;
        if snapshot.heads.iter().any(|head| {
            !snapshot.receipts.iter().any(|receipt| {
                receipt.stable_id == head.stable_id
                    && receipt.revision == head.revision
                    && receipt.digest == head.digest
                    && receipt.delivery_digest == head.delivery_digest
            })
        }) {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        Ok(snapshot)
    }

    fn live_mailbox_bootstrap_candidates(&self) -> Vec<LiveMailboxBootstrapCandidate> {
        self.state
            .terminals
            .keys()
            .filter_map(|terminal_id| {
                let sender_key = terminal_id.to_string();
                // Persisted Active generation, managed terminal generation and
                // foreground process must all match before any history scope.
                let process_generation = self.active_pi_sender_generation(&sender_key)?;
                let history_only =
                    !self.exact_active_mailbox_authority(&sender_key, process_generation);
                if history_only {
                    let recipient = crate::mailbox::RecipientKey {
                        recipient_id: sender_key.clone(),
                        generation: "1".into(),
                    };
                    let store =
                        crate::mailbox::MailboxStore::open(&self.sender_authority_dir).ok()?;
                    let snapshot =
                        crate::mailbox_v1::snapshot(&store.load().ok()?, &recipient).ok()?;
                    if !snapshot
                        .head_states
                        .iter()
                        .any(|state| state.lifecycle == crate::mailbox_v1::HeadLifecycle::Settled)
                    {
                        return None;
                    }
                }
                let job = self.mailbox_bootstrap_foreground_job(terminal_id)?;
                let foreground_pi_pid = crate::detect::identify_agent_process_in_job(&job)
                    .and_then(|(agent, process)| {
                        (agent == crate::detect::Agent::Pi).then_some(process.pid)
                    })
                    // Preserve the explicit per-process launch marker as a
                    // fallback for wrappers whose argv cannot be classified.
                    .or_else(|| {
                        job.processes.iter().find_map(|process| {
                            (crate::platform::process_agent_hint(process.pid)
                                == Some(crate::detect::Agent::Pi))
                            .then_some(process.pid)
                        })
                    })?;
                Some(LiveMailboxBootstrapCandidate {
                    sender_key,
                    process_generation,
                    foreground_pi_pid,
                    history_only,
                })
            })
            .collect()
    }

    /// Whether the Messages bootstrap would currently accept this sender, and
    /// with which generation and scope (`history_only`).
    #[cfg(test)]
    pub(crate) fn live_mailbox_bootstrap_candidate_for_test(
        &self,
        sender_key: &str,
    ) -> Option<(u64, bool)> {
        self.live_mailbox_bootstrap_candidates()
            .into_iter()
            .find(|candidate| candidate.sender_key == sender_key)
            .map(|candidate| (candidate.process_generation, candidate.history_only))
    }

    pub(crate) fn mailbox_bootstrap_foreground_job(
        &self,
        terminal_id: &crate::terminal::TerminalId,
    ) -> Option<crate::platform::ForegroundJob> {
        #[cfg(test)]
        if let Some(job) = self.mailbox_bootstrap_test_foreground_jobs.get(terminal_id) {
            return Some(job.clone());
        }
        let runtime = self.terminal_runtimes.get(terminal_id)?;
        crate::detect::foreground_job(runtime.child_pid()?)
    }

    #[cfg(test)]
    pub(crate) fn install_mailbox_bootstrap_test_foreground_job(
        &mut self,
        terminal_id: crate::terminal::TerminalId,
        job: crate::platform::ForegroundJob,
    ) {
        self.mailbox_bootstrap_test_foreground_jobs
            .insert(terminal_id, job);
    }

    pub(crate) fn promote_and_install_offline_mailbox_authority(
        &mut self,
        pane_id: crate::layout::PaneId,
        agent: crate::detect::Agent,
        process_generation: u64,
    ) {
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane_id).cloned() else {
            return;
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return;
        };
        if terminal.managed_agent_kind() != Some(agent)
            || !terminal.accepts_managed_agent_generation(process_generation)
        {
            return;
        }
        let sender_store = match crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &terminal_id.to_string(),
        ) {
            Ok(store) => store,
            Err(_) => return,
        };
        let record = match sender_store.promote_active(&terminal_id.to_string(), process_generation)
        {
            Ok(record) => record,
            Err(_) => return,
        };
        if agent == crate::detect::Agent::Pi {
            self.bind_active_managed_pi_process(&terminal_id, process_generation);
        }
        self.resolve_pane_wake_on_active(&terminal_id, process_generation);
        let _ = self.install_offline_mailbox_authority(record);
        if !self.pending_route_carries.is_empty() {
            self.retry_route_carries(std::time::Instant::now());
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LiveMailboxBootstrapCandidate {
    sender_key: String,
    process_generation: u64,
    foreground_pi_pid: u32,
    history_only: bool,
}

#[cfg(test)]
mod binding_tests {
    use super::*;

    #[test]
    fn bootstrap_binding_entropy_and_collision_fail_closed() {
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        app.mailbox_bootstrap_boot_nonce = Some("a".repeat(32));
        let stream = "b".repeat(32);
        let first = app
            .mailbox_bootstrap_binding_candidate(Some(&stream))
            .unwrap();
        assert_eq!(
            first,
            format!("mailbox-binding-{}-1-{}", "a".repeat(32), stream)
        );
        assert!(
            app.mailbox_bootstrap_bindings.is_empty(),
            "mint candidate is not admission"
        );
        app.mailbox_bootstrap_boot_nonce = None;
        assert!(app
            .mailbox_bootstrap_binding_candidate(Some(&stream))
            .is_err());
        app.mailbox_bootstrap_boot_nonce = Some("A".repeat(32));
        assert!(app
            .mailbox_bootstrap_binding_candidate(Some(&stream))
            .is_err());
        app.mailbox_bootstrap_boot_nonce = Some("a".repeat(32));
        assert!(app.mailbox_bootstrap_binding_candidate(None).is_err());
        assert!(app.mailbox_bootstrap_binding_candidate(Some("0")).is_err());
        app.used_mailbox_bootstrap_nonces.insert(stream.clone());
        assert!(app
            .mailbox_bootstrap_binding_candidate(Some(&stream))
            .is_err());
        app.used_mailbox_bootstrap_nonces.clear();
        app.next_mailbox_bootstrap_binding = u64::MAX;
        assert!(app
            .mailbox_bootstrap_binding_candidate(Some(&stream))
            .is_err());
    }
}
