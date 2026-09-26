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

/// The re-delivery copy for an explicit Retry: same text, sender, recipient
/// and priority; a new stable ID derived from the old head and its claim.
pub(crate) fn retry_head(head: &MailboxHead, claim: &crate::mailbox::Claim) -> MailboxHead {
    let stable_id = format!(
        "retry.{}",
        &sha256_fields(&[head.stable_id.as_bytes(), claim.claim_id.as_bytes()])[..32]
    );
    let mut delivery = head.delivery.clone().unwrap_or(ServerDelivery {
        origin: "retry".into(),
        sender_label: head.sender.clone(),
        sender_session: None,
        recipient_session: None,
        correlation: None,
        retry_of: None,
    });
    delivery.retry_of = Some(head.stable_id.clone());
    MailboxHead {
        digest: sha256_fields(&[
            stable_id.as_bytes(),
            &1_u64.to_be_bytes(),
            head.subject.as_bytes(),
            head.body.as_bytes(),
        ]),
        delivery_digest: sha256_fields(&[b"herdr-retry-delivery", stable_id.as_bytes()]),
        revision: 1,
        message_id: format!("retry:{}", head.message_id),
        enqueue_epoch: 0,
        accepted_at: now_secs(),
        delivery: Some(delivery),
        stable_id,
        ..head.clone()
    }
}

/// A head for the human's own typing at a pane (`mailbox.enqueue_self`):
/// addressed to that pane's inbox only, sender `human@<terminal>`. The same
/// `clientId` yields the same stable ID, so a retry never queues twice.
pub(crate) fn human_self_head(
    inbox: &RecipientKey,
    terminal_key: &str,
    subject: String,
    body: String,
    priority: String,
    client_id: Option<String>,
    recipient_session: Option<String>,
) -> Option<MailboxHead> {
    if !matches!(priority.as_str(), "low" | "normal" | "high")
        || !mailbox_safe_text(&subject, &body)
        || client_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 128 || id.chars().any(char::is_control))
    {
        return None;
    }
    let sender = format!("human@{terminal_key}");
    let message_id = client_id
        .or_else(crate::platform::random_route_epoch)
        .unwrap_or_else(|| format!("self-{}", now_secs()));
    let stable_id = format!(
        "self.{}",
        &sha256_fields(&[
            inbox.recipient_id.as_bytes(),
            sender.as_bytes(),
            message_id.as_bytes()
        ])[..32]
    );
    Some(MailboxHead {
        digest: sha256_fields(&[
            stable_id.as_bytes(),
            &1_u64.to_be_bytes(),
            subject.as_bytes(),
            body.as_bytes(),
        ]),
        delivery_digest: sha256_fields(&[b"herdr-self-delivery", stable_id.as_bytes()]),
        stable_id,
        revision: 1,
        recipient_generation: inbox.generation.clone(),
        recipient: inbox.clone(),
        subject,
        body,
        sender: sender.clone(),
        target: terminal_key.to_string(),
        grant_id: format!("self:{terminal_key}"),
        message_id,
        kind: "advisory".into(),
        priority,
        original_sequence: 1,
        enqueue_epoch: 0,
        accepted_at: now_secs(),
        delivery: Some(ServerDelivery {
            origin: "human_typed".into(),
            sender_label: "human at pane".into(),
            sender_session: None,
            recipient_session,
            correlation: None,
            retry_of: None,
        }),
    })
}

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

