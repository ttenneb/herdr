//! #181: a waiting message follows its agent, and no message sits unread in
//! the queue of a Pi that has no Messages.
//!
//! * A bootstrap stream counts as a Messages consumer only once it reads its
//!   own inbox (`mailbox.watch`, `mailbox.snapshot`, `mailbox.claim`).
//! * When a **managed** Pi on session S becomes a consumer in pane N, held,
//!   unclaimed heads pinned to S (`delivery.recipientSession`) in any other
//!   pane move to N exactly once: the old head is closed with the Drop
//!   records (`closedBy:"moved"`) and a copy with `delivery.movedFrom` is
//!   appended to N, in one store lock section. Reported and hand-typed Pis
//!   never pull (a reported Pi could name any session). Heads without a
//!   session pin belong to their pane and stay. Claimed heads (including an
//!   ended execution's leftover claim) stay: moving could run them twice.
//! * A live Pi that has not become a consumer within the 30 s grace gets the
//!   heads queued for it during that grace typed, in order, through the draft
//!   guard: each head is closed and replaced by its typed-history row
//!   (`typedReason:"fallback_30s"`, `movedFrom`) in one lock section before
//!   it is typed, so it can never also be claimed later. Its sender is told.

use std::time::{Duration, Instant};

use super::messages::{
    now_secs, pane_recipient, sha256_fields, SenderAttribution, MESSAGES_ATTACH_GRACE,
};
use super::{App, MailboxBootstrapSession};
use crate::mailbox::{is_typed_history, MailboxHead, MailboxStore, RecoveredMailbox};

/// How often the queues of live Pis without Messages are checked.
const UNCONSUMED_QUEUE_SCAN_EVERY: Duration = Duration::from_secs(5);

/// The deterministic ID of a head's copy when it follows session `session`.
pub(crate) fn moved_copy_id(stable_id: &str, session: &str) -> String {
    format!(
        "moved.{}",
        &sha256_fields(&[stable_id.as_bytes(), session.as_bytes()])[..32]
    )
}

/// The deterministic ID of a queued head's typed-history row.
pub(crate) fn typed_row_id(stable_id: &str) -> String {
    format!(
        "typed.{}",
        &sha256_fields(&[b"herdr-queued-typed", stable_id.as_bytes()])[..32]
    )
}

fn resolution_closed_by<'a>(recovered: &'a RecoveredMailbox, stable_id: &str) -> Option<&'a str> {
    let claim = recovered.claims.get(stable_id)?;
    crate::mailbox::is_withdrawn_claim(claim).then_some(())?;
    recovered
        .resolutions
        .get(&claim.claim_id)
        .and_then(|resolution| resolution.closed_by.as_deref())
}

/// The text typed for a queued head: the original prompt for a plain
/// `agent prompt`, otherwise the subject and body.
fn typed_text(head: &MailboxHead) -> String {
    let origin = head
        .delivery
        .as_ref()
        .map(|delivery| delivery.origin.as_str());
    if origin == Some("agent_prompt") {
        head.body.clone()
    } else {
        format!("{}\n\n{}", head.subject, head.body)
    }
}

impl App {
    /// Marks this stream as a Messages consumer on its first own-inbox read;
    /// the first time, the pane becomes Messages-capable and a managed
    /// session pulls the heads waiting for it in other panes.
    pub(crate) fn note_messages_consumer(&mut self, session: &MailboxBootstrapSession) {
        if session.history_only || self.mailbox_bootstrap_session_current(session).is_err() {
            return;
        }
        if !self
            .messages_consumer_bindings
            .insert(session.binding_generation.clone())
        {
            return;
        }
        self.mark_pane_messages_capable(&session.caller);
        if session.recipient_only.is_none() {
            self.follow_session_heads(session);
        }
    }

    /// The verified session of a managed Pi execution: its managed launch
    /// record for this exact generation, and the same session as reported.
    fn managed_session_of(&self, session: &MailboxBootstrapSession) -> Option<String> {
        let (terminal_id, launch) = self
            .managed_pi_launches
            .iter()
            .find(|(terminal_id, _)| terminal_id.to_string() == session.caller)?;
        if launch.generation != session.active_execution_generation
            || launch.session_path.is_empty()
        {
            return None;
        }
        let reported = self.current_agent_session_value(&terminal_id.to_string())?;
        (reported == launch.session_path).then(|| launch.session_path.clone())
    }

