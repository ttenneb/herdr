//! Ordinary agent sends (`agent prompt`, structured prompts, `handoff send`)
//! routed into the recipient's Messages queue when it has a live Messages
//! stream, and typed into the pane exactly as before when it has none.
//!
//! Everything here is attribution and routing. Sender, bound-report and route
//! authority stay with the trusted managed launch; a server-minted head never
//! grants any of them.

use crate::api::schema::{MessageDelivery, MessageSendOptions, MessageTransport};
use crate::app::App;
use crate::mailbox::{
    is_withdrawn_claim, MailboxHead, MailboxStore, RecipientKey, RecoveredMailbox, SendCorrelation,
    ServerDelivery,
};

/// Pi's per-head limits for the Messages path; larger sends use the PTY path.
const MAX_SUBJECT_BYTES: usize = 160;
const MAX_BODY_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SenderAttribution {
    /// Terminal ID of the sending pane, when the caller runs inside one.
    pub terminal: Option<String>,
    pub label: String,
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutgoingMessage {
    pub origin: &'static str,
    pub subject: String,
    pub body: String,
    pub priority: String,
    pub kind: String,
    /// Stable per-send identity; a repeat returns the existing head.
    pub message_id: Option<String>,
    pub correlation: Option<SendCorrelation>,
    /// Structured-prompt `supersession.mode = replace_pending`.
    pub replace_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingView {
    pub stable_id: String,
    pub revision: u64,
    pub digest: String,
    pub subject: String,
    pub priority: String,
    pub enqueued_at: u64,
    pub age_seconds: u64,
}

#[derive(Debug)]
pub(crate) enum SendRoute {
    /// Use the pre-Messages PTY path unchanged.
    Pty,
    Mailbox(MessageDelivery),
}

#[derive(Debug)]
pub(crate) enum SendRefusal {
    /// This sender's waiting messages to the recipient, newest first.
    PendingExists(Vec<PendingView>),
    /// `--edit-pending` named a message that is no longer waiting, or its
    /// revision changed; carries the sender's current waiting list.
    PendingChanged(Vec<PendingView>),
    MailboxUnavailable(&'static str),
    Store(String),
}

/// Where a head stands for the recipient's current execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadStanding {
    Current,
    /// Pinned to a different (or not yet known) recipient session and never
    /// picked up: shown with `previousSession`, never claimed, until the
    /// recipient explicitly repins (Retry) or drops it.
    Stranded,
    /// Dropped by the recipient; hidden from every view.
    Withdrawn,
}

pub(crate) fn head_standing(
    head: &MailboxHead,
    recovered: &RecoveredMailbox,
    current_session: Option<&str>,
) -> HeadStanding {
    match recovered.claims.get(&head.stable_id) {
        Some(claim) if is_withdrawn_claim(claim) => HeadStanding::Withdrawn,
        Some(_) => HeadStanding::Current,
        None => match head
            .delivery
            .as_ref()
            .and_then(|delivery| delivery.recipient_session.as_deref())
        {
            Some(pinned) if current_session != Some(pinned) => HeadStanding::Stranded,
            _ => HeadStanding::Current,
        },
    }
}

/// Whether the recipient's current execution may claim this unclaimed head.
pub(crate) fn head_claimable_by(head: &MailboxHead, current_session: Option<&str>) -> bool {
    match head
        .delivery
        .as_ref()
        .and_then(|delivery| delivery.recipient_session.as_deref())
    {
        Some(pinned) => current_session == Some(pinned),
        None => true,
    }
}

/// Removes withdrawn heads (with their states and receipts) from a recipient
/// view and marks held heads pinned to another session `previousSession`.
pub(crate) fn filter_recipient_snapshot(
    mut snapshot: crate::mailbox_v1::Snapshot,
    recovered: &RecoveredMailbox,
    current_session: Option<&str>,
) -> crate::mailbox_v1::Snapshot {
    let standing: std::collections::HashMap<String, HeadStanding> = snapshot
        .heads
        .iter()
        .map(|head| {
            (
                head.stable_id.clone(),
                head_standing(head, recovered, current_session),
            )
        })
        .collect();
    let keep: std::collections::HashSet<String> = standing
        .iter()
        .filter(|(_, standing)| **standing != HeadStanding::Withdrawn)
        .map(|(stable_id, _)| stable_id.clone())
        .collect();
    for state in &mut snapshot.head_states {
        state.previous_session = standing.get(&state.stable_id) == Some(&HeadStanding::Stranded);
    }
    snapshot.heads.retain(|head| keep.contains(&head.stable_id));
    snapshot
        .head_states
        .retain(|state| keep.contains(&state.stable_id));
    snapshot
        .receipts
        .retain(|receipt| keep.contains(&receipt.stable_id));
    if snapshot
        .claim
        .as_ref()
        .is_some_and(|claim| !keep.contains(&claim.stable_id))
    {
        snapshot.claim = None;
    }
    snapshot
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(1)
        .max(1)
}

fn sha256_fields(fields: &[&[u8]]) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for value in fields {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }
    format!("{:x}", hasher.finalize())
}

/// Text Pi's Messages path renders safely: bounded, no terminal or bidi
/// controls (tab and newline are fine). Anything else keeps the PTY path.
pub(crate) fn mailbox_safe_text(subject: &str, body: &str) -> bool {
    let clean = |text: &str| {
        !text.chars().any(|ch| {
            (ch.is_control() && ch != '\n' && ch != '\t')
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    };
    !subject.is_empty()
        && !body.is_empty()
        && subject.len() <= MAX_SUBJECT_BYTES
        && body.len() <= MAX_BODY_BYTES
        && !subject.contains('\n')
        && clean(subject)
        && clean(body)
}

fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Subject line for a send with no explicit subject.
pub(crate) fn subject_for(prefix: &str, label: &str) -> String {
    truncate_bytes(&format!("{prefix} {label}"), 120)
}

fn pending_view(head: &MailboxHead) -> PendingView {
    PendingView {
        stable_id: head.stable_id.clone(),
        revision: head.revision,
        digest: head.digest.clone(),
        subject: head.subject.clone(),
        priority: head.priority.clone(),
        enqueued_at: head.accepted_at,
        age_seconds: now_secs().saturating_sub(head.accepted_at),
    }
}

/// Parses a `[[pi-input-gate:ingress:v3:<base64url header>]]\n<body>` prompt.
/// Returns `None` for any other text, which then routes as a plain prompt.
pub(crate) fn parse_structured_prompt(text: &str) -> Option<OutgoingMessage> {
    use base64::Engine as _;
    let rest = text.strip_prefix("[[pi-input-gate:ingress:v3:")?;
    let (encoded, body) = rest.split_once("]]\n")?;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded.trim_end_matches('='))
        .ok()?;
    let header: serde_json::Value = serde_json::from_slice(&header).ok()?;
    let priority = header["priority"].as_str()?;
    if !matches!(priority, "low" | "normal" | "high") {
        return None;
    }
    let metadata = &header["metadata"];
    let subject = metadata["subject"].as_str()?.trim().to_string();
    let correlation = &metadata["correlation"];
    let correlation = SendCorrelation {
        namespace: correlation["namespace"].as_str()?.to_string(),
        key: correlation["key"].as_str()?.to_string(),
        revision: correlation["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)?,
    };
    let replace_pending = metadata["supersession"]["mode"].as_str() == Some("replace_pending");
    Some(OutgoingMessage {
        origin: "structured_prompt",
        message_id: Some(format!(
            "corr:{}:{}:{}",
            correlation.namespace, correlation.key, correlation.revision
        )),
        subject,
        body: body.to_string(),
        priority: priority.to_string(),
        kind: "advisory".into(),
        correlation: Some(correlation),
        replace_pending,
    })
}

impl App {
    /// The server-issued recipient key of a live, current, non-history
    /// Messages stream for this terminal, if any.
    pub(crate) fn attached_messages_recipient(&self, terminal_id: &str) -> Option<RecipientKey> {
        self.mailbox_bootstrap_bindings
            .values()
            .filter(|session| session.caller == terminal_id && !session.history_only)
            .find(|session| self.mailbox_bootstrap_session_current(session).is_ok())
            .map(|session| session.recipient.clone())
    }

    /// Current `agent_session` value of the agent in this terminal.
    pub(crate) fn current_agent_session_value(&self, terminal_id: &str) -> Option<String> {
        self.collect_agent_infos()
            .into_iter()
            .find(|agent| agent.terminal_id == terminal_id)
            .and_then(|agent| agent.agent_session.map(|session| session.value))
    }

    /// Attributes a local API caller to the pane whose shell it descends from.
    pub(crate) fn attribute_sender(&self, caller_pid: Option<u32>) -> SenderAttribution {
        let external = SenderAttribution {
            terminal: None,
            label: "external".into(),
            session: None,
        };
        let Some(mut pid) = caller_pid else {
            return external;
        };
        let shells: std::collections::HashMap<u32, String> = self
            .terminal_runtimes
            .iter()
            .filter_map(|(terminal_id, runtime)| {
                runtime
                    .child_pid()
                    .map(|pid| (pid, terminal_id.to_string()))
            })
            .collect();
        for _ in 0..64 {
            if let Some(terminal) = shells.get(&pid) {
                let agent = self
                    .collect_agent_infos()
                    .into_iter()
                    .find(|agent| &agent.terminal_id == terminal);
                let label = agent
                    .as_ref()
                    .and_then(|agent| agent.name.clone().or_else(|| Some(agent.pane_id.clone())))
                    .unwrap_or_else(|| terminal.clone());
                return SenderAttribution {
                    terminal: Some(terminal.clone()),
                    label,
                    session: agent.and_then(|agent| agent.agent_session.map(|s| s.value)),
                };
            }
            match parent_pid(pid) {
                Some(parent) if parent > 1 && parent != pid => pid = parent,
                _ => break,
            }
        }
        external
    }

    /// Routes one ordinary send. `Pty` means: use the unchanged PTY path.
    pub(crate) fn route_ordinary_send(
        &self,
        recipient_terminal: &str,
        sender: &SenderAttribution,
        message: OutgoingMessage,
        options: &MessageSendOptions,
    ) -> Result<SendRoute, SendRefusal> {
        let transport = options.transport.unwrap_or(MessageTransport::Auto);
        if transport == MessageTransport::Pty {
            return Ok(SendRoute::Pty);
        }
        let Some(recipient) = self.attached_messages_recipient(recipient_terminal) else {
            return match transport {
                MessageTransport::Mailbox => Err(SendRefusal::MailboxUnavailable(
                    "the recipient has no live Messages connection",
                )),
                _ => Ok(SendRoute::Pty),
            };
        };
        if !mailbox_safe_text(&message.subject, &message.body) {
            return match transport {
                MessageTransport::Mailbox => Err(SendRefusal::MailboxUnavailable(
                    "the message exceeds Messages limits or contains control characters",
                )),
                _ => Ok(SendRoute::Pty),
            };
        }
        let store = MailboxStore::open(&self.sender_authority_dir)
            .map_err(|error| SendRefusal::Store(error.to_string()))?;
        let recovered = store
            .load()
            .map_err(|error| SendRefusal::Store(error.to_string()))?;
        let sender_key = sender.terminal.clone().unwrap_or_else(|| "external".into());
        let recipient_session = self.current_agent_session_value(recipient_terminal);

        // An identical send (same message ID from the same sender) returns its
        // existing head instead of queuing a duplicate.
        let message_id = message
            .message_id
            .clone()
            .or_else(crate::platform::random_route_epoch)
            .unwrap_or_else(|| format!("send-{}", now_secs()));
        // Printable and deterministic, so a sender can name it on a command
        // line and a repeat of the same send finds it.
        let stable_id = format!(
            "send.{}",
            &sha256_fields(&[
                recipient_terminal.as_bytes(),
                sender_key.as_bytes(),
                message_id.as_bytes()
            ])[..32]
        );
        if let Some(existing) = recovered.heads.get(&stable_id) {
            return Ok(SendRoute::Mailbox(MessageDelivery {
                path: "mailbox".into(),
                stable_id: Some(existing.stable_id.clone()),
                revision: Some(existing.revision),
                edited: false,
                duplicate: true,
            }));
        }

        // Pending check: this sender's unclaimed, visible heads to this recipient.
        let mut pending: Vec<&MailboxHead> = if sender.terminal.is_some() {
            recovered
                .heads
                .values()
                .filter(|head| {
                    head.recipient == recipient
                        && head.sender == sender_key
                        && head.delivery.is_some()
                        && !recovered.claims.contains_key(&head.stable_id)
                })
                .collect()
        } else {
            Vec::new()
        };
        // Newest first.
        pending.sort_by_key(|head| std::cmp::Reverse(head.enqueue_epoch));
        let listed =
            |pending: &[&MailboxHead]| pending.iter().map(|head| pending_view(head)).collect();
        // The newest head from this sender to this recipient with the same
        // correlation namespace and key, in any lifecycle except dropped.
        let correlated = message.correlation.as_ref().and_then(|correlation| {
            recovered
                .heads
                .values()
                .filter(|head| {
                    head.recipient == recipient
                        && head.sender == sender_key
                        && !recovered
                            .claims
                            .get(&head.stable_id)
                            .is_some_and(is_withdrawn_claim)
                        && head
                            .delivery
                            .as_ref()
                            .and_then(|delivery| delivery.correlation.as_ref())
                            .is_some_and(|existing| {
                                existing.namespace == correlation.namespace
                                    && existing.key == correlation.key
                            })
                })
                .max_by_key(|head| head.enqueue_epoch)
        });
        // `replace_pending` edits the correlated head while it waits; once it
        // was picked up the sender must decide again (never auto-send).
        let edit_target = if let (true, Some(head)) = (message.replace_pending, correlated) {
            if recovered.claims.contains_key(&head.stable_id) {
                return Err(SendRefusal::PendingChanged(listed(&pending)));
            }
            Some(head)
        } else if let Some(wanted) = options.edit_pending.as_deref() {
            match pending
                .iter()
                .copied()
                .find(|head| head.stable_id == wanted)
            {
                Some(head) => Some(head),
                None => return Err(SendRefusal::PendingChanged(listed(&pending))),
            }
        } else if !pending.is_empty() && !options.send_new {
            return Err(SendRefusal::PendingExists(listed(&pending)));
        } else {
            None
        };
        if let Some(target) = edit_target {
            if options
                .expect_revision
                .is_some_and(|expected| expected != target.revision)
            {
                return Err(SendRefusal::PendingChanged(listed(&pending)));
            }
            let edited = store
                .edit_unclaimed_head(crate::mailbox::MailboxHeadEdit {
                    stable_id: target.stable_id.clone(),
                    revision: target.revision,
                    digest: target.digest.clone(),
                    subject: message.subject,
                    body: message.body,
                    repin_recipient_session: None,
                })
                .map_err(|error| match error {
                    crate::mailbox::MailboxError::EditConflict
                    | crate::mailbox::MailboxError::HeadClaimed => {
                        SendRefusal::PendingChanged(listed(&pending))
                    }
                    error => SendRefusal::Store(error.to_string()),
                })?;
            return Ok(SendRoute::Mailbox(MessageDelivery {
                path: "mailbox".into(),
                stable_id: Some(edited.stable_id),
                revision: Some(edited.revision),
                edited: true,
                duplicate: false,
            }));
        }

        let accepted_at = now_secs();
        let digest = sha256_fields(&[
            stable_id.as_bytes(),
            &1_u64.to_be_bytes(),
            message.subject.as_bytes(),
            message.body.as_bytes(),
        ]);
        let head = MailboxHead {
            delivery_digest: sha256_fields(&[b"herdr-send-delivery", stable_id.as_bytes()]),
            stable_id,
            revision: 1,
            digest,
            recipient_generation: recipient.generation.clone(),
            recipient: recipient.clone(),
            subject: message.subject,
            body: message.body,
            sender: sender_key.clone(),
            target: recipient_terminal.to_string(),
            grant_id: format!("send:{sender_key}"),
            message_id,
            kind: message.kind,
            priority: message.priority,
            original_sequence: 1,
            enqueue_epoch: 0,
            accepted_at,
            delivery: Some(ServerDelivery {
                origin: message.origin.into(),
                sender_label: sender.label.clone(),
                sender_session: sender.session.clone(),
                recipient_session,
                correlation: message.correlation,
            }),
        };
        let receipt = store
            .append_offline_head(head)
            .map_err(|error| SendRefusal::Store(error.to_string()))?;
        Ok(SendRoute::Mailbox(MessageDelivery {
            path: "mailbox".into(),
            stable_id: Some(receipt.stable_id),
            revision: Some(receipt.revision),
            edited: false,
            duplicate: false,
        }))
    }
}

pub(crate) fn pending_error_json(id: String, refusal: &SendRefusal) -> Option<String> {
    let (code, message, pending) = match refusal {
        SendRefusal::PendingExists(pending) => (
            "pending_exists",
            "you already have messages waiting for this recipient; resend with --edit-pending <stableId> or --send-new",
            pending,
        ),
        SendRefusal::PendingChanged(pending) => (
            "pending_claimed",
            "that message is no longer waiting at the expected revision; see pending for what is still waiting",
            pending,
        ),
        _ => return None,
    };
    let error = serde_json::json!({"code": code, "message": message,
                                   "pending": serde_json::to_value(pending).ok()?});
    Some(serde_json::json!({"id": id, "error": error}).to_string())
}

#[cfg(target_os = "linux")]
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(target_os = "linux"))]
fn parent_pid(_pid: u32) -> Option<u32> {
    None
}