/// The per-pane inbox view for one attached Pi execution: every recipient
/// key of the pane (its durable queue key plus the legacy terminal key),
/// withdrawn heads removed, the session pin shown as display information,
/// and claims marked as this execution's (`current`) or another's (`other`).
/// `claim` is only this execution's outstanding claim.
pub(crate) fn inbox_snapshot(
    recovered: &RecoveredMailbox,
    recipients: &[RecipientKey],
    execution: &str,
    current_session: Option<&str>,
) -> Result<crate::mailbox_v1::Snapshot, crate::mailbox::MailboxError> {
    let mut out = crate::mailbox_v1::Snapshot {
        heads: Vec::new(),
        receipts: Vec::new(),
        head_states: Vec::new(),
        claim: None,
    };
    for recipient in recipients {
        let part = crate::mailbox_v1::snapshot(recovered, recipient)?;
        out.heads.extend(part.heads);
        out.receipts.extend(part.receipts);
        out.head_states.extend(part.head_states);
    }
    let withdrawn: std::collections::HashSet<String> = out
        .heads
        .iter()
        .filter(|head| {
            recovered
                .claims
                .get(&head.stable_id)
                .is_some_and(is_withdrawn_claim)
        })
        .map(|head| head.stable_id.clone())
        .collect();
    out.heads
        .retain(|head| !withdrawn.contains(&head.stable_id));
    out.receipts
        .retain(|receipt| !withdrawn.contains(&receipt.stable_id));
    out.head_states
        .retain(|state| !withdrawn.contains(&state.stable_id));
    for state in &mut out.head_states {
        state.previous_session = state.lifecycle == crate::mailbox_v1::HeadLifecycle::Held
            && state
                .recipient_session
                .as_deref()
                .is_some_and(|pinned| current_session != Some(pinned));
        state.claim_execution = recovered.claims.get(&state.stable_id).map(|claim| {
            if claim_is_current(claim, execution) {
                "current".into()
            } else {
                "other".into()
            }
        });
        state.recovery_needed = state.claim_execution.as_deref() == Some("other")
            && matches!(
                state.lifecycle,
                crate::mailbox_v1::HeadLifecycle::Claimed
                    | crate::mailbox_v1::HeadLifecycle::Admitted
            );
    }
    out.claim = out
        .head_states
        .iter()
        .filter(|state| {
            matches!(
                state.lifecycle,
                crate::mailbox_v1::HeadLifecycle::Claimed
                    | crate::mailbox_v1::HeadLifecycle::Admitted
            ) && state.claim_execution.as_deref() == Some("current")
        })
        .find_map(|state| recovered.claims.get(&state.stable_id).cloned());
    Ok(out)
}