    /// Fills `movedTo` on heads closed `moved`: the pane the copy went to.
    pub(crate) fn annotate_moved_heads(
        &self,
        snapshot: &mut crate::mailbox_v1::Snapshot,
        recovered: &RecoveredMailbox,
    ) {
        for state in &mut snapshot.head_states {
            if state.closed_by.as_deref() != Some("moved") {
                continue;
            }
            let copy = recovered
                .heads
                .values()
                .find(|head| head.moved_from() == Some(state.stable_id.as_str()));
            state.moved_to = Some(
                copy.and_then(|copy| self.terminal_for_recipient(&copy.recipient.recipient_id))
                    .and_then(|terminal| self.public_pane_for_terminal(&terminal))
                    .unwrap_or_else(|| "a closed pane".into()),
            );
        }
    }

    /// A consuming Pi on `session` is attached to this terminal right now.
    fn consuming_on_session(&self, terminal_key: &str, session: &str) -> bool {
        self.attached_messages_recipient(terminal_key).is_some()
            && self.current_agent_session_value(terminal_key).as_deref() == Some(session)
    }

    /// Moves the held, unclaimed heads pinned to this managed session from
    /// every other pane to the caller's pane (and repairs a move a crash
    /// interrupted). Returns (moved, left claimed elsewhere).
    pub(crate) fn follow_session_heads(
        &mut self,
        session: &MailboxBootstrapSession,
    ) -> (usize, usize) {
        let Some(pinned) = self.managed_session_of(session) else {
            return (0, 0);
        };
        let Some(queue_key) = self.pane_queue_key(&session.caller) else {
            return (0, 0);
        };
        let target = pane_recipient(&queue_key);
        let own = self.inbox_recipients(&session.caller);
        let Ok(store) = MailboxStore::open(&self.sender_authority_dir) else {
            return (0, 0);
        };
        let Ok(recovered) = store.load() else {
            return (0, 0);
        };
        let pinned_here = |head: &MailboxHead| {
            head.delivery
                .as_ref()
                .and_then(|delivery| delivery.recipient_session.as_deref())
                == Some(pinned.as_str())
                && !own.contains(&head.recipient)
                && !is_typed_history(head)
        };
        let mut candidates: Vec<&MailboxHead> = recovered
            .heads
            .values()
            .filter(|head| pinned_here(head))
            .filter(|head| match recovered.claims.get(&head.stable_id) {
                None => true,
                // Repair: closed as moved, but its copy never landed.
                Some(_) => {
                    resolution_closed_by(&recovered, &head.stable_id) == Some("moved")
                        && !recovered
                            .heads
                            .contains_key(&moved_copy_id(&head.stable_id, &pinned))
                }
            })
            .filter(|head| {
                self.terminal_for_recipient(&head.recipient.recipient_id)
                    .is_none_or(|old| !self.consuming_on_session(&old, &pinned))
            })
            .collect();
        candidates.sort_by_key(|head| (head.enqueue_epoch, head.accepted_at));
        let mut moved = Vec::new();
        for head in candidates {
            let copy_id = moved_copy_id(&head.stable_id, &pinned);
            let mut delivery =
                head.delivery
                    .clone()
                    .unwrap_or_else(|| crate::mailbox::ServerDelivery {
                        origin: "moved".into(),
                        sender_label: head.sender.clone(),
                        sender_session: None,
                        recipient_session: Some(pinned.clone()),
                        correlation: None,
                        retry_of: None,
                        typed_reason: None,
                        moved_from: None,
                    });
            delivery.moved_from = Some(head.stable_id.clone());
            let copy = MailboxHead {
                digest: sha256_fields(&[
                    copy_id.as_bytes(),
                    &1_u64.to_be_bytes(),
                    head.subject.as_bytes(),
                    head.body.as_bytes(),
                ]),
                delivery_digest: sha256_fields(&[b"herdr-moved-delivery", copy_id.as_bytes()]),
                revision: 1,
                recipient: target.clone(),
                recipient_generation: target.generation.clone(),
                target: session.caller.clone(),
                message_id: format!("moved:{}", head.message_id),
                enqueue_epoch: 0,
                accepted_at: now_secs(),
                delivery: Some(delivery),
                stable_id: copy_id,
                ..head.clone()
            };
            match store.supersede_unclaimed_head(
                &head.stable_id,
                head.revision,
                &head.digest,
                "moved",
                copy,
            ) {
                Ok(()) => moved.push((
                    head.stable_id.clone(),
                    self.terminal_for_recipient(&head.recipient.recipient_id)
                        .and_then(|old| self.public_pane_for_terminal(&old)),
                )),
                // Claimed or edited in its old pane meanwhile: it stays there.
                Err(error) => tracing::info!(
                    head = %head.stable_id,
                    ?error,
                    "messages: a waiting message was not moved (claimed or changed in its old pane)"
                ),
            }
        }
        // Claimed heads of this session left in other panes by an ended
        // execution: never moved (they may have run); named in the note.
        let left: Vec<Option<String>> = recovered
            .heads
            .values()
            .filter(|head| pinned_here(head))
            .filter(|head| {
                recovered.claims.get(&head.stable_id).is_some_and(|claim| {
                    !crate::mailbox::is_withdrawn_claim(claim)
                        && !matches!(
                            recovered.resolutions.get(&claim.claim_id),
                            Some(crate::mailbox::ClaimResolution {
                                outcome: crate::mailbox::ClaimResolutionOutcome::Settled,
                                ..
                            })
                        )
                })
            })
            .map(|head| {
                self.terminal_for_recipient(&head.recipient.recipient_id)
                    .and_then(|old| self.public_pane_for_terminal(&old))
            })
            .collect();
        if !left.is_empty() {
            tracing::info!(
                terminal = %session.caller,
                left_claimed = left.len(),
                "messages: claimed messages of this session stay in their old pane (Retry or Drop there)"
            );
        }
        // One note per move (deterministic ID), naming any claimed messages
        // left behind; a later attach that moves nothing sends none.
        if !moved.is_empty() {
            tracing::info!(
                terminal = %session.caller,
                moved = moved.len(),
                left_claimed = left.len(),
                "messages: waiting messages followed their agent's session to this pane"
            );
            let from = |panes: &mut dyn Iterator<Item = &Option<String>>| {
                let mut names: Vec<String> = panes
                    .map(|pane| pane.clone().unwrap_or_else(|| "a closed pane".into()))
                    .collect();
                names.sort();
                names.dedup();
                names.join(", ")
            };
            let mut body = String::new();
            if !moved.is_empty() {
                body.push_str(&format!(
                    "{} waiting message(s) for this session moved here from {}. They run in their original order.",
                    moved.len(),
                    from(&mut moved.iter().map(|(_, pane)| pane))
                ));
            }
            if !left.is_empty() {
                if !body.is_empty() {
                    body.push_str("\n\n");
                }
                body.push_str(&format!(
                    "{} message(s) an earlier run of this session had already picked up stay in {} and need Retry or Drop there (moving them could run them twice).",
                    left.len(),
                    from(&mut left.iter())
                ));
            }
            let note = crate::app::messages::OutgoingMessage {
                origin: "agent_prompt",
                subject: "Messages moved to this pane".into(),
                body,
                priority: "normal".into(),
                kind: "advisory".into(),
                message_id: Some(format!(
                    "moved-note.{}",
                    &sha256_fields(&[
                        pinned.as_bytes(),
                        moved
                            .iter()
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                            .as_bytes(),
                    ])[..32]
                )),
                correlation: None,
                replace_pending: false,
            };
            self.send_herdr_note(&session.caller, note);
        }
        (moved.len(), left.len())
    }

