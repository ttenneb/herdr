use std::collections::BTreeMap;
use std::os::fd::RawFd;

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

    pub(crate) fn edit(
        &self,
        params: crate::api::schema::MailboxEditParams,
    ) -> Result<crate::mailbox_v1::Snapshot, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Edit(params.edit.clone()))
            .map_err(OfflineMailboxError::Transport)?;
        let capability =
            self.capability_for(&params.caller, &params.grant_id, &params.recipient)?;
        crate::mailbox_v1::edit_unclaimed(&self.store, params.edit)
            .map_err(OfflineMailboxError::Store)?;
        // Read from the durable stream after the edit's sync before responding;
        // callers receive server authority rather than an optimistic local edit.
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
                let terminal = self.state.terminals.get(terminal_id)?;
                // The Active record is necessary but not sufficient: it must
                // still belong to this exact managed Pi launch.
                if terminal.managed_agent_kind() != Some(crate::detect::Agent::Pi)
                    || !terminal.accepts_managed_agent_generation(authority.sender_generation)
                {
                    return None;
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
                    sender_key: authority.sender_key.clone(),
                    process_generation: authority.sender_generation,
                    foreground_pi_pid,
                })
            })
            .collect()
    }

    fn mailbox_bootstrap_foreground_job(
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
        let _ = self.install_offline_mailbox_authority(record);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LiveMailboxBootstrapCandidate {
    sender_key: String,
    process_generation: u64,
    foreground_pi_pid: u32,
}
