use std::collections::BTreeMap;

use crate::api::schema::MailboxOfflineSubmitParams;
use crate::app::App;
use crate::direct_transport::TransportError;

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
    ) -> Option<Self> {
        if !record.authoritative() {
            return None;
        }
        let recipient = crate::mailbox::RecipientKey {
            recipient_id: record.sender_key.clone(),
            generation: record.process_generation.to_string(),
        };
        let grant_id = format!(
            "offline:{}:{}",
            record.sender_key, record.process_generation
        );
        let capability = OfflineMailboxCapability {
            grant_id: grant_id.clone(),
            recipient,
        };
        Some(Self {
            sender_key: record.sender_key.clone(),
            sender_generation: record.process_generation,
            caller_selector: record.sender_key.clone(),
            capabilities: BTreeMap::from([(grant_id, capability)]),
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
        let receipt = crate::mailbox_v1::submit_offline(
            &self.store,
            capability.recipient.clone(),
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
    /// Installs a route only when its caller is the current authoritative
    /// sender record. The lifecycle caller supplies the already-promoted record;
    /// this method mints a server-owned capability without recipient attachment.
    pub(crate) fn install_offline_mailbox_authority(
        &mut self,
        record: crate::sender_authority::SenderAuthorityRecord,
    ) -> Result<(), OfflineMailboxInstallError> {
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::open(&self.sender_authority_dir)
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
        let authority = OfflineMailboxAuthority::from_active_sender(&record, store)
            .ok_or(OfflineMailboxInstallError::SenderRecordMismatch)?;
        self.offline_mailbox_authority = Some(authority);
        Ok(())
    }

    pub(crate) fn offline_mailbox_authority_current(
        &self,
    ) -> Result<bool, OfflineMailboxInstallError> {
        let Some(authority) = self.offline_mailbox_authority.as_ref() else {
            return Ok(false);
        };
        let store = crate::sender_authority::SenderAuthorityStore::open(&self.sender_authority_dir)
            .map_err(OfflineMailboxInstallError::Store)?;
        Ok(store
            .load()
            .map_err(OfflineMailboxInstallError::Store)?
            .is_some_and(|record| authority.matches_sender_record(&record)))
    }

    /// Called only by the App event path after it verified the current terminal
    /// and generation. A stale/replayed detector event cannot promote a record.
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
        let Some(authority) = self.offline_mailbox_authority.as_ref() else {
            return;
        };
        if authority.sender_key != terminal_id.to_string() {
            return;
        }
        let sender_store =
            match crate::sender_authority::SenderAuthorityStore::open(&self.sender_authority_dir) {
                Ok(store) => store,
                Err(_) => return,
            };
        if sender_store
            .invalidate_active(&authority.sender_key, authority.sender_generation)
            .is_ok()
        {
            self.offline_mailbox_authority = None;
        }
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
        let sender_store =
            match crate::sender_authority::SenderAuthorityStore::open(&self.sender_authority_dir) {
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