    /// Queues a note from Herdr itself to a pane that takes Messages.
    fn send_herdr_note(&self, terminal_key: &str, note: crate::app::messages::OutgoingMessage) {
        if !self.pane_takes_messages(terminal_key) {
            return;
        }
        let herdr = SenderAttribution {
            terminal: None,
            label: "herdr".into(),
            session: None,
            external_key: None,
        };
        let options = crate::api::schema::MessageSendOptions {
            send_new: true,
            ..Default::default()
        };
        if let Err(err) = self.route_ordinary_send(terminal_key, &herdr, note, &options) {
            tracing::warn!(
                terminal = terminal_key,
                ?err,
                "messages: could not queue a Herdr note"
            );
        }
    }

    /// The live foreground Pi of this pane, when it has been running for the
    /// 30 s grace without becoming a Messages consumer: (pid, birth tick,
    /// age).
    fn unconsumed_live_pi(&self, terminal_key: &str) -> Option<(u32, u64, Duration)> {
        if self.attached_messages_recipient(terminal_key).is_some() {
            return None;
        }
        let (pid, ticks) = self.foreground_pi_identity(terminal_key)?;
        let consuming = self
            .messages_attached_pis
            .get(&(pid, ticks))
            .is_some_and(|closed| {
                closed.is_none_or(|since| since.elapsed() < MESSAGES_ATTACH_GRACE)
            });
        if consuming {
            return None;
        }
        let age = self.pi_process_age(pid, ticks)?;
        (age >= MESSAGES_ATTACH_GRACE).then_some((pid, ticks, age))
    }

