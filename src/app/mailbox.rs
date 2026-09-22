use crate::api::schema::MailboxOfflineSubmitParams;
use crate::direct_transport::{
    AuthorityContext, Effect, GrantAuthority, MessageKind, RecipientChannelRegistry, RouteScope,
    SessionGeneration, TransportError,
};

/// Server-injected local authority for offline mailbox admission. None of these
/// fields are decoded from the mailbox wire request.
pub(crate) struct OfflineMailboxAuthority {
    pub(crate) caller_selector: String,
    pub(crate) caller: SessionGeneration,
    pub(crate) scope: RouteScope,
    pub(crate) recipients: RecipientChannelRegistry,
    pub(crate) grants: GrantAuthority,
    pub(crate) store: crate::mailbox::MailboxStore,
    pub(crate) now: u64,
}

pub(crate) enum OfflineMailboxError {
    CallerMismatch,
    Transport(TransportError),
    Store(crate::mailbox::MailboxError),
    ReceiptMissing,
}

impl OfflineMailboxAuthority {
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
