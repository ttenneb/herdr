//! Draft guard for typed (PTY) delivery.
//!
//! Typing a message into a pane whose editor holds the human's unsent text
//! appends to that draft, and the submit Enter sends both. Herdr therefore
//! keeps a per-pane estimate of pending human input since the last Enter
//! (only human input forwarded from an attached client counts), and while it
//! is positive every typed delivery to the pane is held in memory, in order,
//! and typed as soon as it drops to zero. After [`TYPED_DEFERRAL_LIMIT`] a
//! held delivery fails with `agent_input_busy`, reported to the sender. When
//! a Pi publishes its own "editor has unsent text" flag, that wins for Pi
//! panes (not wired yet).

use std::time::{Duration, Instant};

use bytes::Bytes;
use crossterm::event::{KeyCode, KeyModifiers};

use super::App;
use crate::terminal::TerminalId;

/// How long a typed delivery waits for the human's draft to clear.
pub(crate) const TYPED_DEFERRAL_LIMIT: Duration = Duration::from_secs(600);
const TYPED_SUBMIT_DELAY: Duration = Duration::from_millis(300);

/// Whether the human may have unsent input in one pane since its last
/// submit/clear key (`count > 0`: possibly present).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct HumanDraft {
    pub count: usize,
    /// Wall-clock Unix ms of the last human key or text counted into this
    /// draft; a Pi editor sample older than this cannot clear it.
    pub last_key_at_ms: u64,
    /// The agent process generation the count belongs to; a new agent
    /// process starts with an empty editor.
    pub process_generation: Option<u64>,
}

/// One held typed delivery.
#[derive(Debug, Clone)]
pub(crate) struct TypedDeferral {
    pub id: String,
    pub terminal_id: TerminalId,
    pub text: String,
    pub expected_agent: crate::detect::Agent,
    /// Target as the sender named it (for messages and logs).
    pub target: String,
    pub sender: crate::app::messages::SenderAttribution,
    pub origin: &'static str,
    pub deadline: Instant,
    /// The recipient agent's execution when the message was sent (managed
    /// generation and foreground process PID plus birth tick). A held
    /// message is only ever typed into that same execution.
    pub execution: Option<String>,
    /// Why it is typed rather than queued (for the history record).
    pub reason: String,
}

/// How a human keystroke changes the draft state: `None` clears it,
/// `Some(0)` leaves it, `Some(1)` marks a draft as possibly present.
///
/// Any forwarded key may put text in the editor (history recall with Up or
/// Ctrl-R, Tab completion, Ctrl-Y, agent autocomplete, Backspace over a
/// submitted line...), so every non-release key marks the draft except the
/// known submit/clear keys: a plain unmodified Enter, Ctrl-C and Ctrl-U.
/// Ctrl-J, Ctrl-M and modified Enter insert newlines; Esc proves nothing.
fn key_effect(key: &crate::input::TerminalKey) -> Option<isize> {
    if key.kind == crossterm::event::KeyEventKind::Release {
        return Some(0);
    }
    match key.code {
        KeyCode::Enter if key.modifiers.is_empty() => None,
        KeyCode::Char(c)
            if key.modifiers == KeyModifiers::CONTROL
                && matches!(c.to_ascii_lowercase(), 'c' | 'u') =>
        {
            None
        }
        _ => Some(1),
    }
}

