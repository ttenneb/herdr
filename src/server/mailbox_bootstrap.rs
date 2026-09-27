//! Authenticated bootstrap for the Pi mailbox projection.
//!
//! The socket path is only a discovery mechanism.  A connection becomes a
//! mailbox channel only after the App verifies its peer credentials against a
//! live foreground Pi process and commits an Active sender generation.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::app::{App, MailboxBootstrapError, MailboxBootstrapSession};
use crate::ipc::{remove_socket_file_if_owned, socket_file_identity, SocketFileIdentity};

pub(crate) const MAILBOX_BOOTSTRAP_PROTOCOL_VERSION: u16 = 1;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The address is deliberately discoverable but conveys no authority.
pub(crate) fn mailbox_bootstrap_socket_path() -> PathBuf {
    let api = crate::api::socket_path();
    let stem = api
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("herdr");
    api.parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("{stem}-mailbox-bootstrap.sock"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MailboxBootstrapDescriptor {
    pub protocol_version: u16,
    /// The typed, server-scoped report path available on this accepted stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_submit: Option<ReportSubmitAdvertisement>,
    /// Read-only terminal history for this exact current Pi execution. It
    /// cannot expose pending heads or mint a grant for old work.
    pub history_snapshot: ReportSubmitAdvertisement,
    pub history_only: bool,
    /// `managed` (trusted managed launch), `history_only`, or
    /// `recipient_only` (no trusted launch: own inbox only, and no sender,
    /// report or route advertisement is ever present).
    pub binding: &'static str,
    /// Own-inbox methods beyond the v1 set: `mailbox.watch` (long-poll),
    /// and `mailbox.drop` (the human's Drop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<MessagesAdvertisement>,
    /// Offered only when the server can validate an exact active delegation
    /// parent. Admission here is a durable mailbox receipt, not Pi Gate
    /// admission or the parent's acceptance of the report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_report: Option<ParentReportAdvertisement>,
    /// C2, managed sessions only: this pane as a delegation parent can read
    /// "child reported done, no report seen yet" facts for its bound routes.
    /// Deliberately NOT `parentSignals`: installed Pi (d74c3a4) validates that
    /// key strictly (it requires the #115 recovery/bind methods) and would
    /// turn Messages off; a new key is ignored by an older Pi.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_done_signals: Option<ChildDoneSignalsAdvertisement>,
    pub endpoint: String,
    pub caller: String,
    pub recipient: crate::mailbox::RecipientKey,
    pub grant_id: String,
    pub active_execution_generation: u64,
    pub binding_generation: String,
    pub request_id_policy: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ChildDoneSignalsAdvertisement {
    pub method: &'static str,
    pub protocol: &'static str,
    pub max_wait_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessagesAdvertisement {
    pub watch_method: &'static str,
    pub watch_max_wait_ms: u64,
    /// The pane's durable queue: every message addressed to this pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inbox: Option<crate::mailbox::RecipientKey>,
    pub drop_method: &'static str,
    /// Re-deliver a head an ended Pi execution left in recovery.
    pub retry_method: &'static str,
    /// Change a held head's priority.
    pub reprioritize_method: &'static str,
    /// The human's own typing at this pane, queued in this pane's inbox only.
    pub enqueue_self_method: &'static str,
    pub protocol: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportSubmitAdvertisement {
    pub method: &'static str,
    pub protocol: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParentReportAdvertisement {
    pub method: &'static str,
    pub todo_state_method: &'static str,
    /// Optional opt-in accepted-child pre-effect observation; not a gen1
    /// sender-origin or all-path qualification.
    pub path_attempt_method: &'static str,
    pub prepared_method: &'static str,
    pub coverage_method: &'static str,
    /// A durable observation is not a trusted all-path closure certificate.
    pub coverage_qualified: bool,
    pub protocol: &'static str,
    pub recipient: crate::mailbox::RecipientKey,
    pub grant_id: String,
}

/// The descriptor a session would receive, as JSON (tests and diagnostics).
#[cfg(test)]
pub(crate) fn descriptor_value(session: &MailboxBootstrapSession) -> Value {
    serde_json::to_value(MailboxBootstrapDescriptor::from_session(
        session,
        &mailbox_bootstrap_socket_path(),
    ))
    .expect("descriptor serializes")
}

impl MailboxBootstrapDescriptor {
    fn from_session(session: &MailboxBootstrapSession, endpoint: &Path) -> Self {
        Self {
            protocol_version: MAILBOX_BOOTSTRAP_PROTOCOL_VERSION,
            report_submit: (!session.history_only && session.recipient_only.is_none()).then_some(
                ReportSubmitAdvertisement {
                    method: "report_submit",
                    protocol: crate::mailbox_v1::PROTOCOL,
                },
            ),
            history_snapshot: ReportSubmitAdvertisement {
                method: "mailbox.history_snapshot",
                protocol: crate::mailbox_v1::PROTOCOL,
            },
            history_only: session.history_only,
            binding: if session.recipient_only.is_some() {
                "recipient_only"
            } else if session.history_only {
                "history_only"
            } else {
                "managed"
            },
            messages: (!session.history_only).then_some(MessagesAdvertisement {
                watch_method: "mailbox.watch",
                watch_max_wait_ms: MAX_WATCH_TIMEOUT_MS,
                inbox: session.pane_inbox.clone(),
                drop_method: "mailbox.drop",
                retry_method: "mailbox.retry",
                reprioritize_method: "mailbox.reprioritize",
                enqueue_self_method: "mailbox.enqueue_self",
                protocol: crate::mailbox_v1::PROTOCOL,
            }),
            parent_report: session
                .parent_report
                .as_ref()
                .map(|route| ParentReportAdvertisement {
                    method: "report_submit_parent",
                    todo_state_method: "todo_state",
                    path_attempt_method: "report_path_attempt",
                    prepared_method: "report_prepared",
                    coverage_method: "report_coverage",
                    coverage_qualified: false,
                    protocol: crate::mailbox_v1::PROTOCOL,
                    recipient: route.recipient.clone(),
                    grant_id: route.grant_id.clone(),
                }),
            child_done_signals: (!session.history_only && session.recipient_only.is_none())
                .then_some(ChildDoneSignalsAdvertisement {
                    method: "child_report_signals",
                    protocol: crate::mailbox_v1::PROTOCOL,
                    max_wait_ms: MAX_WATCH_TIMEOUT_MS,
                }),
            endpoint: endpoint.display().to_string(),
            caller: session.caller.clone(),
            recipient: session.recipient.clone(),
            grant_id: session.grant_id.clone(),
            active_execution_generation: session.active_execution_generation,
            binding_generation: session.binding_generation.clone(),
            request_id_policy: "correlation_only",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BootstrapRequest {
    method: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    binding_generation: Option<String>,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapSuccess<T: Serialize> {
    ok: bool,
    request_id: Option<String>,
    result: T,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapFailure {
    ok: bool,
    request_id: Option<String>,
    error: BootstrapFailureBody,
}

#[derive(Serialize)]
struct BootstrapFailureBody {
    code: &'static str,
    message: &'static str,
}

fn failure(request_id: Option<String>, error: MailboxBootstrapError) -> String {
    let (code, message) = match error {
        MailboxBootstrapError::GrantMissing => (
            "grant_missing",
            "no current authenticated local mailbox sender route is available",
        ),
        MailboxBootstrapError::GrantRevoked => (
            "grant_revoked",
            "the server-selected mailbox binding was revoked",
        ),
        MailboxBootstrapError::PeerRejected => (
            "grant_missing",
            "the local peer is not the foreground Pi process for an Active sender",
        ),
        MailboxBootstrapError::InvalidRequest => (
            "invalid_request",
            "the bootstrap mailbox request is invalid",
        ),
        MailboxBootstrapError::Refused(code) => (
            code,
            "the server refused or failed the mailbox request; see the code",
        ),
        MailboxBootstrapError::RecoveryNeedsDropOrRetry => (
            "mailbox_recovery_needs_drop_or_retry",
            "that message was claimed by a Pi that is gone but never admitted; use Drop or Retry",
        ),
        MailboxBootstrapError::RecipientNotAllowed => (
            "mailbox_recipient_not_allowed",
            "a send grant can be provisioned only for your delegation parent or a direct child; reach other agents with herdr agent prompt or handoff",
        ),
        MailboxBootstrapError::ClaimExecutionAlive => (
            "mailbox_claim_execution_alive",
            "that message is held by another Pi that is still running; it cannot be dropped, retried or recovered from here",
        ),
        MailboxBootstrapError::HeadOutOfScope => (
            "mailbox_head_out_of_scope",
            "that message is not in this pane's own inbox; nothing was changed",
        ),
    };
    serde_json::to_string(&BootstrapFailure {
        ok: false,
        request_id,
        error: BootstrapFailureBody { code, message },
    })
    .unwrap_or_else(|_| {
        r#"{"ok":false,"error":{"code":"grant_missing","message":"bootstrap unavailable"}}"#.into()
    })
}

struct AcceptedMailboxConnection {
    stream: UnixStream,
    input: Vec<u8>,
    session: Option<MailboxBootstrapSession>,
    /// One parked `mailbox.watch` long-poll, answered from `poll`.
    watch: Option<ParkedWatch>,
    /// One parked `child_report_signals` long-poll (C2), answered from `poll`.
    signals: Option<ParkedWatch>,
}

struct ParkedWatch {
    request_id: Option<String>,
    after_marker: u64,
    deadline: std::time::Instant,
}

/// `mailbox.watch` waits at most this long before answering `changed:false`.
const MAX_WATCH_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WATCH_TIMEOUT_MS: u64 = 25_000;

fn watch_response(request_id: Option<String>, changed: bool, cursor: u64) -> String {
    serde_json::to_string(&BootstrapSuccess {
        ok: true,
        request_id,
        result: serde_json::json!({"type": "mailbox_watch", "changed": changed, "cursor": cursor}),
    })
    .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing))
}

fn signals_response(
    app: &App,
    session: &MailboxBootstrapSession,
    request_id: Option<String>,
    marker: u64,
) -> String {
    match app.child_report_signals_for_parent(session, marker) {
        Ok(result) => serde_json::to_string(&BootstrapSuccess {
            ok: true,
            request_id,
            result,
        })
        .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
        Err(error) => failure(request_id, error),
    }
}

/// A HeadlessServer-owned listener.  It is nonblocking; accepted streams are
/// incrementally framed in the headless event loop rather than given authority
/// by the filesystem name or by a background reader.
pub(crate) struct MailboxBootstrapListener {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketFileIdentity,
    accepted: HashMap<u64, AcceptedMailboxConnection>,
    next_connection_id: u64,
}

impl MailboxBootstrapListener {
    pub(crate) fn bind() -> io::Result<Self> {
        Self::bind_at(mailbox_bootstrap_socket_path())
    }

    pub(crate) fn bind_at(path: PathBuf) -> io::Result<Self> {
        crate::server::socket_paths::prepare_socket_path(&path)?;
        let listener = UnixListener::bind(&path)?;
        crate::server::socket_paths::restrict_socket_permissions(&path)?;
        listener.set_nonblocking(true)?;
        let identity = socket_file_identity(&path)?;
        Ok(Self {
            listener,
            path,
            identity,
            accepted: HashMap::new(),
            next_connection_id: 1,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Accept and service all complete newline-delimited requests currently
    /// available.  No socket discovery data is read as caller/scope authority.
    pub(crate) fn poll(&mut self, app: &mut App) -> io::Result<()> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true)?;
                    let id = self.next_connection_id;
                    self.next_connection_id = self.next_connection_id.saturating_add(1);
                    self.accepted.insert(
                        id,
                        AcceptedMailboxConnection {
                            stream,
                            input: Vec::new(),
                            session: None,
                            watch: None,
                            signals: None,
                        },
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }

        let mut closed = Vec::new();
        for (id, connection) in &mut self.accepted {
            let mut bytes = [0_u8; 4096];
            loop {
                match connection.stream.read(&mut bytes) {
                    Ok(0) => {
                        closed.push(*id);
                        break;
                    }
                    Ok(read) => {
                        if connection.input.len().saturating_add(read) > MAX_REQUEST_BYTES {
                            let _ = write_response(
                                &mut connection.stream,
                                &failure(None, MailboxBootstrapError::InvalidRequest),
                            );
                            closed.push(*id);
                            break;
                        }
                        connection.input.extend_from_slice(&bytes[..read]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        closed.push(*id);
                        break;
                    }
                }
            }
            while let Some(end) = connection.input.iter().position(|byte| *byte == b'\n') {
                let frame: Vec<u8> = connection.input.drain(..=end).collect();
                let request = std::str::from_utf8(&frame[..frame.len().saturating_sub(1)])
                    .ok()
                    .and_then(|frame| serde_json::from_str::<BootstrapRequest>(frame).ok());
                let response = match request {
                    Some(request) => Self::handle_request(app, connection, request),
                    None => Some(failure(None, MailboxBootstrapError::InvalidRequest)),
                };
                let Some(response) = response else {
                    continue;
                };
                if write_response(&mut connection.stream, &response).is_err() {
                    closed.push(*id);
                    break;
                }
            }
            // Answer a parked watch once the journal changed, the session is
            // no longer current, or its deadline passed.
            if let (Some(watch), Some(session)) = (&connection.watch, &connection.session) {
                let response = match app.mailbox_watch_marker(session) {
                    Ok(marker) if marker != watch.after_marker => {
                        Some(watch_response(watch.request_id.clone(), true, marker))
                    }
                    Ok(marker) if std::time::Instant::now() >= watch.deadline => {
                        Some(watch_response(watch.request_id.clone(), false, marker))
                    }
                    Ok(_) => None,
                    Err(error) => Some(failure(watch.request_id.clone(), error)),
                };
                if let Some(response) = response {
                    connection.watch = None;
                    if write_response(&mut connection.stream, &response).is_err() {
                        closed.push(*id);
                    }
                }
            }
            // A parked C2 signals poll answers with a fresh snapshot once the
            // journal changed or its deadline passed.
            if let (Some(parked), Some(session)) = (&connection.signals, &connection.session) {
                let response = match app.mailbox_watch_marker(session) {
                    Ok(marker)
                        if marker != parked.after_marker
                            || std::time::Instant::now() >= parked.deadline =>
                    {
                        Some(signals_response(
                            app,
                            session,
                            parked.request_id.clone(),
                            marker,
                        ))
                    }
                    Ok(_) => None,
                    Err(error) => Some(failure(parked.request_id.clone(), error)),
                };
                if let Some(response) = response {
                    connection.signals = None;
                    if write_response(&mut connection.stream, &response).is_err() {
                        closed.push(*id);
                    }
                }
            }
        }
        for id in closed {
            if let Some(session) = self
                .accepted
                .remove(&id)
                .and_then(|connection| connection.session)
            {
                app.release_mailbox_bootstrap_binding(&session.binding_generation);
            }
        }
        Ok(())
    }

    fn handle_request(
        app: &mut App,
        connection: &mut AcceptedMailboxConnection,
        request: BootstrapRequest,
    ) -> Option<String> {
        let request_id = request.request_id;
        if request.method == "bootstrap" {
            if connection.session.is_some() {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            }
            let session = match app.accept_mailbox_bootstrap_stream(connection.stream.as_raw_fd()) {
                Ok(session) => session,
                Err(error) => return Some(failure(request_id, error)),
            };
            let descriptor = MailboxBootstrapDescriptor::from_session(
                &session,
                &mailbox_bootstrap_socket_path(),
            );
            connection.session = Some(session);
            return Some(
                serde_json::to_string(&BootstrapSuccess {
                    ok: true,
                    request_id,
                    result: descriptor,
                })
                .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            );
        }

        let Some(session) = connection.session.as_ref() else {
            return Some(failure(request_id, MailboxBootstrapError::GrantMissing));
        };
        if request.binding_generation.as_deref() != Some(&session.binding_generation) {
            return Some(failure(request_id, MailboxBootstrapError::GrantRevoked));
        }
        if request.method == "mailbox.watch" {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct WatchParams {
                protocol: String,
                #[serde(default)]
                after_cursor: Option<u64>,
                #[serde(default)]
                wait_ms: Option<u64>,
            }
            let Ok(params) = serde_json::from_value::<WatchParams>(request.params) else {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            };
            if params.protocol != crate::mailbox_v1::PROTOCOL
                || session.history_only
                || connection.watch.is_some()
            {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            }
            // #181: a watch on its own inbox makes this stream a Messages consumer.
            let consumer = session.clone();
            app.note_messages_consumer(&consumer);
            let session = &consumer;
            let marker = match app.mailbox_watch_marker(session) {
                Ok(marker) => marker,
                Err(error) => return Some(failure(request_id, error)),
            };
            if params
                .wait_ms
                .is_some_and(|wait| wait > MAX_WATCH_TIMEOUT_MS)
            {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            }
            return match params.after_cursor {
                Some(after) if after == marker => {
                    let timeout = params
                        .wait_ms
                        .unwrap_or(DEFAULT_WATCH_TIMEOUT_MS)
                        .min(MAX_WATCH_TIMEOUT_MS);
                    connection.watch = Some(ParkedWatch {
                        request_id,
                        after_marker: after,
                        deadline: std::time::Instant::now()
                            + std::time::Duration::from_millis(timeout),
                    });
                    None
                }
                after => Some(watch_response(request_id, after.is_some(), marker)),
            };
        }
        if request.method == "child_report_signals" {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct SignalsParams {
                protocol: String,
                #[serde(default)]
                after_cursor: Option<u64>,
                #[serde(default)]
                wait_ms: Option<u64>,
            }
            let Ok(params) = serde_json::from_value::<SignalsParams>(request.params) else {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            };
            if params.protocol != crate::mailbox_v1::PROTOCOL
                || params
                    .wait_ms
                    .is_some_and(|wait| wait > MAX_WATCH_TIMEOUT_MS)
                || connection.signals.is_some()
            {
                return Some(failure(request_id, MailboxBootstrapError::InvalidRequest));
            }
            if session.history_only || session.recipient_only.is_some() {
                return Some(failure(request_id, MailboxBootstrapError::GrantRevoked));
            }
            let marker = match app.mailbox_watch_marker(session) {
                Ok(marker) => marker,
                Err(error) => return Some(failure(request_id, error)),
            };
            let wait = params.wait_ms.unwrap_or(0);
            if params.after_cursor == Some(marker) && wait > 0 {
                connection.signals = Some(ParkedWatch {
                    request_id,
                    after_marker: marker,
                    deadline: std::time::Instant::now() + std::time::Duration::from_millis(wait),
                });
                return None;
            }
            return Some(signals_response(app, session, request_id, marker));
        }
        let result = app.dispatch_mailbox_bootstrap(session, &request.method, request.params);
        Some(match result {
            Ok(result) => serde_json::to_string(&BootstrapSuccess {
                ok: true,
                request_id,
                result,
            })
            .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            Err(error) => failure(request_id, error),
        })
    }
}

/// The Headless owner calls this only after a listener was successfully bound.
/// It publishes discovery to a future Pi child, never any mailbox authority.
pub(crate) fn publish_owned_mailbox_bootstrap_discovery(
    app: &mut App,
    listener: &MailboxBootstrapListener,
) {
    app.publish_mailbox_bootstrap_discovery_address(listener.path());
}

impl Drop for MailboxBootstrapListener {
    fn drop(&mut self) {
        let _ = remove_socket_file_if_owned(&self.path, &self.identity);
    }
}

fn write_response(stream: &mut UnixStream, response: &str) -> io::Result<()> {
    stream.write_all(response.as_bytes())?;
    stream.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::time::Duration;

    use serde_json::{json, Value};

    use super::*;

    fn unique_dir() -> PathBuf {
        // Parallel tests may read the same clock value; the process-wide
        // counter keeps every test directory distinct.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "herdr-mailbox-bootstrap-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ))
    }

    fn active_app() -> (App, PathBuf, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        // These tests cover the trusted managed path; receive-only bindings
        // for untrusted Pis are covered in app::api::messages_tests.
        app.unmanaged_pi_messages = false;
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("sender")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = crate::app::Mode::Terminal;
        let pane_id = app.state.workspaces[0].tabs[0]
            .root_pane
            .expect("sender pane");
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .expect("sender terminal")
            .clone();
        let directory = unique_dir();
        let sender = terminal_id.to_string();
        app.sender_authority_dir = directory.clone();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .expect("authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist preparing record");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("sender terminal state");
        terminal.begin_managed_agent(
            "sender".into(),
            crate::detect::Agent::Pi,
            std::time::Instant::now(),
            Duration::from_secs(3),
            Duration::from_secs(30),
        );
        terminal.set_managed_agent_generation(1);
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: crate::detect::Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        let pid = std::process::id();
        app.install_mailbox_bootstrap_test_foreground_job(
            terminal_id,
            crate::platform::ForegroundJob {
                process_group_id: pid,
                processes: vec![crate::platform::ForegroundProcess {
                    pid,
                    name: "node".into(),
                    argv0: None,
                    argv: Some(vec![
                        "node".into(),
                        "/opt/pi/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    ]),
                    cmdline: Some(
                        "node /opt/pi/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
                            .into(),
                    ),
                }],
            },
        );
        install_trusted_test_pi(&mut app, &directory, &sender, std::process::id());
        (app, directory, sender)
    }

    fn install_trusted_test_pi(app: &mut App, directory: &Path, sender: &str, pid: u32) {
        use std::os::unix::fs::PermissionsExt;
        let terminal_id = app
            .state
            .terminals
            .keys()
            .find(|id| id.to_string() == sender)
            .unwrap()
            .clone();
        let path = directory.join(format!("{sender}.jsonl"));
        std::fs::write(
            &path,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"test\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let real = crate::platform::process_birth_identity(std::process::id()).unwrap();
        let birth = crate::platform::ProcessBirthIdentity {
            pid,
            start_ticks: real.start_ticks,
        };
        app.mailbox_bootstrap_test_process_births.insert(pid, birth);
        app.managed_pi_launches.insert(
            terminal_id.clone(),
            crate::app::ManagedPiLaunch {
                generation: 1,
                session_path: path.display().to_string(),
                earliest_birth_ticks: birth.start_ticks,
                process: Some(birth),
            },
        );
        app.install_mailbox_bootstrap_test_foreground_job(
            terminal_id,
            crate::platform::ForegroundJob {
                process_group_id: birth.pid,
                processes: vec![crate::platform::ForegroundProcess {
                    pid: birth.pid,
                    name: "pi".into(),
                    argv0: None,
                    argv: Some(vec!["pi".into()]),
                    cmdline: Some("pi".into()),
                }],
            },
        );
    }

    /// Makes the sender (workspace 0) the delegation parent of the most
    /// recently added recipient workspace's pane, as provision_recipient
    /// requires.
    fn relate_sender_and_recipient(app: &mut App) {
        let sender_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let recipient_pane = app.state.workspaces.last().unwrap().tabs[0]
            .root_pane
            .unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(sender_pane), None, None)
            .unwrap();
        app.state
            .delegations
            .create(Some(recipient_pane), Some(parent), None)
            .unwrap();
    }

    fn active_managed_recipient(app: &mut App, directory: &Path) -> (String, String) {
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("recipient"));
        app.state.ensure_test_terminals();
        let ws_idx = app.state.workspaces.len() - 1;
        let pane_id = app.state.workspaces[ws_idx].tabs[0]
            .root_pane
            .expect("recipient pane");
        let terminal_id = app.state.workspaces[ws_idx]
            .terminal_id(pane_id)
            .expect("recipient terminal")
            .clone();
        let sender = terminal_id.to_string();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(directory, &sender)
            .expect("recipient authority store");
        store
            .cas(
                None,
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 1,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 1,
                },
            )
            .expect("persist recipient preparing record");
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("recipient terminal state");
        terminal.begin_managed_agent(
            "recipient".into(),
            crate::detect::Agent::Pi,
            std::time::Instant::now(),
            Duration::from_secs(3),
            Duration::from_secs(30),
        );
        terminal.set_managed_agent_generation(1);
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: crate::detect::Agent::Pi,
            process_generation: 1,
            observed_at: std::time::Instant::now(),
        });
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: false,
            observed_at: std::time::Instant::now(),
        });
        install_trusted_test_pi(app, directory, &sender, std::process::id() + 1);
        (sender, "recipient".into())
    }

    fn try_ready_test_route(
        app: &mut App,
        directory: &Path,
        child: crate::delegation::DelegationId,
        parent: crate::delegation::DelegationId,
    ) -> serde_json::Value {
        app.no_session = false;
        app.session_save_path = directory.join("session.json");
        serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
            id: "route-ready".into(),
            method: crate::api::schema::Method::DelegationRouteReady(
                crate::api::schema::DelegationRouteReadyParams {
                    child_delegation_id: child.to_string(),
                    expected_parent_delegation_id: parent.to_string(),
                },
            ),
        }))
        .unwrap()
    }

    fn ready_test_route(
        app: &mut App,
        directory: &Path,
        child: crate::delegation::DelegationId,
        parent: crate::delegation::DelegationId,
    ) {
        let response = try_ready_test_route(app, directory, child, parent);
        assert_eq!(
            response["result"]["type"], "delegation_route_ready",
            "{response}"
        );
    }

    /// A ready child → parent route whose child pane has a recipe on the same
    /// session file its trusted Pi uses, and whose Pi has just exited.
    fn ready_route_with_child_recipe() -> (
        App,
        PathBuf,
        crate::delegation::DelegationId,
        crate::delegation::DelegationId,
        crate::layout::PaneId,
        crate::terminal::TerminalId,
    ) {
        let (mut app, directory, child_sender) = active_app();
        active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child, parent);
        let child_terminal = app.state.workspaces[0]
            .terminal_id(child_pane)
            .unwrap()
            .clone();
        let session = directory.join(format!("{child_sender}.jsonl"));
        let carry = app.state.terminals[&child_terminal]
            .route_carry
            .clone()
            .expect("route_ready remembers the route on the child pane");
        assert_eq!(carry.parent_delegation, parent.to_string());
        assert_eq!(carry.session_path, session.display().to_string());
        let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        std::mem::forget(input);
        app.terminal_runtimes
            .insert(child_terminal.clone(), runtime);
        app.state
            .terminals
            .get_mut(&child_terminal)
            .unwrap()
            .launch_recipe = crate::launch_recipe::LaunchRecipe::capture(
            "sender",
            "pi",
            &["--session".into(), session.display().to_string()],
            &[(
                "PI_CODING_AGENT_SESSION_DIR".into(),
                directory.display().to_string(),
            )],
        );
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: child_pane,
            agent: None,
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });
        (app, directory, child, parent, child_pane, child_terminal)
    }

    /// The relaunched Pi (the test process stands in for it) is born after the
    /// launch cutoff and observed Active.
    fn relaunched_pi_attaches(
        app: &mut App,
        pane: crate::layout::PaneId,
        terminal: &crate::terminal::TerminalId,
        generation: u64,
    ) {
        let floor = app.managed_pi_launches[terminal].earliest_birth_ticks;
        let pid = std::process::id();
        app.mailbox_bootstrap_test_process_births.insert(
            pid,
            crate::platform::ProcessBirthIdentity {
                pid,
                start_ticks: floor,
            },
        );
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: crate::detect::Agent::Pi,
            process_generation: generation,
            observed_at: std::time::Instant::now(),
        });
    }

    fn route_carry_outcome(
        directory: &Path,
        child: crate::delegation::DelegationId,
        generation: u64,
    ) -> String {
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                directory
                    .join("route-carries")
                    .join(format!("{child}-g{generation}.json")),
            )
            .expect("route carry record"),
        )
        .unwrap();
        record["outcome"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn sleep_then_wake_carries_the_delegation_route_to_the_new_generation() {
        let (mut app, directory, child, parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        app.state.terminals.get_mut(&child_terminal).unwrap().sleep =
            Some(App::new_pane_sleep("sender".into(), 1));
        let public = app.public_pane_id(0, child_pane).unwrap();
        let outcome = app.wake_pane(
            &public,
            crate::app::wake::WakeTrigger {
                cause: crate::app::wake::WakeCause::HeadAppended,
                recipient_id: child_terminal.to_string(),
                head_id: "h".into(),
            },
        );
        assert!(
            matches!(
                outcome,
                crate::app::wake::WakeOutcome::Started { generation: 2, .. }
            ),
            "{outcome:?}"
        );
        assert!(app.pending_route_carries.contains_key(&child_terminal));
        relaunched_pi_attaches(&mut app, child_pane, &child_terminal, 2);
        let route = app
            .ready_delegation_routes
            .get(&child)
            .expect("route re-established");
        assert_eq!(route.child_generation, 2);
        assert_eq!(route.parent, parent);
        assert_eq!(
            route.child_terminal, child_terminal,
            "same pane and terminal"
        );
        assert!(app.pending_route_carries.is_empty());
        assert_eq!(route_carry_outcome(&directory, child, 2), "established");
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// A carry pins the parent execution: if the parent pane now runs another
    /// session, the woken child is not rebound to it and shows not ready.
    #[tokio::test]
    async fn carry_is_refused_when_the_parent_changed_its_session() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, directory, child, _parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_terminal = app.state.workspaces[1]
            .terminal_id(parent_pane)
            .unwrap()
            .clone();
        let carry = app.state.terminals[&child_terminal]
            .route_carry
            .clone()
            .unwrap();
        assert_eq!(carry.parent_terminal, parent_terminal.to_string());
        // The parent restarts on a new session file.
        let new_session = directory.join("parent-new.jsonl");
        std::fs::write(
            &new_session,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"parent-new\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&new_session, std::fs::Permissions::from_mode(0o600)).unwrap();
        app.managed_pi_launches
            .get_mut(&parent_terminal)
            .unwrap()
            .session_path = new_session.display().to_string();
        assert_ne!(carry.parent_session, new_session.display().to_string());

        app.state.terminals.get_mut(&child_terminal).unwrap().sleep =
            Some(App::new_pane_sleep("sender".into(), 1));
        let public = app.public_pane_id(0, child_pane).unwrap();
        let outcome = app.wake_pane(
            &public,
            crate::app::wake::WakeTrigger {
                cause: crate::app::wake::WakeCause::HeadAppended,
                recipient_id: child_terminal.to_string(),
                head_id: "h".into(),
            },
        );
        assert!(matches!(
            outcome,
            crate::app::wake::WakeOutcome::Started { generation: 2, .. }
        ));
        relaunched_pi_attaches(&mut app, child_pane, &child_terminal, 2);
        assert!(
            !app.ready_delegation_routes.contains_key(&child),
            "the child shows NOT ready"
        );
        assert!(app.state.terminals[&child_terminal].route_carry.is_none());
        assert!(app.pending_route_carries.is_empty());
        assert_eq!(route_carry_outcome(&directory, child, 2), "refused");
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn recipe_restart_resume_carries_the_delegation_route() {
        let (mut app, directory, child, parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        let recipe = app.state.terminals[&child_terminal]
            .launch_recipe
            .clone()
            .unwrap();
        app.pending_managed_resumes.insert(
            child_terminal.clone(),
            crate::app::agent_resume::PendingManagedResume {
                recipe,
                public_pane_id: app.public_pane_id(0, child_pane).unwrap(),
                fallback_command: "pi".into(),
                deadline: std::time::Instant::now() + Duration::from_secs(5),
                // The shell has been idle long enough to settle.
                idle_since: Some(std::time::Instant::now() - Duration::from_secs(1)),
                last_error: None,
                started_at: std::time::Instant::now(),
            },
        );
        assert!(app.retry_pending_managed_resumes(std::time::Instant::now()));
        assert!(app.pending_route_carries.contains_key(&child_terminal));
        relaunched_pi_attaches(&mut app, child_pane, &child_terminal, 2);
        let route = app
            .ready_delegation_routes
            .get(&child)
            .expect("route re-established");
        assert_eq!((route.child_generation, route.parent), (2, parent));
        assert_eq!(route_carry_outcome(&directory, child, 2), "established");
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn hand_start_on_a_different_session_never_inherits_the_route() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, directory, child, _parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        let other = directory.join("other.jsonl");
        std::fs::write(
            &other,
            b"{\"type\":\"session\",\"version\":3,\"id\":\"other\",\"cwd\":\"/tmp\"}\n",
        )
        .unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600)).unwrap();
        let started = app.handle_api_request(crate::api::schema::Request {
            id: "hand-start".into(),
            method: crate::api::schema::Method::AgentStart(crate::api::schema::AgentStartParams {
                name: "sender".into(),
                kind: "pi".into(),
                pane_id: app.public_pane_id(0, child_pane).unwrap(),
                args: vec!["--session".into(), other.display().to_string()],
                env: vec![format!(
                    "PI_CODING_AGENT_SESSION_DIR={}",
                    directory.display()
                )],
                timeout_ms: None,
            }),
        });
        assert!(!started.contains("\"error\""), "{started}");
        assert!(app.state.terminals[&child_terminal].route_carry.is_none());
        assert!(app.pending_route_carries.is_empty());
        relaunched_pi_attaches(&mut app, child_pane, &child_terminal, 2);
        assert_ne!(
            app.ready_delegation_routes
                .get(&child)
                .map(|route| route.child_generation),
            Some(2),
            "a hand start never re-establishes the previous route"
        );
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Review finding 8 on 7e858344: a reparent revokes the route and its
    /// carry, so a later recipe relaunch re-creates nothing.
    #[tokio::test]
    async fn reparent_forgets_the_carried_route() {
        let (mut app, directory, child, _parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        app.save_session_now();
        let saved = std::fs::read_to_string(directory.join("session.json")).unwrap();
        assert!(
            saved.contains("route_carry"),
            "the carry is on disk before the revoke"
        );
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "reparent".into(),
            method: crate::api::schema::Method::DelegationReparent(
                crate::api::schema::DelegationReparentParams {
                    delegation_id: child.to_string(),
                    parent_id: None,
                },
            ),
        });
        assert!(!response.contains("\"error\""), "{response}");
        assert!(app.state.terminals[&child_terminal].route_carry.is_none());
        assert!(!app.ready_delegation_routes.contains_key(&child));
        // QA: the clear is on disk at once, not only after the debounced save.
        let saved = std::fs::read_to_string(directory.join("session.json")).unwrap();
        assert!(
            !saved.contains("route_carry"),
            "carry cleared on disk at once"
        );
        // A later wake of the slept child starts it but carries nothing.
        app.state.terminals.get_mut(&child_terminal).unwrap().sleep =
            Some(App::new_pane_sleep("sender".into(), 1));
        let public = app.public_pane_id(0, child_pane).unwrap();
        let outcome = app.wake_pane(
            &public,
            crate::app::wake::WakeTrigger {
                cause: crate::app::wake::WakeCause::HeadAppended,
                recipient_id: child_terminal.to_string(),
                head_id: "h".into(),
            },
        );
        assert!(
            matches!(outcome, crate::app::wake::WakeOutcome::Started { .. }),
            "{outcome:?}"
        );
        assert!(app.pending_route_carries.is_empty());
        relaunched_pi_attaches(&mut app, child_pane, &child_terminal, 2);
        assert!(!app.ready_delegation_routes.contains_key(&child));
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Review finding 8 on 7e858344: any route teardown (here route-ready
    /// failing while the child is gone) forgets the carry too.
    #[tokio::test]
    async fn a_failed_route_ready_forgets_the_carried_route() {
        let (mut app, directory, child, parent, _child_pane, child_terminal) =
            ready_route_with_child_recipe();
        // The fixture's child Pi has exited, so the route is not ready now.
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "route-ready".into(),
            method: crate::api::schema::Method::DelegationRouteReady(
                crate::api::schema::DelegationRouteReadyParams {
                    child_delegation_id: child.to_string(),
                    expected_parent_delegation_id: parent.to_string(),
                },
            ),
        });
        assert!(response.contains("route_not_ready"), "{response}");
        assert!(app.state.terminals[&child_terminal].route_carry.is_none());
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    /// Review finding 9 on 7e858344: a hand agent.start while a wake is
    /// outstanding is a hand start: it ends the sleep and inherits no route.
    #[tokio::test]
    async fn a_hand_start_during_an_outstanding_wake_is_a_hand_start() {
        let (mut app, directory, _child, _parent, child_pane, child_terminal) =
            ready_route_with_child_recipe();
        app.state.terminals.get_mut(&child_terminal).unwrap().sleep =
            Some(App::new_pane_sleep("sender".into(), 1));
        app.pane_wakes.insert(
            child_terminal.clone(),
            crate::app::wake::OutstandingWake {
                wake_id: "outstanding".into(),
                pane_key: "pane:outstanding".into(),
                generation: 9,
                trigger: crate::app::wake::WakeTrigger {
                    cause: crate::app::wake::WakeCause::HeadAppended,
                    recipient_id: "pane:outstanding".into(),
                    head_id: "h".into(),
                },
                deadline: std::time::Instant::now() + Duration::from_secs(60),
                retried: false,
            },
        );
        let started = app.handle_api_request(crate::api::schema::Request {
            id: "hand-start".into(),
            method: crate::api::schema::Method::AgentStart(crate::api::schema::AgentStartParams {
                name: "sender".into(),
                kind: "pi".into(),
                pane_id: app.public_pane_id(0, child_pane).unwrap(),
                args: Vec::new(),
                env: Vec::new(),
                timeout_ms: None,
            }),
        });
        assert!(!started.contains("\"error\""), "{started}");
        let terminal = &app.state.terminals[&child_terminal];
        assert!(terminal.sleep.is_none(), "a hand start ends the sleep");
        assert!(terminal.route_carry.is_none(), "and inherits no route");
        assert!(app.pending_route_carries.is_empty());
        drop(app);
        let _ = std::fs::remove_dir_all(directory);
    }

    fn listener(directory: &Path) -> MailboxBootstrapListener {
        std::fs::create_dir_all(directory).expect("create test directory");
        MailboxBootstrapListener::bind_at(directory.join("mailbox.sock")).expect("bind listener")
    }

    fn exchange(
        listener: &mut MailboxBootstrapListener,
        app: &mut App,
        client: &mut UnixStream,
        request: Value,
    ) -> Value {
        let frame = serde_json::to_vec(&request).expect("encode request");
        client.write_all(&frame).expect("write request");
        client.write_all(b"\n").expect("frame request");
        listener.poll(app).expect("poll listener");
        let mut response = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            client.read_exact(&mut byte).expect("read response");
            if byte[0] == b'\n' {
                break;
            }
            response.push(byte[0]);
        }
        serde_json::from_slice(&response).expect("decode response")
    }

    fn bootstrap(
        listener: &mut MailboxBootstrapListener,
        app: &mut App,
        client: &mut UnixStream,
    ) -> Value {
        exchange(
            listener,
            app,
            client,
            json!({"method": "bootstrap", "requestId": "bootstrap-1"}),
        )
    }

    #[test]
    fn owned_listener_removes_its_socket_and_allows_clean_rebind() {
        let directory = unique_dir();
        let path = {
            let listener = listener(&directory);
            let path = listener.path().to_path_buf();
            assert!(path.exists(), "listener owns its socket path");
            path
        };
        assert!(
            !path.exists(),
            "dropped listener removes its owned socket path"
        );

        let rebound = MailboxBootstrapListener::bind_at(path.clone()).expect("rebind socket path");
        assert!(path.exists(), "rebound listener owns its socket path");
        drop(rebound);
        assert!(!path.exists(), "rebound listener cleans up its socket path");
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn shutting_down_listener_preserves_successor_ready_socket() {
        let directory = unique_dir();
        let first = listener(&directory);
        let path = first.path().to_path_buf();
        std::fs::remove_file(&path).expect("unlink first listener socket path");
        let successor = MailboxBootstrapListener::bind_at(path.clone()).expect("bind successor");
        assert!(
            UnixStream::connect(&path).is_ok(),
            "successor socket is ready"
        );

        drop(first);
        assert!(
            path.exists(),
            "first listener shutdown must not remove successor socket"
        );
        drop(successor);
        assert!(!path.exists(), "successor cleans up its own socket");
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn watch_parks_until_the_journal_changes_and_close_releases_the_binding() {
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(
            descriptor["result"]["messages"]["watchMethod"],
            "mailbox.watch"
        );
        assert_eq!(descriptor["result"]["messages"]["watchMaxWaitMs"], 30000);
        assert_eq!(descriptor["result"]["binding"], "managed");
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        // #181: bootstrap alone is not a Messages consumer; the first watch is.
        assert!(app.attached_messages_recipient(&sender).is_none());
        let first = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"mailbox.watch","requestId":"w0","bindingGeneration":binding,
                   "params":{"protocol":crate::mailbox_v1::PROTOCOL}}),
        );
        assert_eq!(first["result"]["changed"], false);
        assert!(app.attached_messages_recipient(&sender).is_some());
        let marker = first["result"]["cursor"].as_u64().unwrap();
        // Parked: no response while nothing changes.
        let frame = json!({"method":"mailbox.watch","requestId":"w1","bindingGeneration":binding,
            "params":{"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":marker,"waitMs":5000}});
        client.write_all(format!("{frame}\n").as_bytes()).unwrap();
        listener.poll(&mut app).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert!(
            client.read_exact(&mut byte).is_err(),
            "watch must be parked"
        );
        // Another writer appends; the next poll answers the watch.
        crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .append_offline_head(crate::mailbox::MailboxHead {
                stable_id: "watch-stable".into(),
                revision: 1,
                digest: "c".repeat(64),
                delivery_digest: "d".repeat(64),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: sender.clone(),
                    generation: "1".into(),
                },
                subject: "s".into(),
                body: "b".into(),
                recipient_generation: "1".into(),
                sender: "someone".into(),
                target: sender.clone(),
                grant_id: "g".into(),
                message_id: "m".into(),
                kind: "advisory".into(),
                priority: "normal".into(),
                original_sequence: 1,
                enqueue_epoch: 0,
                accepted_at: 1,
                delivery: None,
            })
            .unwrap();
        listener.poll(&mut app).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut response = Vec::new();
        loop {
            client.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            response.push(byte[0]);
        }
        let response: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["requestId"], "w1");
        assert_eq!(response["result"]["changed"], true);
        assert_ne!(response["result"]["cursor"].as_u64().unwrap(), marker);
        // C1: a second accepted stream (Pi's dedicated watch stream) never
        // revokes the first stream's binding.
        let mut second = UnixStream::connect(listener.path()).expect("second stream");
        second
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let second_descriptor = bootstrap(&mut listener, &mut app, &mut second);
        assert_eq!(second_descriptor["ok"], true);
        assert_ne!(second_descriptor["result"]["bindingGeneration"], binding);
        let still = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"mailbox.snapshot","requestId":"s-after","bindingGeneration":binding,
                   "params":{"protocol":crate::mailbox_v1::PROTOCOL}}),
        );
        assert_eq!(still["ok"], true, "{still}");
        // A managed binding may self-enqueue into its own pane inbox too.
        assert_eq!(
            descriptor["result"]["messages"]["enqueueSelfMethod"],
            "mailbox.enqueue_self"
        );
        let typed = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"mailbox.enqueue_self","requestId":"self-1","bindingGeneration":binding,
                   "params":{"protocol":crate::mailbox_v1::PROTOCOL,"subject":"Typed","body":"typed while busy"}}),
        );
        assert_eq!(typed["result"]["type"], "mailbox_enqueued", "{typed}");
        // A concurrently forked test child can briefly inherit a socket FD
        // until it execs, delaying EOF; poll until the close is observed.
        let poll_until = |listener: &mut MailboxBootstrapListener,
                          app: &mut App,
                          done: &dyn Fn(&App) -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                listener.poll(app).unwrap();
                if done(app) || std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        drop(second);
        poll_until(&mut listener, &mut app, &|app| {
            app.mailbox_bootstrap_bindings.len() == 1
        });
        assert!(app.attached_messages_recipient(&sender).is_some());
        // Closing the stream releases the binding: no longer "has Messages".
        drop(client);
        poll_until(&mut listener, &mut app, &|app| {
            app.attached_messages_recipient(&sender).is_none()
        });
        assert!(app.attached_messages_recipient(&sender).is_none());
        std::fs::remove_dir_all(directory).ok();
    }

    /// A handler refusal keeps its specific code on the wire, so a client can
    /// tell a pre-append refusal from a malformed request.
    #[test]
    fn bootstrap_wire_passes_specific_handler_error_codes_through() {
        let (mut app, directory, _sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .expect("binding generation")
            .to_owned();
        let submit = |request_id: &str| {
            json!({
                "method": "mailbox.offline_submit", "requestId": request_id,
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "stable-1", "revision": 1,
                    "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
                    "subject": "subject", "body": "body", "messageId": "message-1",
                    "kind": "advisory", "priority": "normal", "originalSequence": 1
                }
            })
        };
        let first = exchange(&mut listener, &mut app, &mut client, submit("first"));
        assert_eq!(first["ok"], true, "{first}");
        let replay = exchange(&mut listener, &mut app, &mut client, submit("replay"));
        assert_eq!(replay["ok"], false);
        assert_eq!(
            replay["error"]["code"], "mailbox_replay_rejected",
            "{replay}"
        );
        let malformed = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method": "mailbox.offline_submit", "requestId": "bad",
                   "bindingGeneration": binding, "params": {"protocol": "nope"}}),
        );
        assert_eq!(malformed["error"]["code"], "invalid_request", "{malformed}");
        assert_eq!(
            crate::app::MailboxBootstrapError::from_handler_error("something_new"),
            crate::app::MailboxBootstrapError::Refused("mailbox_request_refused")
        );
        drop(listener);
        std::fs::remove_dir_all(directory).ok();
    }

    #[test]
    fn bootstrap_success_authenticates_all_mailbox_dispatches_with_server_scope() {
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["ok"], true);
        assert_eq!(descriptor["result"]["caller"], sender);
        assert_eq!(
            descriptor["result"]["grantId"],
            format!("offline:{sender}:1")
        );
        assert_eq!(descriptor["result"]["requestIdPolicy"], "correlation_only");
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .expect("binding generation")
            .to_owned();

        let submit = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.offline_submit", "requestId": "submit-1",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "stable-1", "revision": 1,
                    "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
                    "subject": "subject", "body": "body", "messageId": "message-1",
                    "kind": "report", "priority": "normal", "originalSequence": 1
                }
            }),
        );
        assert_eq!(submit["ok"], true);
        let edit = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.edit", "requestId": "edit-1",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "stable-1", "revision": 1, "digest": "a".repeat(64),
                    "subject": "edited subject", "body": "edited body"
                }
            }),
        );
        assert_eq!(edit["ok"], true);
        assert_eq!(edit["result"]["snapshot"]["heads"][0]["revision"], 2);
        let snapshot = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.snapshot", "requestId": "snapshot-1",
                "bindingGeneration": binding,
                "params": {"protocol": crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(snapshot["ok"], true);
        assert_eq!(
            snapshot["result"]["snapshot"]["heads"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let claim = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.claim", "requestId": "claim-1",
                "bindingGeneration": binding,
                "params": {"protocol": crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(claim["ok"], true);
        let claim_id = claim["result"]["claim"]["claimId"]
            .as_str()
            .expect("claim id")
            .to_owned();
        let resolve = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.resolve", "requestId": "resolve-1",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "claimId": claim_id, "outcome": "settled"
                }
            }),
        );
        assert_eq!(resolve["ok"], true);
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn route_ready_replaced_lock_quarantines_marker_until_exclusive_resync() {
        let (mut app, directory, _sender) = active_app();
        active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child, parent);
        let original_epoch = app.ready_delegation_routes[&child].epoch.clone();
        let lock_path = directory.join(".session-writer.lock");
        std::fs::remove_file(&lock_path).unwrap();
        let replacement_owner =
            crate::persist::SessionWriter::acquire(&app.session_save_path).unwrap();
        let denied = try_ready_test_route(&mut app, &directory, child, parent);
        let quarantined = !app.ready_delegation_routes.contains_key(&child);
        drop(replacement_owner);
        let retry = try_ready_test_route(&mut app, &directory, child, parent);
        let renewed_epoch = app
            .ready_delegation_routes
            .get(&child)
            .map(|route| route.epoch.clone());
        let owns_replacement = app
            .session_writer
            .as_ref()
            .is_some_and(|writer| writer.validate(&app.session_save_path).is_ok());
        drop(app);
        std::fs::remove_dir_all(directory).unwrap();
        assert_eq!(
            denied["error"]["code"], "route_persistence_failed",
            "replaced lock cannot receive an idempotent ready acknowledgment: {denied}"
        );
        assert!(
            quarantined,
            "old marker must be quarantined while exclusive ownership is lost"
        );
        assert_eq!(retry["result"]["type"], "delegation_route_ready", "{retry}");
        assert!(
            owns_replacement,
            "fresh acknowledgment requires owning the replacement lock"
        );
        assert_ne!(
            renewed_epoch.as_deref(),
            Some(original_epoch.as_str()),
            "new lease requires a fresh route epoch"
        );
    }

    #[test]
    fn background_snapshot_started_before_child_cannot_undo_ready_edge() {
        let (mut app, directory, _sender) = active_app();
        active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        app.no_session = false;
        app.session_save_path = directory.join("session.json");
        app.start_background_session_save(); // captured a graph with no child
        let child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child, parent); // joins old writer before sync
        let snapshot: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&app.session_save_path).unwrap()).unwrap();
        assert!(
            snapshot["delegations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["id"] == child.to_string()),
            "older in-flight snapshot must not follow acknowledgement"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bound_parent_route_requires_durable_ready_and_fresh_stream_after_aba() {
        let (mut app, directory, _sender) = active_app();
        active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        let mut listener = listener(&directory);
        let mut old = UnixStream::connect(listener.path()).unwrap();
        old.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let before = bootstrap(&mut listener, &mut app, &mut old);
        assert!(
            before["result"]["parentReport"].is_null(),
            "visible graph is not route authority"
        );
        app.no_session = false;
        app.session_save_path = directory.join("session.json");
        let competing = crate::persist::SessionWriter::acquire(&app.session_save_path).unwrap();
        let denied = try_ready_test_route(&mut app, &directory, child, parent);
        assert_eq!(denied["error"]["code"], "route_persistence_failed");
        drop(competing);
        for stage in [
            crate::persist::DurableStep::Write,
            crate::persist::DurableStep::FileSync,
            crate::persist::DurableStep::Rename,
            crate::persist::DurableStep::DirectorySync,
        ] {
            crate::persist::inject_durable_failure(Some(stage));
            let uncertain = try_ready_test_route(&mut app, &directory, child, parent);
            crate::persist::inject_durable_failure(None);
            assert_eq!(
                uncertain["error"]["code"], "route_persistence_failed",
                "{stage:?}"
            );
            assert!(
                !app.ready_delegation_routes.contains_key(&child),
                "{stage:?} cannot activate route"
            );
        }
        let mut still_unready = UnixStream::connect(listener.path()).unwrap();
        still_unready
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(
            bootstrap(&mut listener, &mut app, &mut still_unready)["result"]["parentReport"]
                .is_null()
        );
        ready_test_route(&mut app, &directory, child, parent);
        let acknowledged_epoch = app.ready_delegation_routes[&child].epoch.clone();
        ready_test_route(&mut app, &directory, child, parent);
        assert_eq!(
            app.ready_delegation_routes[&child].epoch, acknowledged_epoch,
            "lost response retry is idempotent"
        );
        let mut ready = UnixStream::connect(listener.path()).unwrap();
        ready
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let issued = bootstrap(&mut listener, &mut app, &mut ready);
        let first_grant = issued["result"]["parentReport"]["grantId"]
            .as_str()
            .unwrap()
            .to_string();
        let old_binding = before["result"]["bindingGeneration"].as_str().unwrap();
        let old_attempt = exchange(
            &mut listener,
            &mut app,
            &mut old,
            json!({
                "method":"report_submit_parent","bindingGeneration":old_binding,"params":{
                    "protocol":crate::mailbox_v1::PROTOCOL,"stableId":"retroactive","revision":1,
                    "digest":"a".repeat(64),"deliveryDigest":"b".repeat(64),"subject":"report",
                    "body":"body","messageId":"retroactive-message","kind":"report","priority":"normal","originalSequence":1
                }
            }),
        );
        assert!(
            old_attempt["error"]["code"] == "grant_missing"
                || old_attempt["error"]["code"] == "grant_revoked"
        );
        app.state.delegations.reparent(child, None).unwrap();
        app.state.delegations.reparent(child, Some(parent)).unwrap();
        let binding = issued["result"]["bindingGeneration"].as_str().unwrap();
        let stale = exchange(
            &mut listener,
            &mut app,
            &mut ready,
            json!({
                "method":"report_submit_parent","bindingGeneration":binding,"params":{
                    "protocol":crate::mailbox_v1::PROTOCOL,"stableId":"stale-aba","revision":1,
                    "digest":"c".repeat(64),"deliveryDigest":"d".repeat(64),"subject":"report",
                    "body":"body","messageId":"stale-aba-message","kind":"report","priority":"normal","originalSequence":2
                }
            }),
        );
        assert_eq!(stale["error"]["code"], "grant_revoked");
        ready_test_route(&mut app, &directory, child, parent);
        let mut newer = UnixStream::connect(listener.path()).unwrap();
        newer
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let new_descriptor = bootstrap(&mut listener, &mut app, &mut newer);
        assert_ne!(
            new_descriptor["result"]["parentReport"]["grantId"],
            first_grant
        );
        let unrelated_parent = app.state.delegations.create(None, None, None).unwrap();
        let wrong = try_ready_test_route(&mut app, &directory, child, unrelated_parent);
        assert_eq!(wrong["error"]["code"], "route_not_ready");
        let mut no_wrong_parent = UnixStream::connect(listener.path()).unwrap();
        no_wrong_parent
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(
            bootstrap(&mut listener, &mut app, &mut no_wrong_parent)["result"]["parentReport"]
                .is_null()
        );
        ready_test_route(&mut app, &directory, child, parent);
        app.state.delegations.tombstone_pane(child_pane);
        let mut tombstoned = UnixStream::connect(listener.path()).unwrap();
        tombstoned
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(
            bootstrap(&mut listener, &mut app, &mut tombstoned)["result"]["parentReport"].is_null()
        );
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parent_read_only_child_disposition_is_exact_and_cursor_scoped() {
        let (mut app, directory, child) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        // The test process is the parent Pi peer. The child still has a
        // separately trusted exact live birth, but cannot claim this socket.
        install_trusted_test_pi(&mut app, &directory, &child, std::process::id() + 2);
        install_trusted_test_pi(&mut app, &directory, &parent, std::process::id());
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["result"]["caller"], parent);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let wrong_child = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"todo_state","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"root",
                          "localRevision":1,"stateDigest":"a".repeat(64),"state":"done"}
            }),
        );
        assert_eq!(wrong_child["error"]["code"], "grant_missing");
        let params = json!({"protocol":crate::mailbox_v1::PROTOCOL,
                            "childDelegationId":child_id.to_string()});
        let first = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,"params":params
            }),
        );
        assert_eq!(
            first["result"]["disposition"]["kind"], "unknown_unattested",
            "{first}"
        );
        let cursor = first["result"]["disposition"]["cursor"].as_u64().unwrap();
        let again = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                          "childDelegationId":child_id.to_string(),"afterCursor":cursor}
            }),
        );
        assert_eq!(again["result"]["changed"], false);
        let future = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                          "childDelegationId":child_id.to_string(),"afterCursor":cursor + 1}
            }),
        );
        assert_eq!(future["error"]["code"], "invalid_request");
        let unrelated = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"childDelegationId":"d999999"}
            }),
        );
        assert_eq!(unrelated["error"]["code"], "grant_revoked");
        // Replacement revokes even a read-only stream; an old parent never
        // receives current-child completion information after its generation.
        crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &parent)
            .unwrap()
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: parent.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        let stale = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"childDelegationId":child_id.to_string()}
            }),
        );
        assert_eq!(stale["error"]["code"], "grant_revoked");
        drop(client);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// #181 test head: a server-sent message waiting in `recipient`'s queue,
    /// pinned to `session` (or unpinned).
    fn c181_head(
        id: &str,
        recipient: &crate::mailbox::RecipientKey,
        session: Option<&str>,
    ) -> crate::mailbox::MailboxHead {
        let digest = crate::app::messages::sha256_fields(&[id.as_bytes(), b"digest"]);
        crate::mailbox::MailboxHead {
            stable_id: id.into(),
            revision: 1,
            digest,
            delivery_digest: crate::app::messages::sha256_fields(&[id.as_bytes(), b"delivery"]),
            recipient: recipient.clone(),
            subject: format!("subject {id}"),
            body: format!("body {id}"),
            recipient_generation: recipient.generation.clone(),
            sender: "external:1".into(),
            target: recipient.recipient_id.clone(),
            grant_id: "g".into(),
            message_id: format!("m-{id}"),
            kind: "advisory".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 0,
            accepted_at: 1,
            delivery: Some(crate::mailbox::ServerDelivery {
                origin: "agent_prompt".into(),
                sender_label: "pm".into(),
                sender_session: None,
                recipient_session: session.map(str::to_string),
                correlation: None,
                retry_of: None,
                typed_reason: None,
                moved_from: None,
            }),
        }
    }

    #[test]
    fn c181_waiting_messages_follow_a_managed_session_to_its_new_pane_once() {
        let (mut app, directory, new_pane) = active_app();
        let (old_pane, _) = active_managed_recipient(&mut app, &directory);
        // The managed Pi in the new pane runs on session S (its launch record).
        let pinned = app
            .current_agent_session_value(&new_pane)
            .expect("managed session");
        assert_eq!(
            app.managed_pi_launches
                .iter()
                .find(|(id, _)| id.to_string() == new_pane)
                .unwrap()
                .1
                .session_path,
            pinned
        );
        let old_inbox =
            crate::app::messages::pane_recipient(&app.pane_queue_key(&old_pane).unwrap());
        let new_inbox =
            crate::app::messages::pane_recipient(&app.pane_queue_key(&new_pane).unwrap());
        let store = crate::mailbox::MailboxStore::open(&directory).unwrap();
        for (id, session) in [
            ("h1", Some(pinned.as_str())),
            ("h2", Some(pinned.as_str())),
            ("h3", None),
            ("h4", Some("/other/session.jsonl")),
            ("h5", Some(pinned.as_str())),
        ] {
            store
                .append_offline_head(c181_head(id, &old_inbox, session))
                .unwrap();
        }
        // h5 was picked up by an earlier run of S that has since ended.
        let h5 = store.load().unwrap().heads["h5"].clone();
        store
            .claim(crate::mailbox::Claim {
                claim_id: "claim-h5".into(),
                recipient: old_inbox.clone(),
                stable_id: "h5".into(),
                revision: 1,
                digest: h5.digest.clone(),
                execution: Some("managed:gone:1".into()),
            })
            .unwrap();

        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["result"]["caller"], new_pane, "{descriptor}");
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        // Bootstrap alone moves nothing: the Pi may have no Messages.
        assert!(!store.load().unwrap().claims.contains_key("h1"));
        let watch = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"mailbox.watch","bindingGeneration":binding,
                   "params":{"protocol":crate::mailbox_v1::PROTOCOL}}),
        );
        assert_eq!(watch["ok"], true, "{watch}");
        let recovered = store.load().unwrap();
        let closed_by = |id: &str| {
            let claim = recovered.claims.get(id)?;
            recovered
                .resolutions
                .get(&claim.claim_id)
                .and_then(|resolution| resolution.closed_by.clone())
        };
        let mut copies = Vec::new();
        for id in ["h1", "h2"] {
            assert!(crate::mailbox::is_withdrawn_claim(&recovered.claims[id]));
            assert_eq!(closed_by(id).as_deref(), Some("moved"));
            let copy = &recovered.heads[&crate::app::message_follow::moved_copy_id(id, &pinned)];
            assert_eq!(copy.recipient, new_inbox);
            assert_eq!(copy.moved_from(), Some(id));
            assert_eq!(copy.body, format!("body {id}"));
            assert!(
                !recovered.claims.contains_key(&copy.stable_id),
                "held in the new pane"
            );
            copies.push(copy.enqueue_epoch);
        }
        assert!(copies[0] < copies[1], "original order kept");
        assert!(
            !recovered.claims.contains_key("h3"),
            "unpinned: pane-owned, stays"
        );
        assert!(
            !recovered.claims.contains_key("h4"),
            "another session's: stays"
        );
        assert_eq!(
            recovered.claims["h5"].claim_id, "claim-h5",
            "claimed: stays"
        );
        let notes: Vec<_> = recovered
            .heads
            .values()
            .filter(|head| head.subject == "Messages moved to this pane")
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].recipient, new_inbox);
        assert!(
            notes[0].body.starts_with("2 waiting message(s)"),
            "{}",
            notes[0].body
        );
        assert!(notes[0].body.contains("1 message(s)"), "{}", notes[0].body);
        // The old pane's history says where they went.
        let mut old_view = crate::app::messages::inbox_snapshot(
            &recovered,
            &app.inbox_recipients(&old_pane),
            "x",
            None,
            &|_| false,
        )
        .unwrap();
        app.annotate_moved_heads(&mut old_view, &recovered);
        let new_public = app.public_pane_for_terminal(&new_pane).unwrap();
        for state in old_view
            .head_states
            .iter()
            .filter(|state| ["h1", "h2"].contains(&state.stable_id.as_str()))
        {
            assert_eq!(state.closed_by.as_deref(), Some("moved"));
            assert_eq!(state.moved_to.as_deref(), Some(new_public.as_str()));
        }
        // Once only: a second stream of the same execution moves nothing more.
        let records = std::fs::read_to_string(directory.join(crate::mailbox::RECORD_STREAM_FILE))
            .unwrap()
            .lines()
            .count();
        let mut again = UnixStream::connect(listener.path()).unwrap();
        again
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let second = bootstrap(&mut listener, &mut app, &mut again);
        let second_binding = second["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        let snapshot = exchange(
            &mut listener,
            &mut app,
            &mut again,
            json!({"method":"mailbox.snapshot","bindingGeneration":second_binding,
                   "params":{"protocol":crate::mailbox_v1::PROTOCOL}}),
        );
        assert_eq!(snapshot["ok"], true, "{snapshot}");
        assert_eq!(
            std::fs::read_to_string(directory.join(crate::mailbox::RECORD_STREAM_FILE))
                .unwrap()
                .lines()
                .count(),
            records,
            "no second move and no second note"
        );
        drop(client);
        drop(again);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn c2_descriptor_advertises_child_done_signals_and_never_parent_signals() {
        // Installed Pi d74c3a4 rejects a `parentSignals` without the #115
        // recovery/bind methods; C2 must use only its own new key.
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["result"]["caller"], sender, "{descriptor}");
        assert_eq!(descriptor["result"]["binding"], "managed");
        assert_eq!(
            descriptor["result"]["childDoneSignals"],
            json!({"method":"child_report_signals","protocol":crate::mailbox_v1::PROTOCOL,
                   "maxWaitMs":30000})
        );
        assert!(
            descriptor["result"].get("parentSignals").is_none(),
            "{descriptor}"
        );
        let mut keys: Vec<_> = descriptor["result"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "activeExecutionGeneration",
                "binding",
                "bindingGeneration",
                "caller",
                "childDoneSignals",
                "endpoint",
                "grantId",
                "historyOnly",
                "historySnapshot",
                "messages",
                "protocolVersion",
                "recipient",
                "reportSubmit",
                "requestIdPolicy",
            ],
            "only the one new key is added to the rc3 descriptor"
        );
        drop(client);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn c2_parent_reads_child_done_without_report_until_admission_or_reopen() {
        let (mut app, directory, child) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let mut listener = listener(&directory);
        let me = std::process::id();
        // The test process plays one Pi at a time; each role gets a fresh stream.
        let become_pi = |app: &mut App, listener: &mut MailboxBootstrapListener, who: &str| {
            let other = if who == child { &parent } else { &child };
            install_trusted_test_pi(app, &directory, other, me + 2);
            install_trusted_test_pi(app, &directory, who, me);
            let mut stream = UnixStream::connect(listener.path()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let descriptor = bootstrap(listener, app, &mut stream);
            assert_eq!(descriptor["result"]["caller"], who, "{descriptor}");
            assert!(descriptor["result"].get("parentSignals").is_none());
            assert_eq!(
                descriptor["result"]["childDoneSignals"],
                json!({"method":"child_report_signals","protocol":crate::mailbox_v1::PROTOCOL,
                       "maxWaitMs":30000})
            );
            let binding = descriptor["result"]["bindingGeneration"]
                .as_str()
                .unwrap()
                .to_owned();
            (stream, binding)
        };
        let todo = |revision: u64, state: &str| {
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"root",
                   "localRevision":revision,"stateDigest":format!("{revision:064x}"),"state":state})
        };
        let signals = |binding: &str, params: Value| json!({"method":"child_report_signals","bindingGeneration":binding,"params":params});
        let protocol = json!({"protocol":crate::mailbox_v1::PROTOCOL});

        // Child: not done, then done.
        let (mut cs, cb) = become_pi(&mut app, &mut listener, &child);
        for (revision, state) in [(1, "not_done"), (2, "done")] {
            let ack = exchange(
                &mut listener,
                &mut app,
                &mut cs,
                json!({"method":"todo_state","bindingGeneration":cb,"params":todo(revision, state)}),
            );
            assert_eq!(ack["result"]["type"], "todo_state", "{ack}");
        }
        // The child is nobody's parent: an empty snapshot.
        let own = exchange(
            &mut listener,
            &mut app,
            &mut cs,
            signals(&cb, protocol.clone()),
        );
        assert_eq!(own["result"]["signals"], json!([]), "{own}");
        drop(cs);

        // Parent: exactly one fact, with the full wire shape.
        let (mut ps, pb) = become_pi(&mut app, &mut listener, &parent);
        let first = exchange(
            &mut listener,
            &mut app,
            &mut ps,
            signals(&pb, protocol.clone()),
        );
        assert_eq!(first["result"]["type"], "child_report_signals", "{first}");
        assert_eq!(first["result"]["truncated"], false);
        let fact = &first["result"]["signals"][0];
        assert_eq!(
            first["result"]["signals"].as_array().unwrap().len(),
            1,
            "{first}"
        );
        assert_eq!(fact["type"], "report_unknown");
        assert_eq!(fact["reason"], "coverage_unqualified");
        assert_eq!(fact["delegationId"], child_id.to_string());
        assert_eq!(
            fact["routeEpoch"],
            app.ready_delegation_routes[&child_id].epoch
        );
        assert!(fact["doneAt"].as_u64().is_some_and(|at| at > 0), "{fact}");
        assert_eq!(fact["child"]["terminalId"], child);
        assert!(fact["child"]["paneId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        assert_eq!(fact["child"]["session"]["source"], "herdr:pi");
        assert_eq!(fact["child"]["session"]["agent"], "pi");
        assert_eq!(fact["child"]["session"]["kind"], "path");
        assert!(fact["child"]["session"]["value"]
            .as_str()
            .unwrap()
            .ends_with(".jsonl"));
        assert_eq!(
            fact["todo"],
            json!({"localRoot":"root","localRevision":2,
            "stateDigest":format!("{:064x}", 2),"state":"done",
            "todoStateCursor":fact["todo"]["todoStateCursor"].clone()})
        );
        assert!(fact["todo"]["todoStateCursor"].as_u64().unwrap() > 0);
        let through = first["result"]["throughCursor"].as_u64().unwrap();
        let again = exchange(
            &mut listener,
            &mut app,
            &mut ps,
            signals(
                &pb,
                json!({"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":through}),
            ),
        );
        assert_eq!(
            again["result"]["signals"], first["result"]["signals"],
            "doneAt is stable"
        );
        for bad in [
            json!({"protocol":"wrong"}),
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"waitMs":30001}),
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"caller":child}),
        ] {
            let denied = exchange(&mut listener, &mut app, &mut ps, signals(&pb, bad.clone()));
            assert_eq!(
                denied["error"]["code"], "invalid_request",
                "{bad}: {denied}"
            );
        }
        let forged = exchange(
            &mut listener,
            &mut app,
            &mut ps,
            signals("forged", protocol.clone()),
        );
        assert_eq!(forged["error"]["code"], "grant_revoked");

        // Parked until the journal changes: the child's reopen clears the fact.
        let frame = signals(
            &pb,
            json!({"protocol":crate::mailbox_v1::PROTOCOL,
            "afterCursor":through,"waitMs":5000}),
        );
        ps.write_all(format!("{frame}\n").as_bytes()).unwrap();
        listener.poll(&mut app).unwrap();
        ps.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0_u8; 1];
        assert!(
            ps.read_exact(&mut byte).is_err(),
            "signals poll must be parked"
        );
        let recorded = crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap();
        let crate::child_report::ChildReportEvent::TodoState { route, .. } =
            recorded.child_report_events.last().unwrap().clone()
        else {
            panic!("last event is the done TodoState");
        };
        crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .append_child_report_event(crate::child_report::ChildReportEvent::TodoState {
                route,
                local_root: "root".into(),
                local_revision: 3,
                state_digest: format!("{:064x}", 3),
                state: crate::child_report::LocalTodoState::NotDone,
            })
            .unwrap();
        listener.poll(&mut app).unwrap();
        ps.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut response = Vec::new();
        loop {
            ps.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            response.push(byte[0]);
        }
        let woke: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(
            woke["result"]["signals"],
            json!([]),
            "reopen clears: {woke}"
        );
        assert!(woke["result"]["throughCursor"].as_u64().unwrap() > through);
        drop(ps);

        // Child: done again (fact is back), then an admitted bound report.
        let (mut cs, cb) = become_pi(&mut app, &mut listener, &child);
        let ack = exchange(
            &mut listener,
            &mut app,
            &mut cs,
            json!({"method":"todo_state","bindingGeneration":cb,"params":todo(4, "done")}),
        );
        assert_eq!(ack["result"]["type"], "todo_state", "{ack}");
        drop(cs);
        let (mut ps, pb) = become_pi(&mut app, &mut listener, &parent);
        let back = exchange(
            &mut listener,
            &mut app,
            &mut ps,
            signals(&pb, protocol.clone()),
        );
        assert_eq!(
            back["result"]["signals"][0]["todo"]["localRevision"], 4,
            "{back}"
        );
        assert!(back["result"]["signals"][0]["doneAt"].as_u64().is_some());
        drop(ps);
        let (mut cs, cb) = become_pi(&mut app, &mut listener, &child);
        let preparation = json!({
            "protocol":crate::mailbox_v1::PROTOCOL,
            "localRoot":"root","localRevision":4,"reportId":"report-one",
            "reportDigest":"a".repeat(64),"stableId":"c2-report",
            "submitRevision":1,"submitDigest":"a".repeat(64),
            "deliveryDigest":"b".repeat(64),"messageId":"c2-message"
        });
        let prepared = exchange(
            &mut listener,
            &mut app,
            &mut cs,
            json!({"method":"report_prepared","bindingGeneration":cb,"params":preparation}),
        );
        assert_eq!(prepared["result"]["type"], "report_prepared", "{prepared}");
        let submit = json!({
            "protocol": crate::mailbox_v1::PROTOCOL, "stableId": "c2-report",
            "revision": 1, "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
            "subject": "report", "body": "body", "messageId": "c2-message",
            "kind": "report", "priority": "normal", "originalSequence": 1
        });
        let admitted = exchange(
            &mut listener,
            &mut app,
            &mut cs,
            json!({"method":"report_submit_parent","bindingGeneration":cb,"params":submit}),
        );
        assert_eq!(
            admitted["result"]["receipt"]["status"], "admitted",
            "{admitted}"
        );
        drop(cs);
        let (mut ps, pb) = become_pi(&mut app, &mut listener, &parent);
        let cleared = exchange(
            &mut listener,
            &mut app,
            &mut ps,
            signals(&pb, protocol.clone()),
        );
        assert_eq!(
            cleared["result"]["signals"],
            json!([]),
            "admitted report clears: {cleared}"
        );
        drop(ps);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn child_todo_state_requires_current_bound_stream_and_never_qualifies_missing() {
        let (mut app, directory, child) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        let mut listener = listener(&directory);
        let mut early = UnixStream::connect(listener.path()).unwrap();
        early
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let before = bootstrap(&mut listener, &mut app, &mut early);
        assert!(before["result"]["parentReport"].is_null());
        let early_binding = before["result"]["bindingGeneration"].as_str().unwrap();
        let not_done = json!({"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"child-root",
                              "localRevision":1,"stateDigest":"a".repeat(64),"state":"not_done"});
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut early,
            json!({
                "method":"todo_state","bindingGeneration":early_binding,"params":not_done
            }),
        );
        assert_eq!(denied["error"]["code"], "grant_missing");
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let retroactive = exchange(
            &mut listener,
            &mut app,
            &mut early,
            json!({
                "method":"todo_state","bindingGeneration":early_binding,"params":not_done
            }),
        );
        assert!(
            retroactive["error"]["code"] == "grant_missing"
                || retroactive["error"]["code"] == "grant_revoked",
            "{retroactive}"
        );
        let mut current = UnixStream::connect(listener.path()).unwrap();
        current
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let issued = bootstrap(&mut listener, &mut app, &mut current);
        assert_eq!(
            issued["result"]["parentReport"]["todoStateMethod"],
            "todo_state"
        );
        let binding = issued["result"]["bindingGeneration"].as_str().unwrap();
        let request = |params: serde_json::Value| {
            json!({
                "method":"todo_state","bindingGeneration":binding,"params":params
            })
        };
        let first = exchange(
            &mut listener,
            &mut app,
            &mut current,
            request(not_done.clone()),
        );
        assert_eq!(first["result"]["type"], "todo_state", "{first}");
        let cursor = first["result"]["cursor"].as_u64().unwrap();
        let epoch = first["result"]["routeEpoch"].as_str().unwrap().to_owned();
        let duplicate = exchange(
            &mut listener,
            &mut app,
            &mut current,
            request(not_done.clone()),
        );
        assert_eq!(duplicate["result"]["cursor"], cursor);
        let recorded = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        let exact_route = recorded
            .child_report_events
            .iter()
            .find_map(|event| match event {
                crate::child_report::ChildReportEvent::TodoState {
                    route,
                    local_root,
                    local_revision,
                    state_digest,
                    state,
                } if local_root == "child-root" => {
                    assert_eq!(
                        (*local_revision, state_digest.as_str()),
                        (1, "a".repeat(64).as_str())
                    );
                    assert_eq!(*state, crate::child_report::LocalTodoState::NotDone);
                    Some(route.clone())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(exact_route.child_delegation_id, child_id.to_string());
        assert_eq!(exact_route.parent_delegation_id, parent_id.to_string());
        assert_eq!(exact_route.child_terminal_id, child);
        assert_eq!(exact_route.parent_terminal_id, parent);
        assert_eq!(exact_route.child_session.source, "herdr:pi");
        assert_eq!(exact_route.parent_session.source, "herdr:pi");
        assert!(
            exact_route.child_process_generation > 0 && exact_route.parent_process_generation > 0
        );
        assert_eq!(exact_route.route_epoch, epoch);
        for (label, mut bad) in [
            ("conflict-digest", not_done.clone()),
            ("conflict-state", not_done.clone()),
            ("changed-root", not_done.clone()),
            ("zero-revision", not_done.clone()),
            ("wrong-protocol", not_done.clone()),
            ("invalid-digest", not_done.clone()),
            ("invalid-state", not_done.clone()),
            ("forged-parent", not_done.clone()),
        ] {
            match label {
                "conflict-digest" => bad["stateDigest"] = json!("b".repeat(64)),
                "conflict-state" => bad["state"] = json!("done"),
                "changed-root" => bad["localRoot"] = json!("other-root"),
                "zero-revision" => bad["localRevision"] = json!(0),
                "wrong-protocol" => bad["protocol"] = json!("other"),
                "invalid-digest" => bad["stateDigest"] = json!("A".repeat(64)),
                "invalid-state" => bad["state"] = json!("idle"),
                "forged-parent" => bad["parentTerminalId"] = json!(parent),
                _ => unreachable!(),
            }
            let rejected = exchange(&mut listener, &mut app, &mut current, request(bad));
            assert_eq!(
                rejected["error"]["code"], "invalid_request",
                "{label}: {rejected}"
            );
        }
        let forged_binding = exchange(
            &mut listener,
            &mut app,
            &mut current,
            json!({
                "method":"todo_state","bindingGeneration":"forged","params":not_done
            }),
        );
        assert_eq!(forged_binding["error"]["code"], "grant_revoked");
        let mut done = not_done.clone();
        done["localRevision"] = json!(2);
        done["stateDigest"] = json!("b".repeat(64));
        done["state"] = json!("done");
        let accepted = exchange(&mut listener, &mut app, &mut current, request(done.clone()));
        assert!(
            accepted["result"]["cursor"].as_u64().unwrap() > cursor,
            "{accepted}"
        );
        let rollback = exchange(
            &mut listener,
            &mut app,
            &mut current,
            request(not_done.clone()),
        );
        assert_eq!(rollback["error"]["code"], "invalid_request");
        let observed = exchange(
            &mut listener,
            &mut app,
            &mut current,
            json!({
                "method":"report_coverage","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"child-root","localRevision":2}
            }),
        );
        assert_eq!(observed["result"]["coverageQualified"], false, "{observed}");
        let recovered = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        assert_eq!(
            crate::child_report::project(&exact_route, &recovered).kind,
            crate::child_report::ReportDispositionKind::InFlightOrUncertain
        );
        let stable_cursor = recovered.record_cursor;
        app.state.delegations.reparent(child_id, None).unwrap();
        let stale = exchange(&mut listener, &mut app, &mut current, request(done));
        assert_eq!(stale["error"]["code"], "grant_revoked");
        assert_eq!(
            crate::mailbox::MailboxStore::existing(&directory)
                .load()
                .unwrap()
                .record_cursor,
            stable_cursor
        );
        app.state
            .delegations
            .reparent(child_id, Some(parent_id))
            .unwrap();
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let still_stale = exchange(
            &mut listener,
            &mut app,
            &mut current,
            request(not_done.clone()),
        );
        assert_eq!(still_stale["error"]["code"], "grant_revoked");
        let mut renewed = UnixStream::connect(listener.path()).unwrap();
        renewed
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut renewed);
        let new_binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let fresh = exchange(
            &mut listener,
            &mut app,
            &mut renewed,
            json!({
                "method":"todo_state","bindingGeneration":new_binding,"params":not_done
            }),
        );
        assert_eq!(fresh["result"]["type"], "todo_state", "{fresh}");
        assert_ne!(fresh["result"]["routeEpoch"], epoch);
        let latest = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        let current_route = latest
            .child_report_events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::child_report::ChildReportEvent::TodoState { route, .. }
                    if route.route_epoch != epoch =>
                {
                    Some(route)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            crate::child_report::project(current_route, &latest).kind,
            crate::child_report::ReportDispositionKind::NotDone,
            "old done claim cannot become current after reconnect"
        );
        crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &parent)
            .unwrap()
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: parent.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        let replaced = exchange(
            &mut listener,
            &mut app,
            &mut renewed,
            json!({
                "method":"todo_state","bindingGeneration":new_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"child-root",
                          "localRevision":2,"stateDigest":"b".repeat(64),"state":"done"}
            }),
        );
        assert_eq!(replaced["error"]["code"], "grant_revoked");
        assert_eq!(
            crate::mailbox::MailboxStore::existing(&directory)
                .load()
                .unwrap()
                .record_cursor,
            latest.record_cursor
        );
        drop(renewed);
        drop(current);
        drop(early);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn child_todo_state_does_not_acknowledge_failed_journal_append() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, directory, _) = active_app();
        active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child, parent);
        let mut listener = listener(&directory);
        let mut current = UnixStream::connect(listener.path()).unwrap();
        current
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut current);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let before = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap()
            .record_cursor;
        let stream = directory.join(crate::mailbox::RECORD_STREAM_FILE);
        let original = std::fs::metadata(&stream).unwrap().permissions();
        std::fs::set_permissions(&stream, std::fs::Permissions::from_mode(0o400)).unwrap();
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut current,
            json!({
                "method":"todo_state","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"root",
                          "localRevision":1,"stateDigest":"a".repeat(64),"state":"done"}
            }),
        );
        std::fs::set_permissions(&stream, original).unwrap();
        assert_eq!(denied["error"]["code"], "grant_missing", "{denied}");
        let recovered = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        assert_eq!(recovered.record_cursor, before);
        assert!(!recovered.child_report_events.iter().any(|event| matches!(
            event,
            crate::child_report::ChildReportEvent::TodoState { .. }
        )));
        drop(current);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bound_parent_report_route_is_durable_and_rejects_selectors_non_reports_and_stale_parent() {
        let (mut app, directory, sender) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(
            descriptor["result"]["parentReport"]["method"],
            "report_submit_parent"
        );
        assert_eq!(
            descriptor["result"]["parentReport"]["todoStateMethod"],
            "todo_state"
        );
        assert_eq!(
            descriptor["result"]["parentReport"]["preparedMethod"],
            "report_prepared"
        );
        assert_eq!(
            descriptor["result"]["parentReport"]["coverageQualified"],
            false
        );
        assert_eq!(
            descriptor["result"]["parentReport"]["recipient"]["recipientId"],
            parent
        );
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let wrong_parent = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"delegation.child_report_disposition",
                "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                          "childDelegationId":child_id.to_string()}
            }),
        );
        assert_eq!(wrong_parent["error"]["code"], "grant_revoked");
        let submit = json!({
            "protocol": crate::mailbox_v1::PROTOCOL, "stableId": "parent-report",
            "revision": 1, "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
            "subject": "report", "body": "body", "messageId": "parent-message",
            "kind": "report", "priority": "normal", "originalSequence": 1
        });
        let mut wrong_selector = submit.clone();
        wrong_selector["recipient"] = json!({"recipientId":sender,"generation":"1"});
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding,
                "params": wrong_selector
            }),
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":"forged", "params":submit
            }),
        );
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        let mut non_report = submit.clone();
        non_report["kind"] = json!("assignment");
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params":non_report
            }),
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        let unprepared = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params":submit
            }),
        );
        assert_eq!(unprepared["error"]["code"], "grant_missing");
        let preparation = json!({
            "protocol":crate::mailbox_v1::PROTOCOL,
            "localRoot":"local-root","localRevision":1,"reportId":"report-one",
            "reportDigest":"a".repeat(64),"stableId":"parent-report",
            "submitRevision":1,"submitDigest":"a".repeat(64),
            "deliveryDigest":"b".repeat(64),"messageId":"parent-message"
        });
        let prepared = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,"params":preparation
            }),
        );
        assert_eq!(prepared["result"]["type"], "report_prepared", "{prepared}");
        let duplicate = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,"params":preparation
            }),
        );
        assert_eq!(duplicate["result"]["cursor"], prepared["result"]["cursor"]);
        let stale_binding = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":"forged","params":preparation
            }),
        );
        assert_eq!(stale_binding["error"]["code"], "grant_revoked");
        let mut forged_route = preparation.clone();
        forged_route["parentTerminalId"] = json!(parent);
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,"params":forged_route
            }),
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        let mut conflict = preparation.clone();
        conflict["reportDigest"] = json!("f".repeat(64));
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,"params":conflict
            }),
        );
        assert_eq!(rejected["error"]["code"], "invalid_request");
        let admitted = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params":submit
            }),
        );
        assert_eq!(admitted["result"]["receipt"]["status"], "admitted");
        let recovered = crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(
            recovered.heads["parent-report"].recipient.recipient_id,
            parent
        );
        assert_eq!(recovered.heads["parent-report"].sender, sender);
        // A report from the authenticated bound stream durably preannounces
        // its exact route attempt before the admitted head and receipt.
        assert_eq!(recovered.child_report_events.len(), 2);
        assert!(matches!(
            &recovered.child_report_events[1],
            crate::child_report::ChildReportEvent::PreparedAttempt { preparation }
                if preparation.report_id == "report-one"
                    && preparation.message_id == "parent-message"
                    && preparation.delivery_digest == "b".repeat(64)
                    && preparation.route.child_delegation_id == child_id.to_string()
                    && preparation.route.parent_delegation_id == parent_id.to_string()
                    && preparation.route.child_terminal_id == sender
                    && preparation.route.parent_terminal_id == parent
                    && crate::child_report::project(&preparation.route, &recovered).kind
                        == crate::child_report::ReportDispositionKind::InFlightOrUncertain
        ));
        assert_eq!(
            crate::mailbox_v1::snapshot(
                &recovered,
                &crate::mailbox::RecipientKey {
                    recipient_id: parent.clone(),
                    generation: "1".into()
                }
            )
            .unwrap()
            .heads
            .len(),
            1
        );
        let coverage = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_coverage","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                          "localRoot":"local-root","localRevision":1}
            }),
        );
        assert_eq!(coverage["result"]["coverageQualified"], false, "{coverage}");
        let barrier = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        assert!(barrier.child_report_events.iter().any(|e| matches!(e,
            crate::child_report::ChildReportEvent::CoverageBarrier {
                through_cursor,qualification:crate::child_report::CoverageQualification::ObservedOnly,..
            } if *through_cursor < barrier.record_cursor)));
        let parent_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &parent).unwrap();
        parent_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: parent,
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params": {
                    "protocol":crate::mailbox_v1::PROTOCOL,"stableId":"stale","revision":1,
                    "digest":"c".repeat(64),"deliveryDigest":"d".repeat(64),
                    "subject":"stale","body":"body","messageId":"stale-message",
                    "kind":"report","priority":"normal","originalSequence":2
                }
            }),
        );
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        let stale_generic = app.handle_mailbox_offline_submit(
            "stale-generic".into(),
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id: descriptor["result"]["parentReport"]["grantId"]
                    .as_str()
                    .unwrap()
                    .into(),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: parent_store.load().unwrap().unwrap().sender_key,
                    generation: "1".into(),
                },
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: "stale-generic".into(),
                    revision: 1,
                    digest: "5".repeat(64),
                    delivery_digest: "6".repeat(64),
                    subject: "report".into(),
                    body: "body".into(),
                    message_id: "stale-generic-message".into(),
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 2,
                },
                api_peer: None,
            },
        );
        let stale_generic: crate::api::schema::ErrorResponse = serde_json::from_str(&stale_generic)
            .expect("stale parent grant must not admit generic API request");
        assert_eq!(stale_generic.error.code, "mailbox_capability_mismatch");
        assert!(!crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap()
            .heads
            .contains_key("stale-generic"));
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bound_parent_report_reparent_revokes_old_stream_without_retargeting() {
        let (mut app, directory, sender) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        app.state.delegations.reparent(child_id, None).unwrap();
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params": {
                    "protocol":crate::mailbox_v1::PROTOCOL,"stableId":"reparented","revision":1,
                    "digest":"e".repeat(64),"deliveryDigest":"f".repeat(64),
                    "subject":"report","body":"body","messageId":"reparented-message",
                    "kind":"report","priority":"normal","originalSequence":1
                }
            }),
        );
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        assert!(crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap()
            .heads
            .is_empty());
        let recipient = crate::mailbox::RecipientKey {
            recipient_id: parent,
            generation: "1".into(),
        };
        let generic = |grant_id: String, stable_id: &str, digest: &str, delivery_digest: &str| {
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id,
                recipient: recipient.clone(),
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: stable_id.into(),
                    revision: 1,
                    digest: digest.repeat(64),
                    delivery_digest: delivery_digest.repeat(64),
                    subject: "report".into(),
                    body: "body".into(),
                    message_id: stable_id.into(),
                    kind: "advisory".into(),
                    priority: "normal".into(),
                    original_sequence: 2,
                },
                api_peer: None,
            }
        };
        let bound_grant = descriptor["result"]["parentReport"]["grantId"]
            .as_str()
            .unwrap()
            .to_owned();
        let blocked = app.handle_mailbox_offline_submit(
            "stale".into(),
            generic(bound_grant.clone(), "stale-generic", "a", "b"),
        );
        let blocked: crate::api::schema::ErrorResponse = serde_json::from_str(&blocked).unwrap();
        assert_eq!(blocked.error.code, "mailbox_capability_mismatch");
        let explicit_grant = app
            .provision_cross_recipient_mailbox_grant(&sender, recipient.clone())
            .unwrap();
        assert_ne!(explicit_grant, bound_grant);
        let admitted = app.handle_mailbox_offline_submit(
            "explicit".into(),
            generic(explicit_grant, "explicit", "c", "d"),
        );
        assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&admitted).is_ok());
        let recovered = crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap();
        assert!(!recovered.heads.contains_key("stale-generic"));
        assert_eq!(recovered.heads["explicit"].recipient, recipient);
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bound_parent_report_and_old_local_journal_survive_fresh_app_and_socket() {
        let (mut app, directory, sender) = active_app();
        let (parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let old = app.handle_mailbox_offline_submit(
            "old".into(),
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id: format!("offline:{sender}:1"),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: sender.clone(),
                    generation: "1".into(),
                },
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: "old-local".into(),
                    revision: 1,
                    digest: "1".repeat(64),
                    delivery_digest: "2".repeat(64),
                    subject: "old".into(),
                    body: "body".into(),
                    message_id: "old-message".into(),
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 1,
                },
                api_peer: None,
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&old).is_ok());
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let old_bound_grant = descriptor["result"]["parentReport"]["grantId"]
            .as_str()
            .unwrap()
            .to_owned();
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let prepared = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                    "localRoot":"root","localRevision":1,"reportId":"report-new",
                    "reportDigest":"3".repeat(64),"stableId":"new-parent",
                    "submitRevision":1,"submitDigest":"3".repeat(64),
                    "deliveryDigest":"4".repeat(64),"messageId":"new-message"}
            }),
        );
        assert_eq!(prepared["result"]["type"], "report_prepared", "{prepared}");
        let admitted = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"report_submit_parent", "bindingGeneration":binding, "params": {
                    "protocol":crate::mailbox_v1::PROTOCOL,"stableId":"new-parent","revision":1,
                    "digest":"3".repeat(64),"deliveryDigest":"4".repeat(64),
                    "subject":"report","body":"body","messageId":"new-message",
                    "kind":"report","priority":"normal","originalSequence":2
                }
            }),
        );
        assert_eq!(admitted["result"]["receipt"]["status"], "admitted");
        drop(client);
        drop(listener);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut restarted = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        restarted.state = app.state;
        restarted.sender_authority_dir = directory.clone();
        for key in [&sender, &parent] {
            let record = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, key)
                .unwrap()
                .load()
                .unwrap()
                .unwrap();
            restarted.install_offline_mailbox_authority(record).unwrap();
        }
        let sender_terminal = restarted.state.workspaces[0]
            .terminal_id(child_pane)
            .unwrap()
            .clone();
        let pid = std::process::id();
        restarted.install_mailbox_bootstrap_test_foreground_job(
            sender_terminal,
            crate::platform::ForegroundJob {
                process_group_id: pid,
                processes: vec![crate::platform::ForegroundProcess {
                    pid,
                    name: "node".into(),
                    argv0: None,
                    argv: Some(vec![
                        "node".into(),
                        "/opt/pi/node_modules/@earendil-works/pi-coding-agent/dist/cli.js".into(),
                    ]),
                    cmdline: Some(
                        "node /opt/pi/node_modules/@earendil-works/pi-coding-agent/dist/cli.js"
                            .into(),
                    ),
                }],
            },
        );
        let mut listener = self::listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut restarted, &mut client);
        assert!(
            descriptor["result"]["parentReport"].is_null(),
            "restored graph alone cannot revive route readiness"
        );
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let stale_preparation = exchange(
            &mut listener,
            &mut restarted,
            &mut client,
            json!({
                "method":"report_prepared","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"root",
                    "localRevision":1,"reportId":"report-new","reportDigest":"3".repeat(64),
                    "stableId":"new-parent","submitRevision":1,"submitDigest":"3".repeat(64),
                    "deliveryDigest":"4".repeat(64),"messageId":"new-message"}
            }),
        );
        assert_eq!(stale_preparation["error"]["code"], "grant_missing");
        let old_snapshot = exchange(
            &mut listener,
            &mut restarted,
            &mut client,
            json!({
                "method":"mailbox.snapshot","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(
            old_snapshot["result"]["snapshot"]["heads"][0]["stableId"],
            "old-local"
        );
        let stale_generic = restarted.handle_mailbox_offline_submit(
            "old-grant".into(),
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id: old_bound_grant,
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: parent.clone(),
                    generation: "1".into(),
                },
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: "post-restart-generic".into(),
                    revision: 1,
                    digest: "5".repeat(64),
                    delivery_digest: "6".repeat(64),
                    subject: "report".into(),
                    body: "body".into(),
                    message_id: "post-restart-message".into(),
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 3,
                },
                api_peer: None,
            },
        );
        let stale_generic: crate::api::schema::ErrorResponse =
            serde_json::from_str(&stale_generic).unwrap();
        assert_eq!(stale_generic.error.code, "mailbox_capability_mismatch");
        let parent_snapshot = restarted.handle_mailbox_snapshot(
            "parent".into(),
            crate::api::schema::MailboxSnapshotParams {
                caller: parent.clone(),
                grant_id: format!("offline:{parent}:1"),
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: parent,
                    generation: "1".into(),
                },
                protocol: crate::mailbox_v1::PROTOCOL.into(),
                api_peer: None,
            },
        );
        let parent_snapshot: serde_json::Value = serde_json::from_str(&parent_snapshot).unwrap();
        assert_eq!(
            parent_snapshot["result"]["snapshot"]["heads"][0]["stableId"],
            "new-parent"
        );
        assert_eq!(
            parent_snapshot["result"]["snapshot"]["heads"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn fresh_app_same_ordinal_rejects_old_descriptor_on_new_accepted_stream() {
        let (mut first, directory, sender) = active_app();
        let mut old_listener = listener(&directory);
        let mut old_client = UnixStream::connect(old_listener.path()).unwrap();
        old_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let old = bootstrap(&mut old_listener, &mut first, &mut old_client);
        assert_eq!(old["ok"], true);
        let old_binding = old["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        let old_boot = first.mailbox_bootstrap_boot_nonce.clone();
        assert_eq!(first.next_mailbox_bootstrap_binding, 2);
        drop(old_client);
        drop(old_listener);
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut restarted = App::new(
            &crate::config::Config::default(),
            true,
            None,
            rx,
            crate::api::EventHub::default(),
        );
        assert_eq!(restarted.next_mailbox_bootstrap_binding, 1);
        assert_ne!(restarted.mailbox_bootstrap_boot_nonce, old_boot);
        restarted.state = first.state;
        restarted.sender_authority_dir = directory.clone();
        let record = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .unwrap()
            .load()
            .unwrap()
            .unwrap();
        restarted.install_offline_mailbox_authority(record).unwrap();
        install_trusted_test_pi(&mut restarted, &directory, &sender, std::process::id());
        let mut new_listener = listener(&directory);
        let mut current = UnixStream::connect(new_listener.path()).unwrap();
        current
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let fresh = bootstrap(&mut new_listener, &mut restarted, &mut current);
        assert_eq!(fresh["ok"], true, "{fresh}");
        let current_binding = fresh["result"]["bindingGeneration"].as_str().unwrap();
        assert_ne!(current_binding, old_binding);
        assert_eq!(current_binding.split('-').nth(3), Some("1"));
        assert_eq!(old_binding.split('-').nth(3), Some("1"));
        let stale = exchange(
            &mut new_listener,
            &mut restarted,
            &mut current,
            json!({
                "method":"mailbox.snapshot","bindingGeneration":old_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(stale["error"]["code"], "grant_revoked", "{stale}");
        let new_snapshot = exchange(
            &mut new_listener,
            &mut restarted,
            &mut current,
            json!({
                "method":"mailbox.snapshot","bindingGeneration":current_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(
            new_snapshot["result"]["type"], "mailbox_snapshot",
            "{new_snapshot}"
        );
        let reported = exchange(
            &mut new_listener,
            &mut restarted,
            &mut current,
            json!({
                "method":"report_submit","bindingGeneration":current_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,"stableId":"self-report",
                          "revision":1,"digest":"a".repeat(64),"deliveryDigest":"b".repeat(64),
                          "subject":"synthetic","body":"synthetic","messageId":"self-message",
                          "kind":"report","priority":"normal","originalSequence":1}
            }),
        );
        assert_eq!(
            reported["result"]["receipt"]["status"], "admitted",
            "{reported}"
        );
        // Even if a test forces the local ordinal backwards and forgets the
        // used nonce, an existing ID must never overwrite another stream.
        let nonce = current_binding.rsplit('-').next().unwrap().to_owned();
        restarted.used_mailbox_bootstrap_nonces.remove(&nonce);
        restarted.next_mailbox_bootstrap_binding = 1;
        assert!(restarted
            .mailbox_bootstrap_binding_candidate(Some(&nonce))
            .is_err());
        restarted.used_mailbox_bootstrap_nonces.insert(nonce);
        restarted.next_mailbox_bootstrap_binding = 2;
        // Entropy failure must not publish another accepted descriptor.
        let current_boot = restarted.mailbox_bootstrap_boot_nonce.clone();
        restarted.mailbox_bootstrap_boot_nonce = None;
        let mut no_entropy = UnixStream::connect(new_listener.path()).unwrap();
        no_entropy
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let denied = bootstrap(&mut new_listener, &mut restarted, &mut no_entropy);
        assert_eq!(denied["error"]["code"], "grant_missing", "{denied}");
        restarted.mailbox_bootstrap_boot_nonce = current_boot;
        let current_still_works = exchange(
            &mut new_listener,
            &mut restarted,
            &mut current,
            json!({
                "method":"mailbox.snapshot","bindingGeneration":current_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(current_still_works["result"]["type"], "mailbox_snapshot");
        crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .unwrap()
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        let replaced = exchange(
            &mut new_listener,
            &mut restarted,
            &mut current,
            json!({
                "method":"mailbox.snapshot","bindingGeneration":current_binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(replaced["error"]["code"], "grant_revoked");
        drop(no_entropy);
        drop(current);
        drop(new_listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn report_submit_is_advertised_and_returns_a_durable_connected_receipt() {
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["ok"], true);
        assert_eq!(
            descriptor["result"]["reportSubmit"],
            json!({"method": "report_submit", "protocol": crate::mailbox_v1::PROTOCOL})
        );
        assert!(descriptor["result"].get("parentReport").is_none());
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .expect("binding generation");

        let submitted = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "report-1",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "report-1", "revision": 1,
                    "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
                    "subject": "report", "body": "body", "messageId": "message-1",
                    "kind": "report", "priority": "normal", "originalSequence": 1
                }
            }),
        );
        assert_eq!(submitted["ok"], true);
        assert_eq!(submitted["result"]["receipt"]["status"], "admitted");
        let non_report = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "not-a-report",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "not-report", "revision": 1,
                    "digest": "c".repeat(64), "deliveryDigest": "d".repeat(64),
                    "subject": "assignment", "body": "body", "messageId": "message-2",
                    "kind": "assignment", "priority": "normal", "originalSequence": 2
                }
            }),
        );
        assert_eq!(non_report["ok"], false);
        assert_eq!(non_report["error"]["code"], "invalid_request");
        let recovered = crate::mailbox::MailboxStore::open(&directory)
            .expect("open mailbox")
            .load()
            .expect("read mailbox");
        assert!(recovered.receipts.contains_key(&"b".repeat(64)));
        assert_eq!(
            recovered.heads["report-1"].recipient,
            crate::mailbox::RecipientKey {
                recipient_id: sender,
                generation: "1".into(),
            }
        );

        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn report_submit_preserves_old_journal_for_current_successor_and_rejects_stale_routes() {
        let (mut app, directory, sender) = active_app();
        let logical_recipient = crate::mailbox::RecipientKey {
            recipient_id: sender.clone(),
            generation: "1".into(),
        };
        let legacy = app.handle_mailbox_offline_submit(
            "legacy".into(),
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id: format!("offline:{sender}:1"),
                recipient: logical_recipient.clone(),
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: "legacy-head".into(),
                    revision: 1,
                    digest: "c".repeat(64),
                    delivery_digest: "d".repeat(64),
                    subject: "legacy report".into(),
                    body: "legacy body".into(),
                    message_id: "legacy-message".into(),
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 1,
                },
                api_peer: None,
            },
        );
        assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&legacy).is_ok());

        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
                .expect("sender authority store");
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("install successor record");
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender)
            .expect("sender terminal")
            .set_managed_agent_generation(2);
        app.install_offline_mailbox_authority(
            sender_store
                .load()
                .expect("read successor record")
                .expect("active successor record"),
        )
        .expect("install successor authority");

        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .expect("successor binding")
            .to_owned();
        assert_eq!(descriptor["result"]["recipient"], json!(logical_recipient));
        assert_eq!(descriptor["result"]["historyOnly"], false);
        assert_eq!(
            descriptor["result"]["reportSubmit"]["method"],
            "report_submit"
        );
        let terminal_history = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot", "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(terminal_history["ok"], true);
        assert!(terminal_history["result"]["snapshot"]["heads"]
            .as_array()
            .unwrap()
            .is_empty());

        let unrelated = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "unrelated-route",
                "bindingGeneration": "mailbox-binding-unrelated",
                "params": {}
            }),
        );
        assert_eq!(unrelated["ok"], false);
        assert_eq!(unrelated["error"]["code"], "grant_revoked");

        let snapshot = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.snapshot", "requestId": "old-journal",
                "bindingGeneration": binding,
                "params": {"protocol": crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(snapshot["ok"], true);
        assert_eq!(
            snapshot["result"]["snapshot"]["heads"][0]["stableId"],
            "legacy-head"
        );

        let admitted = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "successor-report",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "successor-head", "revision": 1,
                    "digest": "e".repeat(64), "deliveryDigest": "f".repeat(64),
                    "subject": "successor report", "body": "body", "messageId": "successor-message",
                    "kind": "report", "priority": "normal", "originalSequence": 2
                }
            }),
        );
        assert_eq!(admitted["ok"], true);
        assert_eq!(admitted["result"]["receipt"]["status"], "admitted");

        sender_store
            .cas(
                Some(3),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender,
                    process_generation: 3,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 4,
                },
            )
            .expect("replace successor record");
        let revoked = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "revoked-successor",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "revoked-head", "revision": 1,
                    "digest": "1".repeat(64), "deliveryDigest": "2".repeat(64),
                    "subject": "revoked", "body": "body", "messageId": "revoked-message",
                    "kind": "report", "priority": "normal", "originalSequence": 3
                }
            }),
        );
        assert_eq!(revoked["ok"], false);
        assert_eq!(revoked["error"]["code"], "grant_revoked");

        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn successor_without_mutating_grant_reads_only_exact_settled_history_on_same_terminal() {
        let (mut app, directory, sender) = active_app();
        let recipient = crate::mailbox::RecipientKey {
            recipient_id: sender.clone(),
            generation: "1".into(),
        };
        let store = crate::mailbox::MailboxStore::open(&directory).unwrap();
        for (stable_id, digest, delivery_digest) in
            [("old-settled", "a", "b"), ("old-unresolved", "c", "d")]
        {
            let response = app.handle_mailbox_offline_submit(
                "seed".into(),
                crate::api::schema::MailboxOfflineSubmitParams {
                    caller: sender.clone(),
                    grant_id: format!("offline:{sender}:1"),
                    recipient: recipient.clone(),
                    submit: crate::mailbox_v1::Submit {
                        protocol: crate::mailbox_v1::PROTOCOL.into(),
                        stable_id: stable_id.into(),
                        revision: 1,
                        digest: digest.repeat(64),
                        delivery_digest: delivery_digest.repeat(64),
                        subject: "old".into(),
                        body: "old body".into(),
                        message_id: stable_id.into(),
                        kind: "report".into(),
                        priority: "normal".into(),
                        original_sequence: 1,
                    },
                    api_peer: None,
                },
            );
            assert!(serde_json::from_str::<crate::api::schema::SuccessResponse>(&response).is_ok());
        }
        let settled = store.claim_next(&recipient).unwrap().unwrap();
        assert_eq!(settled.stable_id, "old-settled");
        store
            .resolve_claim(
                &settled.claim_id,
                crate::mailbox::ClaimResolutionOutcome::Settled,
            )
            .unwrap();
        let parent_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_edge = app.state.delegations.create(None, None, None).unwrap();
        let child_edge = app
            .state
            .delegations
            .create(Some(parent_pane), Some(parent_edge), None)
            .unwrap();
        let sender_store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender).unwrap();
        sender_store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision: 3,
                },
            )
            .unwrap();
        sender_store.promote_active(&sender, 2).unwrap();
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender)
            .unwrap()
            .set_managed_agent_generation(2);
        // Simulate the actual RED boundary: durable gen2 is Active and the
        // foreground Pi is current, but App missed installing a mutating grant.
        assert!(!app.offline_mailbox_authority_current(&sender).unwrap());
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["ok"], true);
        assert_eq!(descriptor["result"]["historyOnly"], true);
        assert!(descriptor["result"].get("childDoneSignals").is_none());
        assert!(descriptor["result"].get("parentSignals").is_none());
        assert_eq!(
            descriptor["result"]["historySnapshot"],
            json!({
                "method":"mailbox.history_snapshot","protocol":crate::mailbox_v1::PROTOCOL
            })
        );
        assert!(descriptor["result"].get("reportSubmit").is_none());
        assert_eq!(descriptor["result"]["grantId"], "");
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let history = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(history["ok"], true);
        let snapshot = &history["result"]["snapshot"];
        assert_eq!(snapshot["heads"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["heads"][0]["stableId"], "old-settled");
        assert_eq!(snapshot["receipts"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["headStates"][0]["lifecycle"], "settled");
        assert!(snapshot.get("claim").is_none());
        for method in [
            "report_submit",
            "mailbox.offline_submit",
            "mailbox.snapshot",
            "mailbox.claim",
            "mailbox.edit",
            "mailbox.resolve",
        ] {
            let denied = exchange(
                &mut listener,
                &mut app,
                &mut client,
                json!({
                    "method":method,"bindingGeneration":binding,"params":{}
                }),
            );
            assert_eq!(
                denied["error"]["code"], "grant_revoked",
                "{method}: {denied}"
            );
        }
        let forged = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot","bindingGeneration":"unrelated","params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(forged["error"]["code"], "grant_revoked");
        let selector = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot", "bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL,
                    "recipient":{"recipientId":"unrelated","generation":"1"}}
            }),
        );
        assert_eq!(selector["error"]["code"], "invalid_request");
        app.state.delegations.reparent(child_edge, None).unwrap();
        let revoked = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot","bindingGeneration":binding,"params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(revoked["error"]["code"], "grant_revoked");
        app.state
            .delegations
            .reparent(child_edge, Some(parent_edge))
            .unwrap();
        sender_store
            .cas(
                Some(4),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender.clone(),
                    process_generation: 3,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 5,
                },
            )
            .unwrap();
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender)
            .unwrap()
            .set_managed_agent_generation(3);
        let stale = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method":"mailbox.history_snapshot","bindingGeneration":binding,
                "params":{"protocol":crate::mailbox_v1::PROTOCOL}
            }),
        );
        assert_eq!(stale["error"]["code"], "grant_revoked");
        sender_store
            .cas(
                Some(5),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: "unrelated".into(),
                    process_generation: 4,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 6,
                },
            )
            .unwrap();
        app.state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == sender)
            .unwrap()
            .set_managed_agent_generation(4);
        let mut unrelated_client = UnixStream::connect(listener.path()).unwrap();
        unrelated_client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let unrelated_bootstrap = bootstrap(&mut listener, &mut app, &mut unrelated_client);
        assert_eq!(unrelated_bootstrap["error"]["code"], "grant_missing");
        let recovered = crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap();
        assert_eq!(recovered.heads.len(), 2);
        assert_eq!(recovered.receipts.len(), 2);
        assert_eq!(
            recovered.resolutions[&settled.claim_id].outcome,
            crate::mailbox::ClaimResolutionOutcome::Settled
        );
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn managed_pi_lifecycle_candidate_bootstraps_without_agent_environment_hint() {
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");

        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(descriptor["ok"], true);
        assert_eq!(descriptor["result"]["caller"], sender);
        assert_eq!(descriptor["result"]["activeExecutionGeneration"], 1);

        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bootstrap_provisions_a_current_managed_recipient_and_admits_cross_submit() {
        let (mut app, directory, sender) = active_app();
        let (recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
        relate_sender_and_recipient(&mut app);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .expect("set read timeout");
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .expect("binding generation");
        let provisioned = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.provision_recipient", "requestId": "provision-1",
                "bindingGeneration": binding,
                "params": {"target": recipient_target}
            }),
        );
        assert_eq!(provisioned["ok"], true);
        assert_eq!(provisioned["result"]["type"], "mailbox_grant_provisioned");
        let grant_id = provisioned["result"]["grant"]["grantId"]
            .as_str()
            .expect("server-minted grant")
            .to_owned();
        assert_eq!(grant_id, format!("mailbox:{sender}:1:{recipient}:1"));
        assert_eq!(
            provisioned["result"]["grant"]["sender"]["recipientId"],
            sender
        );
        assert_eq!(
            provisioned["result"]["grant"]["recipient"]["recipientId"],
            recipient
        );

        let submitted = app.handle_mailbox_offline_submit(
            "cross-submit".into(),
            crate::api::schema::MailboxOfflineSubmitParams {
                caller: sender.clone(),
                grant_id,
                recipient: crate::mailbox::RecipientKey {
                    recipient_id: recipient.clone(),
                    generation: "1".into(),
                },
                submit: crate::mailbox_v1::Submit {
                    protocol: crate::mailbox_v1::PROTOCOL.into(),
                    stable_id: "a-to-b".into(),
                    revision: 1,
                    digest: "a".repeat(64),
                    delivery_digest: "b".repeat(64),
                    subject: "cross-recipient".into(),
                    body: "body".into(),
                    message_id: "cross-message".into(),
                    kind: "advisory".into(),
                    priority: "normal".into(),
                    original_sequence: 1,
                },
                api_peer: None,
            },
        );
        let submitted: crate::api::schema::SuccessResponse =
            serde_json::from_str(&submitted).expect("cross-recipient admitted");
        assert!(matches!(
            submitted.result,
            crate::api::schema::ResponseResult::MailboxOfflineSubmitted { .. }
        ));
        let durable = crate::mailbox::MailboxStore::open(&directory)
            .expect("open durable mailbox")
            .load()
            .expect("read durable mailbox");
        assert!(durable
            .grants
            .contains_key(&format!("mailbox:{sender}:1:{recipient}:1")));

        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bootstrap_provision_rejects_inactive_sender() {
        let (mut app, directory, _sender) = active_app();
        let (_recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let sender_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        app.handle_internal_event(crate::events::AppEvent::PaneDied {
            pane_id: sender_pane,
            process_generation: None,
        });
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.provision_recipient", "bindingGeneration": binding,
                "params": {"target": recipient_target}
            }),
        );
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bootstrap_provision_rejects_stale_sender_generation() {
        let (mut app, directory, sender) = active_app();
        let (_recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .expect("sender authority store");
        store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender,
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace sender generation");
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.provision_recipient", "bindingGeneration": binding,
                "params": {"target": recipient_target}
            }),
        );
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    /// provision_recipient is limited to the caller's own delegation edges.
    #[test]
    fn bootstrap_provision_refuses_an_unrelated_recipient() {
        let (mut app, directory, _sender) = active_app();
        let (_recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let refused = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.provision_recipient", "requestId": "unrelated",
                "bindingGeneration": binding,
                "params": {"target": recipient_target}
            }),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["error"]["code"], "mailbox_recipient_not_allowed");
        assert!(crate::mailbox::MailboxStore::open(&directory)
            .unwrap()
            .load()
            .unwrap()
            .grants
            .is_empty());
    }

    #[test]
    fn bootstrap_provision_rejects_mismatched_recipient_generation() {
        let (mut app, directory, _sender) = active_app();
        let (recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
        relate_sender_and_recipient(&mut app);
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let store =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &recipient)
                .expect("recipient authority store");
        store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: recipient,
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace recipient generation");
        let rejected = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.provision_recipient", "bindingGeneration": binding,
                "params": {"target": recipient_target}
            }),
        );
        assert_eq!(rejected["ok"], false);
        assert_eq!(rejected["error"]["code"], "grant_revoked");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn bootstrap_without_active_sender_is_unavailable() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        let directory = unique_dir();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let response = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "grant_missing");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn replacement_revokes_stale_report_submit_binding_before_dispatch() {
        let (mut app, directory, sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender)
            .expect("authority store");
        store
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender,
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .expect("replace active generation");
        let response = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "report_submit", "requestId": "stale-1",
                "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "stableId": "stale-report", "revision": 1,
                    "digest": "a".repeat(64), "deliveryDigest": "b".repeat(64),
                    "subject": "stale", "body": "body", "messageId": "stale-message",
                    "kind": "report", "priority": "normal", "originalSequence": 1
                }
            }),
        );
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"]["code"], "grant_revoked");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn optin_path_attempt_requires_latest_current_todo_and_taints_typed_receipt() {
        use crate::child_report::{ChildReportEvent, ReportDispositionKind};
        let (mut app, directory, sender) = active_app();
        let (_parent, _) = active_managed_recipient(&mut app, &directory);
        let child_pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let parent_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let parent_id = app
            .state
            .delegations
            .create(Some(parent_pane), None, None)
            .unwrap();
        let child_id = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
        let mut listener = listener(&directory);
        let mut before = UnixStream::connect(listener.path()).unwrap();
        before
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let early = bootstrap(&mut listener, &mut app, &mut before);
        assert!(early["result"]["parentReport"].is_null());
        let early_binding = early["result"]["bindingGeneration"].as_str().unwrap();
        let mut attempt = json!({
            "protocol": crate::mailbox_v1::PROTOCOL,
            "path":"automatic_export_handoff", "localRoot":"root", "localRevision":1,
            "stateDigest":"a".repeat(64), "reportDigest":"b".repeat(64),
            "reportId":"report-one", "messageId":"envelope-one"
        });
        let request = |binding: &str, params: Value| {
            json!({
                "method":"report_path_attempt", "bindingGeneration":binding, "params":params
            })
        };
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut before,
            request(early_binding, attempt.clone()),
        );
        assert_eq!(denied["error"]["code"], "grant_missing", "{denied}");
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut before,
            request(early_binding, attempt.clone()),
        );
        assert_ne!(denied["ok"], true, "pre-ready FD cannot be promoted");
        let mut client = UnixStream::connect(listener.path()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        assert_eq!(
            descriptor["result"]["parentReport"]["pathAttemptMethod"],
            "report_path_attempt"
        );
        assert_eq!(
            descriptor["result"]["parentReport"]["coverageQualified"],
            false
        );
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let no_todo = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        assert_eq!(no_todo["error"]["code"], "invalid_request", "{no_todo}");
        let todo = json!({"protocol":crate::mailbox_v1::PROTOCOL, "localRoot":"root",
            "localRevision":1, "stateDigest":"a".repeat(64), "state":"not_done"});
        let observed = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"todo_state", "bindingGeneration":binding, "params":todo}),
        );
        assert_eq!(observed["result"]["type"], "todo_state", "{observed}");
        for (field, value) in [
            ("localRoot", json!("wrong-root")),
            ("localRevision", json!(2)),
            ("stateDigest", json!("f".repeat(64))),
            ("reportDigest", json!("not-a-digest")),
            ("path", json!("unverified_api_sender")),
            ("protocol", json!("wrong-protocol")),
        ] {
            let mut wrong = attempt.clone();
            wrong[field] = value;
            let denied = exchange(
                &mut listener,
                &mut app,
                &mut client,
                request(binding, wrong),
            );
            assert_eq!(
                denied["error"]["code"], "invalid_request",
                "{field}: {denied}"
            );
        }
        for selector in [
            "caller",
            "parent",
            "recipient",
            "grantId",
            "session",
            "generation",
            "routeEpoch",
        ] {
            let mut forged = attempt.clone();
            forged[selector] = json!("forged");
            let denied = exchange(
                &mut listener,
                &mut app,
                &mut client,
                request(binding, forged),
            );
            assert_eq!(
                denied["error"]["code"], "invalid_request",
                "{selector}: {denied}"
            );
        }
        let stale_binding = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request("forged", attempt.clone()),
        );
        assert_eq!(stale_binding["error"]["code"], "grant_revoked");
        let ordinary_api = json!({"id":"forged", "method":"report_path_attempt",
                                 "params":attempt});
        assert!(
            serde_json::from_value::<crate::api::schema::Request>(ordinary_api).is_err(),
            "generic same-UID API is not the accepted managed Pi FD"
        );
        // Fail an actual append (not a merely invalid wire request), and
        // restore only this test-owned stream before continuing.
        let stream_path = directory.join(crate::mailbox::RECORD_STREAM_FILE);
        let original = std::fs::metadata(&stream_path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&stream_path, readonly).unwrap();
        let failed = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        std::fs::set_permissions(&stream_path, original).unwrap();
        assert_eq!(
            failed["ok"], false,
            "failed journal append cannot ACK: {failed}"
        );
        assert!(!crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap()
            .child_report_events
            .iter()
            .any(|event| matches!(event, ChildReportEvent::PathAttempt { .. })));
        let authority = app.offline_mailbox_authorities.get(&sender).unwrap();
        authority.store.test_fail_path_attempt_readback(true);
        let failed_after_fsync = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        app.offline_mailbox_authorities
            .get(&sender)
            .unwrap()
            .store
            .test_fail_path_attempt_readback(false);
        assert_eq!(
            failed_after_fsync["ok"], false,
            "no ACK when post-fsync readback fails: {failed_after_fsync}"
        );
        let persisted = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        let failed_route = persisted
            .child_report_events
            .iter()
            .find_map(|event| match event {
                ChildReportEvent::PathAttempt { route, .. } => Some(route),
                _ => None,
            })
            .expect("fsynced marker remains even though ACK failed");
        assert_eq!(
            crate::child_report::project(failed_route, &persisted).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        // Reconciliation after an uncertain Pi effect can retrieve only the
        // original cursor, not authority to repeat the send.
        let first = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        assert_eq!(first["result"]["type"], "report_path_attempt", "{first}");
        let result = &first["result"];
        assert_eq!(result["routeAuthenticated"], true);
        for field in [
            "canonicalCommitVerified",
            "coverageQualified",
            "effectRetryAuthorized",
        ] {
            assert_eq!(result[field], false, "{field}: {first}");
        }
        assert!(result["routeEpoch"]
            .as_str()
            .is_some_and(|epoch| !epoch.is_empty()));
        let cursor = result["cursor"].as_u64().unwrap();
        let mut conflict = attempt.clone();
        conflict["messageId"] = json!("different-message");
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, conflict),
        );
        assert_eq!(denied["error"]["code"], "invalid_request");
        let mut conflict = attempt.clone();
        conflict["reportId"] = json!("different-report");
        let denied = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, conflict),
        );
        assert_eq!(denied["error"]["code"], "invalid_request");
        let prepared = json!({"protocol":crate::mailbox_v1::PROTOCOL,
            "localRoot":"root", "localRevision":1, "reportId":"report-one",
            "reportDigest":"b".repeat(64), "stableId":"typed-one", "submitRevision":1,
            "submitDigest":"c".repeat(64), "deliveryDigest":"d".repeat(64),
            "messageId":"envelope-one"});
        let ready = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"report_prepared", "bindingGeneration":binding, "params":prepared}),
        );
        assert_eq!(ready["result"]["type"], "report_prepared", "{ready}");
        let duplicate = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        assert_eq!(
            duplicate["result"]["cursor"], cursor,
            "original cursor despite later record"
        );
        assert_eq!(duplicate["result"]["effectRetryAuthorized"], false);
        let submit = json!({"protocol":crate::mailbox_v1::PROTOCOL,
            "stableId":"typed-one", "revision":1, "digest":"c".repeat(64),
            "deliveryDigest":"d".repeat(64), "subject":"report", "body":"body",
            "messageId":"envelope-one", "kind":"report", "priority":"normal",
            "originalSequence":1});
        let admitted = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"report_submit_parent", "bindingGeneration":binding, "params":submit}),
        );
        assert_eq!(
            admitted["result"]["receipt"]["status"], "admitted",
            "{admitted}"
        );
        let store = crate::mailbox::MailboxStore::existing(&directory);
        let recovered = store.load().unwrap();
        let route = recovered
            .child_report_events
            .iter()
            .find_map(|event| match event {
                ChildReportEvent::PathAttempt {
                    route,
                    cursor: stored_cursor,
                    ..
                } => {
                    assert_eq!(*stored_cursor, cursor);
                    Some(route.clone())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(result["routeEpoch"], route.route_epoch);
        let recorded_attempt = recovered
            .child_report_events
            .iter()
            .find(|event| matches!(event, ChildReportEvent::PathAttempt { .. }))
            .unwrap()
            .clone();
        assert_eq!(
            store
                .append_child_report_event(recorded_attempt)
                .unwrap_err(),
            crate::mailbox::MailboxError::InvalidRecord,
            "generic journal append must not mint or replay path attempts"
        );
        assert_eq!(
            crate::child_report::project(&route, &recovered).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        let mut replaced = route.clone();
        replaced.route_epoch.push_str("-replaced");
        assert_eq!(
            crate::child_report::project(&replaced, &store.load().unwrap()).kind,
            ReportDispositionKind::StaleOrReplaced
        );
        let todo2 = json!({"protocol":crate::mailbox_v1::PROTOCOL, "localRoot":"root",
            "localRevision":2, "stateDigest":"e".repeat(64), "state":"done"});
        let newer = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method":"todo_state", "bindingGeneration":binding, "params":todo2}),
        );
        assert_eq!(newer["result"]["type"], "todo_state", "{newer}");
        let stale = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt.clone()),
        );
        assert_eq!(
            stale["error"]["code"], "invalid_request",
            "older ACK no longer latest"
        );
        attempt["localRevision"] = json!(2);
        attempt["stateDigest"] = json!("e".repeat(64));
        attempt["reportId"] = json!("report-two");
        attempt["messageId"] = json!("envelope-two");
        attempt["path"] = json!("legacy_bound_submit");
        let second = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, attempt),
        );
        assert_eq!(second["result"]["type"], "report_path_attempt", "{second}");
        let after_restart = crate::mailbox::MailboxStore::existing(&directory)
            .load()
            .unwrap();
        assert_eq!(
            crate::child_report::project(&route, &after_restart).kind,
            ReportDispositionKind::InFlightOrUncertain
        );
        let authority =
            crate::sender_authority::SenderAuthorityStore::for_sender(&directory, &sender).unwrap();
        authority
            .cas(
                Some(2),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key: sender,
                    process_generation: 2,
                    phase: crate::sender_authority::SenderAuthorityPhase::Active,
                    transition_revision: 3,
                },
            )
            .unwrap();
        let old = exchange(
            &mut listener,
            &mut app,
            &mut client,
            request(binding, json!({"protocol":crate::mailbox_v1::PROTOCOL})),
        );
        assert_eq!(old["error"]["code"], "grant_revoked");
        drop(listener);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dispatch_rejects_methods_and_scope_selectors_outside_descriptor() {
        let (mut app, directory, _sender) = active_app();
        let mut listener = listener(&directory);
        let mut client = UnixStream::connect(listener.path()).expect("connect bootstrap socket");
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let descriptor = bootstrap(&mut listener, &mut app, &mut client);
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
        let other_method = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({"method": "server.stop", "bindingGeneration": binding, "params": {}}),
        );
        assert_eq!(other_method["error"]["code"], "invalid_request");
        let client_scope = exchange(
            &mut listener,
            &mut app,
            &mut client,
            json!({
                "method": "mailbox.snapshot", "bindingGeneration": binding,
                "params": {
                    "protocol": crate::mailbox_v1::PROTOCOL,
                    "caller": "attacker", "grantId": "attacker", "recipient": {"recipientId":"attacker","generation":"9"}
                }
            }),
        );
        assert_eq!(client_scope["error"]["code"], "invalid_request");
        drop(listener);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}
