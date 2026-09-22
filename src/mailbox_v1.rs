//! JSON codec and fail-closed authorization seam for the Pi `mailbox.v1` projection.
//!
//! This module is deliberately transport-free: attachment metadata is not authority and no
//! request can produce a Pi notification. The caller must obtain an authenticated direct
//! transport route before calling the `GrantAuthority` adapter below.

use serde::{Deserialize, Serialize};

use crate::direct_transport::{
    AuthorityContext, DeliveryRecord, Effect, GrantAuthority, GrantTicket, ManifestRegistry,
    MessageKind, NegotiatedRoute, SenderDeliveryJournal, SessionGeneration, TransportError,
};
use crate::mailbox::{AdmissionReceipt, Claim, MailboxHead, RecipientKey, RecoveredMailbox};

pub const PROTOCOL: &str = "mailbox.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Submit {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub attachment: Attachment,
    pub head: MailboxHead,
    pub receipt: AdmissionReceipt,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Get {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub delivery_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub attachment: Attachment,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimRequest {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub attachment: Attachment,
    pub claim: Claim,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolve {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub attachment: Attachment,
    pub claim_id: String,
    pub outcome: ResolveOutcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveOutcome {
    Admitted,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Submit(Submit),
    Get(Get),
    List(List),
    Claim(ClaimRequest),
    Resolve(Resolve),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub heads: Vec<MailboxHead>,
    pub receipts: Vec<AdmissionReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
}

pub fn snapshot(recovered: &RecoveredMailbox, recipient: &RecipientKey) -> Snapshot {
    let heads = recovered
        .heads
        .values()
        .filter(|head| &head.recipient == recipient)
        .cloned()
        .collect();
    let receipts = recovered
        .receipts
        .values()
        .filter(|receipt| {
            recovered
                .heads
                .get(&receipt.stable_id)
                .is_some_and(|head| &head.recipient == recipient)
        })
        .cloned()
        .collect();
    let claim = recovered
        .claims
        .values()
        .find(|claim| &claim.recipient == recipient)
        .cloned();
    Snapshot {
        heads,
        receipts,
        claim,
    }
}

pub fn validate_request(request: &Request) -> Result<(), TransportError> {
    let valid_identity =
        |protocol: &str, recipient: &RecipientKey, attachment: Option<&Attachment>| {
            protocol == PROTOCOL
                && !recipient.recipient_id.is_empty()
                && !recipient.generation.is_empty()
                && attachment.is_none_or(|value| !value.session_id.is_empty())
        };
    match request {
        Request::Submit(value) => {
            if !valid_identity(&value.protocol, &value.recipient, Some(&value.attachment)) {
                return Err(TransportError::InvalidSchema);
            }
            if value.head.recipient != value.recipient
                || value.receipt.delivery_digest != value.head.delivery_digest
            {
                return Err(TransportError::ReceiptMismatch);
            }
        }
        Request::Get(value)
            if !valid_identity(&value.protocol, &value.recipient, None)
                || value.delivery_digest.len() != 64 =>
        {
            return Err(TransportError::InvalidSchema)
        }
        Request::List(value)
            if !valid_identity(&value.protocol, &value.recipient, Some(&value.attachment)) =>
        {
            return Err(TransportError::InvalidSchema)
        }
        Request::Claim(value) => {
            if !valid_identity(&value.protocol, &value.recipient, Some(&value.attachment)) {
                return Err(TransportError::InvalidSchema);
            }
            if value.claim.recipient != value.recipient {
                return Err(TransportError::GrantScopeMismatch);
            }
        }
        Request::Resolve(value)
            if !valid_identity(&value.protocol, &value.recipient, Some(&value.attachment))
                || value.claim_id.is_empty() =>
        {
            return Err(TransportError::InvalidSchema)
        }
        _ => {}
    }
    Ok(())
}

/// Binds mailbox operations to the existing grant/topology/manifest checks. It intentionally
/// accepts only authenticated server-side context; wire attachments cannot manufacture one.
/// Creates the existing sender-side idempotency record before any future transport attempt.
/// Callers persist `journal.snapshot()` with their server mailbox state before sending.
pub fn begin_sender_delivery(
    journal: &mut SenderDeliveryJournal,
    message_id: String,
    correlation_id: String,
    payload_digest: String,
    recipient: SessionGeneration,
    created_at: u64,
) -> Result<(), TransportError> {
    journal.begin(DeliveryRecord {
        message_id,
        correlation_id,
        payload_digest,
        recipient,
        created_at,
        transport_accepted_at: None,
        resolution: None,
    })
}

pub fn authorize(
    grants: &mut GrantAuthority,
    manifests: &ManifestRegistry,
    grant_id: &str,
    context: &AuthorityContext,
    route: &NegotiatedRoute,
    recipient: &SessionGeneration,
    effect: Effect,
    kind: Option<MessageKind>,
    now: u64,
) -> Result<GrantTicket, TransportError> {
    let ticket = grants.ticket(grant_id, context, route, manifests, effect, kind, now)?;
    if &ticket.recipient != recipient {
        return Err(TransportError::GrantScopeMismatch);
    }
    Ok(ticket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::{MailboxHead, ReceiptStatus};
    fn recipient() -> RecipientKey {
        RecipientKey {
            recipient_id: "r".into(),
            generation: "g".into(),
        }
    }
    fn head() -> MailboxHead {
        MailboxHead {
            stable_id: "s".into(),
            revision: 1,
            digest: "a".repeat(64),
            delivery_digest: "b".repeat(64),
            recipient: recipient(),
            subject: "x".into(),
            body: "y".into(),
        }
    }
    #[test]
    fn codec_rejects_wrong_protocol_and_cross_recipient_submit() {
        let h = head();
        let r = AdmissionReceipt {
            delivery_digest: h.delivery_digest.clone(),
            stable_id: h.stable_id.clone(),
            revision: 1,
            digest: h.digest.clone(),
            status: ReceiptStatus::Admitted,
        };
        let request = Request::Submit(Submit {
            protocol: PROTOCOL.into(),
            recipient: h.recipient.clone(),
            attachment: Attachment {
                session_id: "p".into(),
                session_file: None,
            },
            head: h.clone(),
            receipt: r,
        });
        assert!(validate_request(&request).is_ok());
        let wrong = Request::List(List {
            protocol: "wrong".into(),
            recipient: recipient(),
            attachment: Attachment {
                session_id: "p".into(),
                session_file: None,
            },
        });
        assert_eq!(validate_request(&wrong), Err(TransportError::InvalidSchema));
    }
}