fn new_deferral_id() -> String {
    crate::platform::random_route_epoch()
        .map(|hex| format!("defer.{hex}"))
        .unwrap_or_else(|| {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            format!(
                "defer.{:016x}{:016x}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )
        })
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl App {
    /// Pi reported a true→false editor edge sampled at `sampled_at_ms`: clear
    /// Herdr's key-based draft flag only if no human key was counted after
    /// that sample (a stale, in-flight "false" must not erase a newer draft).
    pub(crate) fn clear_human_draft_if_older_than(
        &mut self,
        terminal_id: &TerminalId,
        sampled_at_ms: Option<u64>,
    ) {
        let Some(sampled_at_ms) = sampled_at_ms else {
            return;
        };
        if self
            .human_drafts
            .get(terminal_id)
            .is_none_or(|draft| draft.last_key_at_ms < sampled_at_ms)
        {
            self.clear_human_draft(terminal_id);
        }
    }

    fn terminal_of_pane(&self, pane_id: crate::layout::PaneId) -> Option<TerminalId> {
        let (ws_idx, _) = self.find_pane(pane_id)?;
        self.state.terminal_id_for_pane(ws_idx, pane_id)
    }

    /// A human key forwarded from an attached client reached this terminal.
    pub(crate) fn note_human_key(
        &mut self,
        terminal_id: &TerminalId,
        key: &crate::input::TerminalKey,
    ) {
        match key_effect(key) {
            None => self.clear_human_draft(terminal_id),
            Some(0) => {}
            Some(delta) => {
                let draft = self.human_drafts.entry(terminal_id.clone()).or_default();
                draft.count = draft.count.saturating_add_signed(delta);
                draft.last_key_at_ms = unix_ms();
                if draft.process_generation.is_none() {
                    draft.process_generation = self
                        .state
                        .terminals
                        .get(terminal_id)
                        .and_then(|terminal| terminal.managed_agent_generation());
                }
            }
        }
    }

    /// Human text reached this terminal: a paste (`paste`: bracketed, so a
    /// trailing newline stays in the unsent draft) or a text commit, where a
    /// trailing carriage return is a raw Enter byte that submits.
    pub(crate) fn note_human_text(&mut self, terminal_id: &TerminalId, text: &str, paste: bool) {
        if !paste && text.ends_with('\r') {
            self.clear_human_draft(terminal_id);
            return;
        }
        let added = text.chars().filter(|c| *c != '\u{1b}').count();
        if added > 0 {
            let draft = self.human_drafts.entry(terminal_id.clone()).or_default();
            draft.count = draft.count.saturating_add(added);
            draft.last_key_at_ms = unix_ms();
        }
    }

    pub(crate) fn clear_human_draft(&mut self, terminal_id: &TerminalId) {
        let had = self.human_drafts.remove(terminal_id).is_some();
        if had
            || self
                .typed_deferrals
                .iter()
                .any(|d| &d.terminal_id == terminal_id)
        {
            self.flush_typed_deferrals(Instant::now());
        }
    }

    /// Agent process changes clear the pane's draft estimate.
    pub(crate) fn note_agent_process_event(&mut self, event: &crate::events::AppEvent) {
        match event {
            crate::events::AppEvent::AgentProcessDetected {
                pane_id,
                process_generation,
                ..
            } => {
                if let Some(terminal_id) = self.terminal_of_pane(*pane_id) {
                    if self
                        .human_drafts
                        .get(&terminal_id)
                        .is_some_and(|draft| draft.process_generation != Some(*process_generation))
                    {
                        self.clear_human_draft(&terminal_id);
                    }
                }
            }
            crate::events::AppEvent::StateChanged {
                pane_id,
                process_exited: true,
                ..
            }
            | crate::events::AppEvent::PaneDied { pane_id, .. } => {
                if let Some(terminal_id) = self.terminal_of_pane(*pane_id) {
                    if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                        terminal.editor_has_text = None;
                    }
                    self.clear_human_draft(&terminal_id);
                }
            }
            _ => {}
        }
    }

    /// Whether the pane's editor may hold the human's unsent text: a Pi's
    /// `editor_has_text=true` report, OR Herdr's own key-based flag (a stale
    /// false from Pi never overrides fresh keys; Pi's true→false edge clears
    /// Herdr's flag when it arrives).
    pub(crate) fn pane_draft_pending(&self, terminal_id: &TerminalId) -> bool {
        let reported = self
            .state
            .terminals
            .get(terminal_id)
            .is_some_and(|terminal| {
                terminal.effective_known_agent() == Some(crate::detect::Agent::Pi)
                    && terminal.editor_has_text == Some(true)
            });
        reported
            || self
                .human_drafts
                .get(terminal_id)
                .is_some_and(|draft| draft.count > 0)
    }

    /// Whether a typed delivery to this terminal must be held: a draft is
    /// pending, or earlier held deliveries still wait (order is kept).
    pub(crate) fn typed_delivery_must_wait(&self, terminal_id: &TerminalId) -> bool {
        self.pane_draft_pending(terminal_id)
            || self
                .typed_deferrals
                .iter()
                .any(|deferral| &deferral.terminal_id == terminal_id)
    }

    pub(crate) fn defer_typed_delivery(
        &mut self,
        terminal_id: TerminalId,
        text: String,
        expected_agent: crate::detect::Agent,
        target: String,
        sender: crate::app::messages::SenderAttribution,
        origin: &'static str,
        reason: String,
    ) -> String {
        let id = new_deferral_id();
        let execution = self.current_agent_execution(&terminal_id);
        tracing::info!(
            deferral = %id,
            terminal = %terminal_id,
            sender = %sender.label,
            "typed delivery held: the pane has unsent human input"
        );
        self.typed_deferrals.push(TypedDeferral {
            id: id.clone(),
            terminal_id,
            text,
            expected_agent,
            target,
            sender,
            origin,
            deadline: Instant::now() + TYPED_DEFERRAL_LIMIT,
            execution,
            reason,
        });
        id
    }

    /// The pane's current agent identity: its managed generation (if any),
    /// its foreground agent process as PID plus kernel birth tick, its agent
    /// name, and its agent session (for a handoff, the envelope's recipient
    /// session, which was verified current at send time).
    pub(crate) fn current_agent_execution(&self, terminal_id: &TerminalId) -> Option<String> {
        let generation = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.managed_agent_generation())
            .filter(|generation| *generation > 0);
        let process = self
            .mailbox_bootstrap_foreground_job(terminal_id)
            .and_then(|job| {
                crate::detect::identify_agent_process_in_job(&job).map(|(_, process)| process.pid)
            })
            .and_then(|pid| {
                self.managed_pi_process_birth(pid)
                    .map(|birth| (pid, birth.start_ticks))
            });
        let name = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.agent_name.clone());
        let session = self.current_agent_session_value(&terminal_id.to_string());
        if generation.is_none() && process.is_none() && name.is_none() && session.is_none() {
            return None;
        }
        // Execution, addressed name and agent session: a held message goes
        // only to exactly the agent it was sent to.
        Some(format!(
            "g{}/p{}/n{}/s{}",
            generation.map_or_else(|| "-".into(), |g| g.to_string()),
            process.map_or_else(|| "-".into(), |(pid, ticks)| format!("{pid}:{ticks}")),
            name.as_deref().unwrap_or("-"),
            session.as_deref().unwrap_or("-"),
        ))
    }

    /// Why a message to this terminal is typed rather than queued.
    pub(crate) fn typed_reason(
        &self,
        terminal_id: &TerminalId,
        transport: Option<crate::api::schema::MessageTransport>,
    ) -> String {
        if transport == Some(crate::api::schema::MessageTransport::Pty) {
            return "explicit_pty".into();
        }
        let key = terminal_id.to_string();
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return "no_messages".into();
        };
        if matches!(terminal.effective_known_agent(), Some(agent) if agent != crate::detect::Agent::Pi)
        {
            return "not_pi".into();
        }
        if terminal.messages_capable && !self.pane_takes_messages(&key) {
            return "fallback_30s".into();
        }
        "no_messages".into()
    }

    /// Durable history for a message typed into a pane: a head in the
    /// pane's queue, never claimable, settled at once with closedBy "typed"
    /// and `delivery.typedReason`, so Messages history shows typed
    /// deliveries next to queued ones. Best effort: a failure is logged.
    pub(crate) fn record_typed_delivery(
        &mut self,
        terminal_id: &TerminalId,
        sender: &crate::app::messages::SenderAttribution,
        text: &str,
        reason: &str,
        origin: &'static str,
    ) {
        // History is for Pi panes (Messages); other agents keep none.
        if reason == "not_pi" {
            return;
        }
        let key = terminal_id.to_string();
        let Some(queue_key) = self.pane_queue_key(&key) else {
            return;
        };
        if let Err(err) = crate::app::messages::append_typed_history(
            &self.sender_authority_dir,
            &queue_key,
            &key,
            sender,
            text,
            reason,
            origin,
        ) {
            tracing::warn!(terminal = %terminal_id, %err, "could not record the typed delivery in history");
        }
    }

    pub(crate) fn next_typed_deferral_deadline(&self) -> Option<Instant> {
        self.typed_deferrals.iter().map(|d| d.deadline).min()
    }

    /// Types held deliveries whose pane has no pending draft (in order) and
    /// fails those past their deadline.
    pub(crate) fn flush_typed_deferrals(&mut self, now: Instant) -> bool {
        if self.typed_deferrals.is_empty() {
            return false;
        }
        let pending = std::mem::take(&mut self.typed_deferrals);
        let mut keep = Vec::new();
        let mut blocked: Vec<TerminalId> = Vec::new();
        let mut changed = false;
        for deferral in pending {
            if blocked.contains(&deferral.terminal_id) {
                keep.push(deferral);
                continue;
            }
            if self.pane_draft_pending(&deferral.terminal_id) {
                if now >= deferral.deadline {
                    self.fail_typed_deferral(
                        &deferral,
                        "agent_input_busy",
                        "the recipient pane had unsent human input for 10 minutes; the message was not typed",
                    );
                    changed = true;
                } else {
                    blocked.push(deferral.terminal_id.clone());
                    keep.push(deferral);
                }
                continue;
            }
            if self.current_agent_execution(&deferral.terminal_id) != deferral.execution {
                self.fail_typed_deferral(
                    &deferral,
                    "agent_replaced",
                    "the agent in the recipient pane was replaced while the message waited; it was not typed into the new agent",
                );
                changed = true;
                continue;
            }
            match self.type_submission(
                &deferral.terminal_id,
                deferral.expected_agent,
                &deferral.text,
            ) {
                Ok(()) => {
                    tracing::info!(deferral = %deferral.id, "held typed delivery typed");
                    self.record_typed_delivery(
                        &deferral.terminal_id,
                        &deferral.sender,
                        &deferral.text,
                        &deferral.reason,
                        deferral.origin,
                    );
                    self.emit_deferral_event(&deferral, "delivered", None, None);
                }
                Err((code, message)) => self.fail_typed_deferral(&deferral, code, &message),
            }
            changed = true;
        }
        keep.extend(std::mem::take(&mut self.typed_deferrals));
        self.typed_deferrals = keep;
        changed
    }

    /// On server stop: every held delivery fails (the hold is in memory).
    pub(crate) fn drop_typed_deferrals_on_stop(&mut self) {
        for deferral in std::mem::take(&mut self.typed_deferrals) {
            self.fail_typed_deferral(
                &deferral,
                "agent_input_busy",
                "the Herdr server stopped while the message waited for the recipient's unsent input; it was not typed",
            );
        }
    }

    fn fail_typed_deferral(&mut self, deferral: &TypedDeferral, code: &str, message: &str) {
        tracing::warn!(
            deferral = %deferral.id,
            terminal = %deferral.terminal_id,
            target = %deferral.target,
            sender = %deferral.sender.label,
            sender_terminal = ?deferral.sender.terminal,
            code,
            "held typed delivery failed: {message}"
        );
        self.emit_deferral_event(deferral, "failed", Some(code), Some(message));
        // A sender Pi with Messages learns it in its own inbox (durable).
        let Some(sender_terminal) = deferral.sender.terminal.clone() else {
            return;
        };
        if !self.pane_takes_messages(&sender_terminal) {
            return;
        }
        let excerpt: String = deferral.text.chars().take(2000).collect();
        let note = crate::app::messages::OutgoingMessage {
            origin: deferral.origin,
            subject: format!("Not delivered to {}", deferral.target),
            body: format!(
                "Your message to {} was not delivered ({code}): {message}.\n\nMessage:\n{excerpt}",
                deferral.target
            ),
            priority: "normal".into(),
            kind: "advisory".into(),
            message_id: Some(format!("{}.failed", deferral.id)),
            correlation: None,
            replace_pending: false,
        };
        let herdr = crate::app::messages::SenderAttribution {
            terminal: None,
            label: "herdr".into(),
            session: None,
            external_key: None,
        };
        let options = crate::api::schema::MessageSendOptions {
            send_new: true,
            ..Default::default()
        };
        if let Err(err) = self.route_ordinary_send(&sender_terminal, &herdr, note, &options) {
            tracing::warn!(deferral = %deferral.id, ?err, "could not queue the failure note for the sender");
        }
    }

    fn emit_deferral_event(
        &mut self,
        deferral: &TypedDeferral,
        outcome: &str,
        code: Option<&str>,
        message: Option<&str>,
    ) {
        let (pane_id, workspace_id) = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| pane.attached_terminal_id == deferral.terminal_id)
                        .map(|(pane_id, _)| (ws_idx, *pane_id))
                })
            })
            .map(|(ws_idx, pane_id)| {
                (
                    self.public_pane_id(ws_idx, pane_id).unwrap_or_default(),
                    self.public_workspace_id(ws_idx),
                )
            })
            .unwrap_or_default();
        let (event, data) = match outcome {
            "delivered" => (
                crate::api::schema::EventKind::DeliveryDeferredDelivered,
                crate::api::schema::EventData::DeliveryDeferredDelivered {
                    deferral_id: deferral.id.clone(),
                    pane_id,
                    workspace_id,
                    terminal_id: deferral.terminal_id.to_string(),
                },
            ),
            _ => (
                crate::api::schema::EventKind::DeliveryDeferredFailed,
                crate::api::schema::EventData::DeliveryDeferredFailed {
                    deferral_id: deferral.id.clone(),
                    pane_id,
                    workspace_id,
                    terminal_id: deferral.terminal_id.to_string(),
                    sender: deferral.sender.label.clone(),
                    sender_terminal_id: deferral.sender.terminal.clone(),
                    code: code.unwrap_or("agent_input_busy").to_string(),
                    message: message.unwrap_or_default().to_string(),
                },
            ),
        };
        self.emit_event(crate::api::schema::EventEnvelope { event, data });
    }

    /// Types one prompt submission into the terminal's agent, after checking
    /// it still hosts the expected agent.
    pub(crate) fn type_submission(
        &mut self,
        terminal_id: &TerminalId,
        expected_agent: crate::detect::Agent,
        text: &str,
    ) -> Result<(), (&'static str, String)> {
        let located = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| &pane.attached_terminal_id == terminal_id)
                        .map(|(pane_id, _)| (ws_idx, *pane_id))
                })
            });
        let Some((ws_idx, pane_id)) = located else {
            return Err(("agent_not_found", "the recipient pane is gone".into()));
        };
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Err((
                "agent_not_found",
                "the recipient pane has no terminal".into(),
            ));
        };
        if !super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err((
                "agent_not_ready",
                "the recipient agent is no longer the pane foreground process".into(),
            ));
        }
        if expected_agent == crate::detect::Agent::GithubCopilot {
            let focus = crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained)
                .map_err(|err| ("agent_prompt_failed", err.to_string()))?;
            runtime
                .try_send_bytes(Bytes::from(focus))
                .map_err(|err| ("agent_prompt_failed", err.to_string()))?;
        }
        let (text, enter) = crate::app::api_helpers::encode_api_submission_parts(runtime, text);
        runtime
            .try_send_prompt_transaction(Bytes::from(text), Bytes::from(enter), TYPED_SUBMIT_DELAY)
            .map_err(|err| {
                let code = match err {
                    crate::pane::PromptTransactionAdmissionError::Full => "agent_prompt_queue_full",
                    crate::pane::PromptTransactionAdmissionError::PayloadTooLarge => {
                        "agent_prompt_payload_too_large"
                    }
                    _ => "agent_prompt_failed",
                };
                (code, err.to_string())
            })?;
        if let Some(restore) = self.begin_archived_member_input(ws_idx, pane_id) {
            self.commit_archived_member_input(restore);
        }
        self.acknowledge_terminal_input(terminal_id);
        Ok(())
    }
}
