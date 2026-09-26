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
use crate::mailbox::{
    AdmissionReceipt, Claim, ClaimResolutionOutcome, MailboxError, MailboxHead, RecipientKey,
    RecoveredMailbox,
};

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
    pub message_id: String,
    pub kind: String,
    pub priority: String,
    pub original_sequence: u64,
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
/// Untrusted human edit intent paired with the exact server-issued head version
/// observed by the editor. The server, not Pi, mints the next revision/digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Edit {
    pub protocol: String,
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub subject: String,
    pub body: String,
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
    Edit(Edit),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Retained heads and receipts remain available for immutable artifact
    /// validation. Use headStates, not heads alone, for active work.
    pub heads: Vec<MailboxHead>,
    pub receipts: Vec<AdmissionReceipt>,
    /// Required authoritative state for every head, in the same order as heads.
    pub head_states: Vec<HeadState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HeadLifecycle {
    Held,
    Claimed,
    Admitted,
    Settled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HeadState {
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub lifecycle: HeadLifecycle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    /// The recipient Pi session a server-sent head is pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_session: Option<String>,
    /// True for a held head pinned to a different session than the viewing
    /// recipient's current one: shown, never claimed, until Retry or Drop.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub previous_session: bool,
    /// For a claimed head: `current` when this execution holds the claim,
    /// `other` when another (for example an exited) Pi does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_execution: Option<String>,
}

pub fn snapshot(
    recovered: &RecoveredMailbox,
    recipient: &RecipientKey,
) -> Result<Snapshot, MailboxError> {
    let heads: Vec<MailboxHead> = recovered
        .heads
        .values()
        .filter(|head| &head.recipient == recipient)
        .cloned()
        .collect();
    let receipts = recovered
        .receipts
        .values()
        .filter(|receipt| {
            recovered.heads.get(&receipt.stable_id).is_some_and(|head| {
                &head.recipient == recipient
                    && receipt.revision == head.revision
                    && receipt.digest == head.digest
            })
        })
        .cloned()
        .collect();
    // Reject inconsistent joins rather than interpreting an orphan or wrong
    // version as held or settled. A claim ID reused by another head would
    // otherwise project one settlement onto two separate artifacts.
    let mut claim_ids = std::collections::HashSet::new();
    for claim in recovered.claims.values() {
        if !claim_ids.insert(&claim.claim_id) {
            return Err(MailboxError::CorruptRecord);
        }
    }
    for (key, resolution) in &recovered.resolutions {
        if key != &resolution.claim_id || !claim_ids.contains(key) {
            return Err(MailboxError::CorruptRecord);
        }
    }
    for (key, claim) in &recovered.claims {
        let selected_head = recovered.heads.get(key);
        if &claim.recipient != recipient
            && !selected_head.is_some_and(|head| &head.recipient == recipient)
        {
            continue;
        }
        let head = selected_head.ok_or(MailboxError::CorruptRecord)?;
        if key != &claim.stable_id
            || claim.recipient != head.recipient
            || claim.revision != head.revision
            || claim.digest != head.digest
            || recovered
                .resolutions
                .get(&claim.claim_id)
                .is_some_and(|resolution| resolution.claim_id != claim.claim_id)
        {
            return Err(MailboxError::CorruptRecord);
        }
    }
    let head_states: Vec<HeadState> = heads
        .iter()
        .map(|head: &MailboxHead| {
            let claim = recovered.claims.get(&head.stable_id);
            let resolution = claim.and_then(|claim| recovered.resolutions.get(&claim.claim_id));
            let lifecycle = match (claim, resolution.map(|resolution| resolution.outcome)) {
                (None, _) => HeadLifecycle::Held,
                (Some(_), None) => HeadLifecycle::Claimed,
                (Some(_), Some(ClaimResolutionOutcome::Admitted)) => HeadLifecycle::Admitted,
                (Some(_), Some(ClaimResolutionOutcome::Settled)) => HeadLifecycle::Settled,
            };
            HeadState {
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                lifecycle,
                claim_id: claim.map(|claim| claim.claim_id.clone()),
                recipient_session: head
                    .delivery
                    .as_ref()
                    .and_then(|delivery| delivery.recipient_session.clone()),
                previous_session: false,
                claim_execution: None,
            }
        })
        .collect();
    let claim = head_states.iter().find_map(|state| {
        (state.lifecycle != HeadLifecycle::Held && state.lifecycle != HeadLifecycle::Settled)
            .then(|| recovered.claims.get(&state.stable_id))
            .flatten()
            .cloned()
    });
    Ok(Snapshot {
        heads,
        receipts,
        head_states,
        claim,
    })
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
                || value.delivery_digest.len() != 64
                || value.message_id.is_empty()
                || value.kind.is_empty()
                || value.priority.is_empty()
                || value.original_sequence == 0 =>
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
        Request::Edit(value)
            if value.protocol != PROTOCOL
                || value.stable_id.is_empty()
                || value.revision == 0
                || value.digest.len() != 64
                || value.subject.is_empty()
                || value.body.is_empty() =>
        {
            Err(TransportError::InvalidSchema)
        }
        _ => Ok(()),
    }
}

