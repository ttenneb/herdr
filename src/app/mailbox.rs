use std::collections::BTreeMap;
use std::os::fd::RawFd;

use crate::api::schema::MailboxOfflineSubmitParams;
use crate::app::App;
use crate::direct_transport::TransportError;
use crate::direct_transport::{SessionGeneration, TrustedMailboxChannelContext};

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
    context: TrustedMailboxChannelContext,
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

    pub(crate) fn claim(
        &self,
        params: crate::api::schema::MailboxClaimParams,
    ) -> Result<Option<crate::mailbox::Claim>, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Claim(params.claim))
            .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        self.store
            .claim_next(&capability.recipient)
            .map_err(OfflineMailboxError::Store)
    }

    pub(crate) fn snapshot(
        &self,
        params: crate::api::schema::MailboxSnapshotParams,
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
        Ok(crate::mailbox_v1::snapshot(
            &recovered,
            &capability.recipient,
        ))
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
        if !self.offline_mailbox_authority_current(sender_key)? {
            return Err(OfflineMailboxInstallError::SenderRecordMismatch);
        }
        let sender = crate::mailbox::RecipientKey {
            recipient_id: sender_key.into(),
            generation: "1".into(),
        };
        let grant_id = format!(
            "mailbox:{}:{}:{}:{}",
            sender.recipient_id, sender.generation, recipient.recipient_id, recipient.generation
        );
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

    pub(crate) fn invalidate_offline_mailbox_authority_for_pane(
        &mut self,
        pane_id: crate::layout::PaneId,
    ) {
        let Some((ws_idx, _)) = self.find_pane(pane_id) else {
            return;
        };
        let Some(terminal_id) = self.state.workspaces[ws_idx].terminal_id(pane_id).cloned() else {
            return;
        };
        let sender_key = terminal_id.to_string();
        let Some(authority) = self.offline_mailbox_authorities.get(&sender_key) else {
            return;
        };
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

    /// Accept a bootstrap stream only by verifying it against a current live
    /// foreground Pi process and then re-reading the exact Active record before
    /// publishing a server-selected binding.
    pub(crate) fn accept_mailbox_bootstrap_stream(
        &mut self,
        socket_fd: RawFd,
    ) -> Result<MailboxBootstrapSession, MailboxBootstrapError> {
        let candidates = self.live_mailbox_bootstrap_candidates();
        if candidates.is_empty() {
            return Err(MailboxBootstrapError::GrantMissing);
        }
        for candidate in candidates {
            let binding_generation =
                format!("mailbox-binding-{}", self.next_mailbox_bootstrap_binding);
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
            if !self
                .exact_active_mailbox_authority(&candidate.sender_key, candidate.process_generation)
            {
                continue;
            }
            self.next_mailbox_bootstrap_binding = self
                .next_mailbox_bootstrap_binding
                .checked_add(1)
                .ok_or(MailboxBootstrapError::GrantMissing)?;
            let session = MailboxBootstrapSession {
                caller: candidate.sender_key.clone(),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: candidate.sender_key.clone(),
                    generation: "1".into(),
                },
                grant_id: format!(
                    "offline:{}:{}",
                    candidate.sender_key, candidate.process_generation
                ),
                active_execution_generation: candidate.process_generation,
                binding_generation: binding_generation.clone(),
                context,
            };
            self.mailbox_bootstrap_bindings
                .insert(binding_generation, session.clone());
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
        if issued != session
            || issued.context().binding().id() != session.binding_generation
            || !self.exact_active_mailbox_authority(
                &session.caller,
                session.active_execution_generation,
            )
        {
            return Err(MailboxBootstrapError::GrantRevoked);
        }
        Ok(())
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

    fn live_mailbox_bootstrap_candidates(&self) -> Vec<LiveMailboxBootstrapCandidate> {
        #[cfg(test)]
        if !self.mailbox_bootstrap_test_candidates.is_empty() {
            return self.mailbox_bootstrap_test_candidates.clone();
        }
        self.offline_mailbox_authorities
            .values()
            .filter_map(|authority| {
                if !self.exact_active_mailbox_authority(
                    &authority.sender_key,
                    authority.sender_generation,
                ) {
                    return None;
                }
                let terminal_id = self
                    .state
                    .terminals
                    .keys()
                    .find(|terminal_id| terminal_id.to_string() == authority.sender_key)?;
                let runtime = self.terminal_runtimes.get(terminal_id)?;
                let job = crate::detect::foreground_job(runtime.child_pid()?)?;
                let foreground_pi_pid = job
                    .processes
                    .iter()
                    .find(|process| {
                        crate::platform::process_agent_hint(process.pid)
                            == Some(crate::detect::Agent::Pi)
                    })
                    .map(|process| process.pid)?;
                Some(LiveMailboxBootstrapCandidate {
                    sender_key: authority.sender_key.clone(),
                    process_generation: authority.sender_generation,
                    foreground_pi_pid,
                })
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn install_mailbox_bootstrap_test_candidate(
        &mut self,
        sender_key: String,
        process_generation: u64,
        foreground_pi_pid: u32,
    ) {
        self.mailbox_bootstrap_test_candidates
            .push(LiveMailboxBootstrapCandidate {
                sender_key,
                process_generation,
                foreground_pi_pid,
            });
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
        let _ = self.install_offline_mailbox_authority(record);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LiveMailboxBootstrapCandidate {
    sender_key: String,
    process_generation: u64,
    foreground_pi_pid: u32,
}