    /// Server tick: types the heads queued during the grace of a live Pi
    /// that never turned Messages on. Rate-limited.
    pub(crate) fn maybe_type_unconsumed_queues(&mut self, now: Instant) -> bool {
        if self.next_unconsumed_queue_scan.is_some_and(|due| now < due) {
            return false;
        }
        self.next_unconsumed_queue_scan = Some(now + UNCONSUMED_QUEUE_SCAN_EVERY);
        let panes: Vec<(String, Duration)> = self
            .state
            .terminals
            .values()
            .filter(|terminal| terminal.messages_capable)
            .filter(|terminal| {
                terminal.effective_known_agent() == Some(crate::detect::Agent::Pi)
                    || terminal.effective_known_agent().is_none()
            })
            .map(|terminal| terminal.id.to_string())
            .filter_map(|key| self.unconsumed_live_pi(&key).map(|(_, _, age)| (key, age)))
            .collect();
        let mut changed = false;
        for (key, age) in panes {
            changed |= self.type_queued_heads_for_unconsumed_pi(&key, age) > 0;
        }
        changed
    }

    /// Converts and types the heads queued for this pane's live Pi since it
    /// started (pinned to its session or unpinned), in their original order.
    /// Returns how many were converted.
    pub(crate) fn type_queued_heads_for_unconsumed_pi(
        &mut self,
        terminal_key: &str,
        pi_age: Duration,
    ) -> usize {
        let Some(terminal_id) = self
            .state
            .terminals
            .keys()
            .find(|id| id.to_string() == terminal_key)
            .cloned()
        else {
            return 0;
        };
        let Some(queue_key) = self.pane_queue_key(terminal_key) else {
            return 0;
        };
        let recipients = self.inbox_recipients(terminal_key);
        let session = self.current_agent_session_value(terminal_key);
        let born_at = now_secs().saturating_sub(pi_age.as_secs() + 1);
        let Ok(store) = MailboxStore::open(&self.sender_authority_dir) else {
            return 0;
        };
        let Ok(recovered) = store.load() else {
            return 0;
        };
        let mut heads: Vec<MailboxHead> = recovered
            .heads
            .values()
            .filter(|head| recipients.contains(&head.recipient))
            .filter(|head| !is_typed_history(head) && head.delivery.is_some())
            .filter(|head| !recovered.claims.contains_key(&head.stable_id))
            .filter(|head| head.accepted_at >= born_at)
            .filter(|head| {
                head.delivery
                    .as_ref()
                    .and_then(|delivery| delivery.recipient_session.as_deref())
                    .is_none_or(|pinned| session.as_deref() == Some(pinned))
            })
            .cloned()
            .collect();
        heads.sort_by_key(|head| (head.enqueue_epoch, head.accepted_at));
        let target_label = self
            .public_pane_for_terminal(terminal_key)
            .unwrap_or_else(|| terminal_key.to_string());
        let mut converted = 0;
        for head in heads {
            let text = typed_text(&head);
            let delivery = head.delivery.clone().expect("filtered on delivery");
            let row_id = typed_row_id(&head.stable_id);
            let recipient = pane_recipient(&queue_key);
            let mut row_delivery = delivery.clone();
            row_delivery.typed_reason = Some("fallback_30s".into());
            row_delivery.moved_from = Some(head.stable_id.clone());
            row_delivery.recipient_session = None;
            let row = MailboxHead {
                digest: sha256_fields(&[
                    row_id.as_bytes(),
                    &1_u64.to_be_bytes(),
                    head.subject.as_bytes(),
                    head.body.as_bytes(),
                ]),
                delivery_digest: sha256_fields(&[b"herdr-typed-delivery", row_id.as_bytes()]),
                revision: 1,
                recipient_generation: recipient.generation.clone(),
                recipient,
                target: terminal_key.to_string(),
                grant_id: format!("typed:{}", head.sender),
                message_id: row_id.clone(),
                enqueue_epoch: 0,
                accepted_at: now_secs(),
                delivery: Some(row_delivery),
                stable_id: row_id,
                ..head.clone()
            };
            // Closed and replaced before typing: it can never also be claimed.
            if let Err(error) = store.supersede_unclaimed_head(
                &head.stable_id,
                head.revision,
                &head.digest,
                "typed",
                row,
            ) {
                tracing::info!(head = %head.stable_id, ?error, "messages: a queued message was not typed (claimed or changed meanwhile)");
                continue;
            }
            converted += 1;
            let sender_terminal = self
                .state
                .terminals
                .keys()
                .map(|id| id.to_string())
                .find(|id| *id == head.sender);
            let sender = SenderAttribution {
                terminal: sender_terminal.clone(),
                label: delivery.sender_label.clone(),
                session: delivery.sender_session.clone(),
                external_key: None,
            };
            tracing::warn!(
                terminal = terminal_key,
                head = %head.stable_id,
                "messages: the Pi in this pane has not turned Messages on within 30 s; a queued message is typed instead"
            );
            if let Some(sender_terminal) = sender_terminal.as_deref() {
                let note = crate::app::messages::OutgoingMessage {
                    origin: "agent_prompt",
                    subject: format!("Typed into {target_label} instead of queued"),
                    body: format!(
                        "Your message \"{}\" to {target_label} was queued, but the Pi there did not turn Messages on within 30 s (for example no .pi/pi-input-gate.json in its checkout). It is typed into the pane instead, after any unsent human input clears; if that fails you get another note.",
                        head.subject
                    ),
                    priority: "normal".into(),
                    kind: "advisory".into(),
                    message_id: Some(format!("typed-note.{}", typed_row_id(&head.stable_id))),
                    correlation: None,
                    replace_pending: false,
                };
                self.send_herdr_note(sender_terminal, note);
            }
            let origin: &'static str = match delivery.origin.as_str() {
                "structured_prompt" => "structured_prompt",
                "handoff" => "handoff",
                _ => "agent_prompt",
            };
            if self.typed_delivery_must_wait(&terminal_id) {
                self.defer_typed_delivery(
                    terminal_id.clone(),
                    text,
                    crate::detect::Agent::Pi,
                    target_label.clone(),
                    sender,
                    origin,
                    "fallback_30s".into(),
                );
                if let Some(deferral) = self.typed_deferrals.last_mut() {
                    deferral.history_recorded = true;
                }
                continue;
            }
            if let Err((code, message)) =
                self.type_submission(&terminal_id, crate::detect::Agent::Pi, &text)
            {
                let deferral = crate::app::typed_deferral::TypedDeferral {
                    id: format!("queued-typed.{}", head.stable_id),
                    terminal_id: terminal_id.clone(),
                    text,
                    expected_agent: crate::detect::Agent::Pi,
                    target: target_label.clone(),
                    sender,
                    origin,
                    deadline: Instant::now(),
                    execution: None,
                    reason: "fallback_30s".into(),
                    history_recorded: true,
                };
                self.fail_typed_deferral(&deferral, code, &message);
            }
        }
        converted
    }
}