/// Legacy claims without an execution belong to whichever Pi holds the pane.
pub(crate) fn claim_is_current(claim: &crate::mailbox::Claim, execution: &str) -> bool {
    claim
        .execution
        .as_deref()
        .is_none_or(|owner| owner == execution)
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

/// The durable per-pane recipient key (`pane:<queueKey>`).
pub(crate) fn pane_recipient(queue_key: &str) -> RecipientKey {
    RecipientKey {
        recipient_id: format!("pane:{queue_key}"),
        generation: "1".into(),
    }
}

/// Stable identity of the Pi execution behind a bootstrap session; claims are
/// bound to it so two Pis can never both claim one head.
pub(crate) fn session_execution(session: &crate::app::MailboxBootstrapSession) -> String {
    match session.recipient_only {
        Some(binding) => format!("pid:{}:{}", binding.foreground_pid, binding.start_ticks),
        None => format!(
            "managed:{}:{}",
            session.caller, session.active_execution_generation
        ),
    }
}

impl App {
    /// Every recipient key a pane's inbox covers: its durable queue key and
    /// the legacy terminal key used by older Pi-to-Pi grants.
    pub(crate) fn inbox_recipients(&self, terminal_key: &str) -> Vec<RecipientKey> {
        let mut keys = Vec::new();
        if let Some(terminal) = self
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == terminal_key)
        {
            keys.push(pane_recipient(&terminal.queue_key));
        }
        keys.push(RecipientKey {
            recipient_id: terminal_key.to_string(),
            generation: "1".into(),
        });
        keys
    }

    pub(crate) fn pane_queue_key(&self, terminal_key: &str) -> Option<String> {
        self.state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == terminal_key)
            .map(|terminal| terminal.queue_key.clone())
    }

    /// A pane gets a Messages queue when its foreground agent is Pi, or when
    /// no agent process is attached yet but the pane is a Pi pane (managed Pi
    /// launch, or its last reported session was Pi: restarting or asleep).
    /// A pane whose foreground agent is another agent keeps PTY input.
    pub(crate) fn pane_takes_messages(&self, terminal_key: &str) -> bool {
        let Some(terminal) = self
            .state
            .terminals
            .values()
            .find(|terminal| terminal.id.to_string() == terminal_key)
        else {
            return false;
        };
        if matches!(terminal.effective_known_agent(), Some(agent) if agent != crate::detect::Agent::Pi)
        {
            return false;
        }
        // A Pi attached right now, or a pane whose Pi attached before and is
        // restarting or asleep. A Pi that never attached keeps typed input.
        self.attached_messages_recipient(terminal_key).is_some() || terminal.messages_capable
    }

    /// A public pane ID whose pane has a Messages queue but no agent process
    /// right now (its Pi exited, is restarting or asleep).
    pub(crate) fn messages_queue_target(
        &self,
        target: &str,
        options: &MessageSendOptions,
    ) -> Option<crate::app::terminal_targets::TerminalTarget> {
        if options.transport == Some(MessageTransport::Pty) {
            return None;
        }
        let (ws_idx, pane_id) = self.parse_current_public_pane_id(target)?;
        let resolved = self.terminal_target_for_pane(ws_idx, pane_id)?;
        self.pane_takes_messages(&resolved.terminal_id)
            .then_some(resolved)
    }

    /// The `agent prompt` result for a queue-only pane (no agent process).
    pub(crate) fn queue_only_agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let pane = self.pane_info(ws_idx, pane_id)?;
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: None,
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: false,
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session: None,
            agent_session_trust: None,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: false,
            interactive_ready: false,
            state_change_seq: 0,
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    /// Wake hook: a head was appended for a pane with no attached Pi. Emits
    /// `pane.wake_requested`; the sleep/wake owner (#25) acts on it.
    pub(crate) fn request_pane_wake_if_detached(&mut self, terminal_key: &str, stable_id: &str) {
        if self.attached_messages_recipient(terminal_key).is_some() {
            return;
        }
        let Some(queue_key) = self.pane_queue_key(terminal_key) else {
            return;
        };
        let Some((ws_idx, pane_id)) =
            self.state
                .workspaces
                .iter()
                .enumerate()
                .find_map(|(ws_idx, workspace)| {
                    workspace.tabs.iter().find_map(|tab| {
                        tab.panes
                            .iter()
                            .find(|(_, pane)| pane.attached_terminal_id.to_string() == terminal_key)
                            .map(|(pane_id, _)| (ws_idx, *pane_id))
                    })
                })
        else {
            return;
        };
        let Some(public_pane) = self.public_pane_id(ws_idx, pane_id) else {
            return;
        };
        let workspace_id = self.public_workspace_id(ws_idx);
        tracing::info!(pane = %public_pane, stable_id, "messages: wake requested for a pane with no attached Pi");
        self.emit_event(crate::api::schema::EventEnvelope {
            event: crate::api::schema::EventKind::PaneWakeRequested,
            data: crate::api::schema::EventData::PaneWakeRequested {
                pane_id: public_pane,
                workspace_id,
                terminal_id: terminal_key.to_string(),
                queue_key,
                stable_id: stable_id.to_string(),
                reason: "message_queued".into(),
            },
        });
    }

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
        // Every Pi pane has a queue, whether or not a Pi is attached right now.
        let Some(queue_key) = self
            .pane_queue_key(recipient_terminal)
            .filter(|_| self.pane_takes_messages(recipient_terminal))
        else {
            return match transport {
                MessageTransport::Mailbox => Err(SendRefusal::MailboxUnavailable(
                    "the recipient pane is not a Pi pane",
                )),
                _ => Ok(SendRoute::Pty),
            };
        };
        let recipient = pane_recipient(&queue_key);
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
                retry_of: None,
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
