use crate::api::schema::MailboxOfflineSubmitParams;
use crate::app::App;
use crate::direct_transport::{
    AuthorityContext, Effect, GrantAuthority, MessageKind, RecipientChannelRegistry, RouteScope,
    SessionGeneration, TransportError,
};

/// Server-injected local authority for offline mailbox admission. None of these
/// fields are decoded from the mailbox wire request.
pub(crate) struct OfflineMailboxAuthority {
    /// Exact durable sender record that installed this server-local route.
    pub(crate) sender_key: String,
    pub(crate) sender_generation: u64,
    pub(crate) caller_selector: String,
    pub(crate) caller: SessionGeneration,
    pub(crate) scope: RouteScope,
    pub(crate) recipients: RecipientChannelRegistry,
    pub(crate) grants: GrantAuthority,
    pub(crate) store: crate::mailbox::MailboxStore,
    pub(crate) now: u64,
}

#[derive(Debug)]
pub(crate) enum OfflineMailboxInstallError {
    SenderRecordUnavailable,
    SenderRecordMismatch,
    Store(std::io::Error),
}

pub(crate) enum OfflineMailboxError {
    CallerMismatch,
    Transport(TransportError),
    Store(crate::mailbox::MailboxError),
    ReceiptMissing,
}

impl App {
    /// Installs a route only when its caller is the current authoritative
    /// sender record. The server must construct the route from its verified
    /// local channel and canonical grant state before calling this method.
    pub(crate) fn install_offline_mailbox_authority(
        &mut self,
        authority: OfflineMailboxAuthority,
    ) -> Result<(), OfflineMailboxInstallError> {
        let store = crate::sender_authority::SenderAuthorityStore::open(&self.sender_authority_dir)
            .map_err(OfflineMailboxInstallError::Store)?;
        let record = store
            .load()
            .map_err(OfflineMailboxInstallError::Store)?
            .ok_or(OfflineMailboxInstallError::SenderRecordUnavailable)?;
        if !authority.matches_sender_record(&record) {
            return Err(OfflineMailboxInstallError::SenderRecordMismatch);
        }
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
}

impl OfflineMailboxAuthority {
    pub(crate) fn matches_sender_record(
        &self,
        record: &crate::sender_authority::SenderAuthorityRecord,
    ) -> bool {
        record.authoritative()
            && record.sender_key == self.sender_key
            && record.process_generation == self.sender_generation
    }

    pub(crate) fn submit(
        &mut self,
        params: MailboxOfflineSubmitParams,
    ) -> Result<crate::mailbox::AdmissionReceipt, OfflineMailboxError> {
        crate::mailbox_v1::validate_request(&crate::mailbox_v1::Request::Submit(
            params.submit.clone(),
        ))
        .map_err(OfflineMailboxError::Transport)?;
        if params.caller != self.caller_selector {
            return Err(OfflineMailboxError::CallerMismatch);
        }
        let recipient = self
            .recipients
            .resolve_mailbox_recipient(&params.recipient)
            .map_err(OfflineMailboxError::Transport)?;
        let authority_context = AuthorityContext {
            issuer: self.caller.clone(),
            recipient: recipient.clone(),
            scope: self.scope.clone(),
            topology_revision: 0,
            grant_revision: self.grants.revision(),
            connected: true,
            authority_confirmed: true,
        };
        let selector = params.recipient;
        let grant_id = params.grant_id;
        let submit = params.submit;
        let now = self.now;
        let recipients = &self.recipients;
        let grants = &mut self.grants;
        recipients
            .with_authenticated_route(&recipient, now, |_, manifests, topology, route| {
                let mut context = authority_context;
                context.topology_revision = topology.revision();
                crate::mailbox_v1::authorize(
                    grants,
                    manifests,
                    &grant_id,
                    &context,
                    &route,
                    &recipient,
                    Effect::Send,
                    Some(MessageKind::Report),
                    now,
                )
                .map(|_| ())
            })
            .map_err(OfflineMailboxError::Transport)?;
        let receipt = crate::mailbox_v1::submit_offline(&self.store, selector, submit)
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
}