/// Server-side exact CAS. The authenticated route supplies scope separately;
/// the wire DTO cannot select a recipient, grant, or next revision/digest.
pub fn edit_unclaimed(
    store: &crate::mailbox::MailboxStore,
    edit: Edit,
) -> Result<MailboxHead, crate::mailbox::MailboxError> {
    validate_request(&Request::Edit(edit.clone()))
        .map_err(|_| crate::mailbox::MailboxError::InvalidRecord)?;
    store.edit_unclaimed_head(crate::mailbox::MailboxHeadEdit {
        stable_id: edit.stable_id,
        revision: edit.revision,
        digest: edit.digest,
        subject: edit.subject,
        body: edit.body,
        repin_recipient_session: None,
    })
}

/// Server-side offline admission. The authenticated sender route supplies `recipient`; the
/// wire DTO cannot select it or submit an accepted receipt. Store durability precedes receipt.
pub fn submit_offline(
    store: &crate::mailbox::MailboxStore,
    recipient: RecipientKey,
    provenance: crate::mailbox::MailboxProvenance,
    submit: Submit,
) -> Result<AdmissionReceipt, crate::mailbox::MailboxError> {
    let head = MailboxHead {
        stable_id: submit.stable_id,
        revision: submit.revision,
        digest: submit.digest,
        delivery_digest: submit.delivery_digest,
        recipient: recipient.clone(),
        subject: submit.subject,
        body: submit.body,
        recipient_generation: recipient.generation.clone(),
        sender: provenance.sender,
        target: provenance.target,
        grant_id: provenance.grant_id,
        message_id: submit.message_id,
        kind: submit.kind,
        priority: submit.priority,
        original_sequence: submit.original_sequence,
        enqueue_epoch: 0,
        accepted_at: provenance.accepted_at,
        delivery: None,
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
            recipient_generation: "g".into(),
            sender: "sender".into(),
            target: "r".into(),
            grant_id: "grant".into(),
            message_id: "message".into(),
            kind: "report".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 1,
            accepted_at: 1,
            delivery: None,
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
            message_id: h.message_id.clone(),
            kind: h.kind.clone(),
            priority: h.priority.clone(),
            original_sequence: h.original_sequence,
        };
        assert!(validate_request(&Request::Submit(submit.clone())).is_ok());
        let receipt = submit_offline(
            &store,
            recipient(),
            crate::mailbox::MailboxProvenance {
                sender: "sender".into(),
                target: "r".into(),
                grant_id: "grant".into(),
                accepted_at: 1,
            },
            submit,
        )
        .unwrap();
        assert_eq!(receipt.delivery_digest, h.delivery_digest);
        assert_eq!(
            store.load().unwrap().receipts.get(&h.delivery_digest),
            Some(&receipt)
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn snapshot_rejects_mismatched_head_claim_and_resolution_joins() {
        use crate::mailbox::{ClaimResolution, ClaimResolutionOutcome, MailboxError};
        let head = head();
        let mut recovered = RecoveredMailbox::default();
        recovered.heads.insert(head.stable_id.clone(), head.clone());
        let exact = Claim {
            claim_id: "claim-s".into(),
            recipient: head.recipient.clone(),
            stable_id: head.stable_id.clone(),
            revision: head.revision,
            digest: head.digest.clone(),
            execution: None,
        };
        let mut mismatched = exact.clone();
        mismatched.digest = "f".repeat(64);
        recovered.claims.insert(head.stable_id.clone(), mismatched);
        assert_eq!(
            snapshot(&recovered, &head.recipient),
            Err(MailboxError::CorruptRecord)
        );
        recovered
            .claims
            .insert(head.stable_id.clone(), exact.clone());
        recovered.resolutions.insert(
            exact.claim_id.clone(),
            ClaimResolution {
                claim_id: "different-claim".into(),
                outcome: ClaimResolutionOutcome::Settled,
            },
        );
        assert_eq!(
            snapshot(&recovered, &head.recipient),
            Err(MailboxError::CorruptRecord)
        );
        recovered.resolutions.clear();
        recovered.claims.insert("wrong-map-key".into(), exact);
        recovered.claims.remove(&head.stable_id);
        assert_eq!(
            snapshot(&recovered, &head.recipient),
            Err(MailboxError::CorruptRecord)
        );
        recovered.claims.clear();
        let mut second = head.clone();
        second.stable_id = "second".into();
        second.delivery_digest = "c".repeat(64);
        recovered
            .heads
            .insert(second.stable_id.clone(), second.clone());
        recovered.claims.insert(
            head.stable_id.clone(),
            Claim {
                claim_id: "duplicate".into(),
                stable_id: head.stable_id.clone(),
                recipient: head.recipient.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                execution: None,
            },
        );
        recovered.claims.insert(
            second.stable_id.clone(),
            Claim {
                claim_id: "duplicate".into(),
                stable_id: second.stable_id.clone(),
                recipient: second.recipient.clone(),
                revision: second.revision,
                digest: second.digest.clone(),
                execution: None,
            },
        );
        recovered.resolutions.insert(
            "duplicate".into(),
            ClaimResolution {
                claim_id: "duplicate".into(),
                outcome: ClaimResolutionOutcome::Settled,
            },
        );
        assert_eq!(
            snapshot(&recovered, &head.recipient),
            Err(MailboxError::CorruptRecord)
        );
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
