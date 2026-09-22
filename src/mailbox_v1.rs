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

/// Untrusted offline sender input. The recipient is selected by the authenticated sender
/// route and passed separately as server-owned state; no Pi attachment or receipt is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submit {
    pub protocol: String,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub delivery_digest: String,
    pub subject: String,
    pub body: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Get {
    pub protocol: String,
    pub recipient: RecipientKey,
    pub delivery_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct List {
    pub protocol: String,
    pub recipient: RecipientKey,
}
/// Consumer-channel request: Herdr mints the claim after separately trusted consumer discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaimRequest {
    pub protocol: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resolve {
    pub protocol: String,
    pub claim_id: String,
    pub outcome: ResolveOutcome,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
    let valid_recipient = |recipient: &RecipientKey| {
        !recipient.recipient_id.is_empty() && !recipient.generation.is_empty()
    };
    match request {
        Request::Submit(value)
            if value.protocol != PROTOCOL
                || value.stable_id.is_empty()
                || value.revision == 0
                || value.digest.len() != 64
                || value.delivery_digest.len() != 64 =>
        {
            Err(TransportError::InvalidSchema)
        }
        Request::Get(value)
            if value.protocol != PROTOCOL
                || !valid_recipient(&value.recipient)
                || value.delivery_digest.len() != 64 =>
        {
            Err(TransportError::InvalidSchema)
        }
        Request::List(value)
            if value.protocol != PROTOCOL || !valid_recipient(&value.recipient) =>
        {
            Err(TransportError::InvalidSchema)
        }
        Request::Claim(value) if value.protocol != PROTOCOL => Err(TransportError::InvalidSchema),
        Request::Resolve(value) if value.protocol != PROTOCOL || value.claim_id.is_empty() => {
            Err(TransportError::InvalidSchema)
        }
        _ => Ok(()),
    }
}

/// Server-side offline admission. The authenticated sender route supplies `recipient`; the
/// wire DTO cannot select it or submit an accepted receipt. Store durability precedes receipt.
pub fn submit_offline(
    store: &crate::mailbox::MailboxStore,
    recipient: RecipientKey,
    submit: Submit,
) -> Result<AdmissionReceipt, crate::mailbox::MailboxError> {
    let head = MailboxHead {
        stable_id: submit.stable_id,
        revision: submit.revision,
        digest: submit.digest,
        delivery_digest: submit.delivery_digest,
        recipient,
        subject: submit.subject,
        body: submit.body,
    };
    store.append_offline_head(head)
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
    use crate::mailbox::MailboxHead;
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
    fn offline_submit_needs_no_attachment_and_mints_the_receipt_after_store_admission() {
        let directory =
            std::env::temp_dir().join(format!("herdr-mailbox-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let store = crate::mailbox::MailboxStore::open(&directory).unwrap();
        let h = head();
        let submit = Submit {
            protocol: PROTOCOL.into(),
            stable_id: h.stable_id.clone(),
            revision: h.revision,
            digest: h.digest.clone(),
            delivery_digest: h.delivery_digest.clone(),
            subject: h.subject.clone(),
            body: h.body.clone(),
        };
        assert!(validate_request(&Request::Submit(submit.clone())).is_ok());
        let receipt = submit_offline(&store, recipient(), submit).unwrap();
        assert_eq!(receipt.delivery_digest, h.delivery_digest);
        assert_eq!(
            store.load().unwrap().receipts.get(&h.delivery_digest),
            Some(&receipt)
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn raw_clients_cannot_submit_receipts_or_claims() {
        let forged_submit = format!(
            r#"{{"method":"submit","params":{{"protocol":"mailbox.v1","stableId":"s","revision":1,"digest":"{}","deliveryDigest":"{}","subject":"x","body":"y","receipt":{{}}}}}}"#,
            "a".repeat(64),
            "b".repeat(64)
        );
        assert!(serde_json::from_str::<Request>(&forged_submit).is_err());
        let forged_claim =
            r#"{"method":"claim","params":{"protocol":"mailbox.v1","claim":{"claimId":"forged"}}}"#;
        assert!(serde_json::from_str::<Request>(forged_claim).is_err());
    }
}
