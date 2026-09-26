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
    /// Offered only when the server can validate an exact active delegation
    /// parent. Admission here is a durable mailbox receipt, not Pi Gate
    /// admission or the parent's acceptance of the report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_report: Option<ParentReportAdvertisement>,
    /// #159, feature-gated: typed child-report signals for this parent and
    /// its recovery request. Never mailbox heads or prompt text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_signals: Option<ParentSignalsAdvertisement>,
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
    /// #159: `true` only for a covered child whose domain names this ready
    /// route, from route readiness onward. Pi keys the child role on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_child: Option<bool>,
    /// #159/#161: offered only to a covered child, always alongside
    /// `todoStateMethod`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_wait_method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_decline_method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_wake_method: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ParentSignalsAdvertisement {
    pub method: &'static str,
    pub recovery_method: &'static str,
    pub bind_method: &'static str,
    pub protocol: &'static str,
}

impl MailboxBootstrapDescriptor {
    fn from_session(session: &MailboxBootstrapSession, endpoint: &Path) -> Self {
        Self {
            protocol_version: MAILBOX_BOOTSTRAP_PROTOCOL_VERSION,
            report_submit: (!session.history_only).then_some(ReportSubmitAdvertisement {
                method: "report_submit",
                protocol: crate::mailbox_v1::PROTOCOL,
            }),
            history_snapshot: ReportSubmitAdvertisement {
                method: "mailbox.history_snapshot",
                protocol: crate::mailbox_v1::PROTOCOL,
            },
            history_only: session.history_only,
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
                    covered_child: session.covered_child.then_some(true),
                    recovery_wait_method: session.covered_child.then_some("report_recovery_wait"),
                    recovery_decline_method: session
                        .covered_child
                        .then_some("report_recovery_decline"),
                    recovery_wake_method: session
                        .covered_child
                        .then_some("report_recovery_wake_request"),
                }),
            parent_signals: session
                .parent_signals
                .then_some(ParentSignalsAdvertisement {
                    method: "child_report_signals",
                    recovery_method: "report_recovery_request",
                    bind_method: "todo_delegation_bind",
                    protocol: crate::mailbox_v1::PROTOCOL,
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
        MailboxBootstrapError::EgressFrozen => (
            "egress_frozen",
            "the covered child's report egress is closed by its closure barrier",
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
    /// #159 long-poll parked until a result exists or its deadline passes.
    /// While parked, later frames stay buffered in order.
    parked: Option<ParkedPoll>,
}

struct ParkedPoll {
    request_id: Option<String>,
    method: String,
    params: Value,
    deadline: std::time::Instant,
}

enum Handled {
    Respond(String),
    Park(ParkedPoll),
}

/// Methods that may park, and the result array that must be nonempty.
fn long_poll_items(method: &str) -> Option<&'static str> {
    match method {
        "child_report_signals" => Some("signals"),
        "report_recovery_wait" => Some("requests"),
        _ => None,
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
                            parked: None,
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
            if let Some(parked) = connection.parked.take() {
                match Self::resume_parked(app, connection, parked) {
                    Handled::Respond(response) => {
                        if write_response(&mut connection.stream, &response).is_err() {
                            closed.push(*id);
                            continue;
                        }
                    }
                    Handled::Park(parked) => {
                        connection.parked = Some(parked);
                        continue;
                    }
                }
            }
            while let Some(end) = connection.input.iter().position(|byte| *byte == b'\n') {
                let frame: Vec<u8> = connection.input.drain(..=end).collect();
                let request = std::str::from_utf8(&frame[..frame.len().saturating_sub(1)])
                    .ok()
                    .and_then(|frame| serde_json::from_str::<BootstrapRequest>(frame).ok());
                let response = match request {
                    Some(request) => match Self::handle_request(app, connection, request) {
                        Handled::Respond(response) => response,
                        Handled::Park(parked) => {
                            connection.parked = Some(parked);
                            break;
                        }
                    },
                    None => failure(None, MailboxBootstrapError::InvalidRequest),
                };
                if write_response(&mut connection.stream, &response).is_err() {
                    closed.push(*id);
                    break;
                }
            }
        }
        // A closed stream's server-issued binding dies with it, so probing
        // clients cannot accumulate stale bindings.
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

    /// Re-run a parked poll through full authentication. A revoked stream
    /// gets its error immediately; an expired deadline gets the empty page.
    fn resume_parked(
        app: &mut App,
        connection: &mut AcceptedMailboxConnection,
        parked: ParkedPoll,
    ) -> Handled {
        let Some(session) = connection.session.as_ref() else {
            return Handled::Respond(failure(
                parked.request_id,
                MailboxBootstrapError::GrantMissing,
            ));
        };
        let items = long_poll_items(&parked.method).unwrap_or("signals");
        match app.dispatch_mailbox_bootstrap(session, &parked.method, parked.params.clone()) {
            Ok(result)
                if result[items].as_array().is_some_and(|list| list.is_empty())
                    && std::time::Instant::now() < parked.deadline =>
            {
                Handled::Park(parked)
            }
            Ok(result) => Handled::Respond(
                serde_json::to_string(&BootstrapSuccess {
                    ok: true,
                    request_id: parked.request_id,
                    result,
                })
                .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            ),
            Err(error) => Handled::Respond(failure(parked.request_id, error)),
        }
    }

    fn handle_request(
        app: &mut App,
        connection: &mut AcceptedMailboxConnection,
        request: BootstrapRequest,
    ) -> Handled {
        let request_id = request.request_id;
        if request.method == "bootstrap" {
            if connection.session.is_some() {
                return Handled::Respond(failure(
                    request_id,
                    MailboxBootstrapError::InvalidRequest,
                ));
            }
            let session = match app.accept_mailbox_bootstrap_stream(connection.stream.as_raw_fd()) {
                Ok(session) => session,
                Err(error) => return Handled::Respond(failure(request_id, error)),
            };
            let descriptor = MailboxBootstrapDescriptor::from_session(
                &session,
                &mailbox_bootstrap_socket_path(),
            );
            connection.session = Some(session);
            return Handled::Respond(
                serde_json::to_string(&BootstrapSuccess {
                    ok: true,
                    request_id,
                    result: descriptor,
                })
                .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            );
        }

        let Some(session) = connection.session.as_ref() else {
            return Handled::Respond(failure(request_id, MailboxBootstrapError::GrantMissing));
        };
        if request.binding_generation.as_deref() != Some(&session.binding_generation) {
            return Handled::Respond(failure(request_id, MailboxBootstrapError::GrantRevoked));
        }
        let wait_ms = long_poll_items(&request.method)
            .and_then(|_| request.params.get("waitMs"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(crate::child_report_closure::MAX_WAIT_MS);
        let params = request.params.clone();
        let result = app.dispatch_mailbox_bootstrap(session, &request.method, request.params);
        match result {
            Ok(result)
                if wait_ms > 0
                    && long_poll_items(&request.method).is_some_and(|items| {
                        result[items].as_array().is_some_and(Vec::is_empty)
                    }) =>
            {
                Handled::Park(ParkedPoll {
                    request_id,
                    method: request.method,
                    params,
                    deadline: std::time::Instant::now() + std::time::Duration::from_millis(wait_ms),
                })
            }
            Ok(result) => Handled::Respond(
                serde_json::to_string(&BootstrapSuccess {
                    ok: true,
                    request_id,
                    result,
                })
                .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            ),
            Err(error) => Handled::Respond(failure(request_id, error)),
        }
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
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 2,
                },
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
                    kind: "report".into(),
                    priority: "normal".into(),
                    original_sequence: 1,
                },
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

    #[test]
    fn bootstrap_provision_rejects_mismatched_recipient_generation() {
        let (mut app, directory, _sender) = active_app();
        let (recipient, recipient_target) = active_managed_recipient(&mut app, &directory);
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

    // ---- #159 covered-child closure, signals and recovery ----

    struct TestVerifier(Option<u64>);

    impl crate::child_report_closure::EnforcementVerifier for TestVerifier {
        fn verify(
            &self,
            query: &crate::child_report_closure::EnforcementQuery<'_>,
        ) -> Result<crate::child_report_closure::EnforcementAttestation, String> {
            let late_by = self.0.ok_or_else(|| "test_refused".to_string())?;
            Ok(crate::child_report_closure::EnforcementAttestation {
                policy_hash: query.policy.policy_hash.clone(),
                covered_from_birth: crate::child_report_closure::BirthRecord {
                    pid: query.managed_launch_birth.pid,
                    start_ticks: query.managed_launch_birth.start_ticks + late_by,
                },
            })
        }
    }

    struct Covered {
        app: App,
        directory: PathBuf,
        listener: MailboxBootstrapListener,
        child: UnixStream,
        child_binding: String,
        parent: UnixStream,
        parent_binding: String,
        parent_descriptor: Value,
        child_id: crate::delegation::DelegationId,
        child_terminal: crate::terminal::TerminalId,
        parent_key: String,
    }

    fn terminal_for(app: &App, key: &str) -> crate::terminal::TerminalId {
        app.state
            .terminals
            .keys()
            .find(|id| id.to_string() == key)
            .unwrap()
            .clone()
    }

    fn connect(listener: &MailboxBootstrapListener) -> UnixStream {
        let stream = UnixStream::connect(listener.path()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
    }

    /// Both Pis share this test process as peer; hide one terminal's
    /// foreground job so the other is the only bootstrap candidate.
    fn bootstrap_hiding(
        app: &mut App,
        listener: &mut MailboxBootstrapListener,
        hide: &crate::terminal::TerminalId,
    ) -> (UnixStream, Value) {
        let hidden = app
            .mailbox_bootstrap_test_foreground_jobs
            .remove(hide)
            .unwrap();
        let mut stream = connect(listener);
        let descriptor = bootstrap(listener, app, &mut stream);
        app.install_mailbox_bootstrap_test_foreground_job(hide.clone(), hidden);
        (stream, descriptor)
    }

    fn covered_policy(
        birth: crate::platform::ProcessBirthIdentity,
    ) -> crate::child_report_closure::CoveredLaunchPolicy {
        crate::child_report_closure::CoveredLaunchPolicy {
            policy_id: "sandbox-145".into(),
            policy_hash: "e".repeat(64),
            receipt_digest: "d".repeat(64),
            sandboxed_birth: birth.into(),
        }
    }

    fn covered_fixture(enabled: bool, register: bool, verifier: Option<u64>) -> Covered {
        let (mut app, directory, child_key) = active_app();
        app.child_report_signals_enabled = enabled;
        app.enforcement_verifier = std::sync::Arc::new(TestVerifier(verifier));
        let (parent_key, _) = active_managed_recipient(&mut app, &directory);
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
        let child_terminal = terminal_for(&app, &child_key);
        if register {
            let birth = app.managed_pi_launches[&child_terminal].process.unwrap();
            assert_eq!(
                app.register_covered_child_launch(child_terminal.clone(), covered_policy(birth)),
                enabled
            );
        }
        ready_test_route(&mut app, &directory, child_id, parent_id);
        let mut listener = listener(&directory);
        let mut child = connect(&listener);
        let descriptor = bootstrap(&mut listener, &mut app, &mut child);
        assert_eq!(descriptor["result"]["caller"], child_key, "{descriptor}");
        let covered = enabled && register;
        if covered {
            assert_eq!(descriptor["result"]["parentReport"]["coveredChild"], true);
        } else {
            assert!(descriptor["result"]["parentReport"]["coveredChild"].is_null());
        }
        for (field, method) in [
            ("recoveryWaitMethod", "report_recovery_wait"),
            ("recoveryDeclineMethod", "report_recovery_decline"),
            ("recoveryWakeMethod", "report_recovery_wake_request"),
        ] {
            if covered {
                assert_eq!(descriptor["result"]["parentReport"][field], method);
                assert_eq!(
                    descriptor["result"]["parentReport"]["todoStateMethod"],
                    "todo_state"
                );
            } else {
                assert!(
                    descriptor["result"]["parentReport"][field].is_null(),
                    "{field}"
                );
            }
        }
        let child_binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        // The parent Pi becomes this test process for its own stream.
        install_trusted_test_pi(&mut app, &directory, &parent_key, std::process::id());
        let (parent, parent_descriptor) =
            bootstrap_hiding(&mut app, &mut listener, &child_terminal);
        assert_eq!(parent_descriptor["result"]["caller"], parent_key);
        let parent_binding = parent_descriptor["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        Covered {
            app,
            directory,
            listener,
            child,
            child_binding,
            parent,
            parent_binding,
            parent_descriptor,
            child_id,
            child_terminal,
            parent_key,
        }
    }

    fn call(fx: &mut Covered, as_parent: bool, method: &str, params: Value) -> Value {
        let Covered {
            app,
            listener,
            child,
            parent,
            child_binding,
            parent_binding,
            ..
        } = fx;
        let (stream, binding) = if as_parent {
            (parent, parent_binding.clone())
        } else {
            (child, child_binding.clone())
        };
        exchange(
            listener,
            app,
            stream,
            json!({"method":method,"bindingGeneration":binding,"params":params}),
        )
    }

    fn state_digest(revision: u64) -> String {
        format!("{revision:064x}")
    }

    fn todo(revision: u64, done: bool) -> Value {
        json!({"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"child-root",
               "localRevision":revision,"stateDigest":state_digest(revision),
               "state": if done { "done" } else { "not_done" }})
    }

    fn path_attempt(revision: u64, id: &str) -> Value {
        json!({"protocol":crate::mailbox_v1::PROTOCOL,"path":"legacy_bound_submit",
               "localRoot":"child-root","localRevision":revision,
               "stateDigest":state_digest(revision),"reportDigest":"c".repeat(64),
               "reportId":format!("report-{id}"),"messageId":format!("message-{id}")})
    }

    fn preparation(revision: u64, id: &str) -> Value {
        json!({"protocol":crate::mailbox_v1::PROTOCOL,"localRoot":"child-root",
               "localRevision":revision,"reportId":format!("report-{id}"),
               "reportDigest":"a".repeat(64),"stableId":format!("stable-{id}"),
               "submitRevision":1,"submitDigest":"a".repeat(64),
               "deliveryDigest":format!("{:064x}", id.len() as u64 + 0xb00),
               "messageId":format!("message-{id}")})
    }

    fn submission(id: &str) -> Value {
        json!({"protocol":crate::mailbox_v1::PROTOCOL,"stableId":format!("stable-{id}"),
               "revision":1,"digest":"a".repeat(64),
               "deliveryDigest":format!("{:064x}", id.len() as u64 + 0xb00),
               "subject":"report","body":"body","messageId":format!("message-{id}"),
               "kind":"report","priority":"normal","originalSequence":1})
    }

    fn signals(fx: &mut Covered, after: u64) -> Vec<Value> {
        let page = call(
            fx,
            true,
            "child_report_signals",
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":after}),
        );
        page["result"]["signals"]
            .as_array()
            .unwrap_or_else(|| panic!("{page}"))
            .clone()
    }

    fn recovery_request(signal: &Value) -> Value {
        json!({"protocol":crate::mailbox_v1::PROTOCOL,
               "signalCursor":signal["cursor"],
               "routeEpoch":signal["route"]["routeEpoch"],
               "childTerminalId":signal["route"]["childTerminalId"],
               "localRoot":signal["todo"]["localRoot"],
               "localRevision":signal["todo"]["localRevision"],
               "stateDigest":signal["todo"]["stateDigest"]})
    }

    fn closure_journal(fx: &Covered) -> crate::child_report_closure::ClosureJournal {
        crate::child_report_closure::load_closure(&crate::mailbox::MailboxStore::existing(
            &fx.directory,
        ))
        .unwrap()
    }

    fn current_route(fx: &Covered) -> crate::child_report::RouteIdentity {
        crate::mailbox::MailboxStore::existing(&fx.directory)
            .load()
            .unwrap()
            .child_report_events
            .iter()
            .rev()
            .find_map(|event| match event {
                crate::child_report::ChildReportEvent::TodoState { route, .. } => {
                    Some(route.clone())
                }
                _ => None,
            })
            .unwrap()
    }

    fn finish(fx: Covered) {
        let directory = fx.directory.clone();
        drop(fx);
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn covered_quiet_done_signals_missing_then_recovers_exactly_once() {
        let mut fx = covered_fixture(true, true, Some(0));
        assert_eq!(
            fx.parent_descriptor["result"]["parentSignals"]["method"],
            "child_report_signals"
        );
        assert_eq!(
            fx.parent_descriptor["result"]["parentSignals"],
            json!({"method":"child_report_signals","recoveryMethod":"report_recovery_request",
                   "bindMethod":"todo_delegation_bind","protocol":crate::mailbox_v1::PROTOCOL})
        );
        assert_eq!(
            call(&mut fx, false, "todo_state", todo(1, false))["result"]["type"],
            "todo_state"
        );
        assert!(signals(&mut fx, 0).is_empty(), "not_done closes nothing");
        let done = call(&mut fx, false, "todo_state", todo(2, true));
        let done_cursor = done["result"]["cursor"].as_u64().unwrap();
        let page = signals(&mut fx, 0);
        assert_eq!(page.len(), 1, "{page:?}");
        let signal = page[0].clone();
        assert_eq!(signal["type"], "missing_after_done", "{signal}");
        assert!(signal["reason"].is_null());
        assert_eq!(
            signal["route"]["childDelegationId"],
            fx.child_id.to_string()
        );
        assert_eq!(signal["route"]["parentTerminalId"], fx.parent_key);
        assert_eq!(signal["route"]["routeEpoch"], done["result"]["routeEpoch"]);
        assert_eq!(signal["todo"]["todoStateCursor"], done_cursor);
        assert_eq!(signal["todo"]["localRevision"], 2);
        assert_eq!(signal["todo"]["state"], "done");
        for count in ["pathAttemptCount", "preparedCount", "admittedReportCount"] {
            assert_eq!(signal["closure"][count], 0, "{count}");
        }
        // Pi's cross-check: todoStateCursor < closureCursor < cursor.
        assert!(done_cursor < signal["closure"]["closureCursor"].as_u64().unwrap());
        assert!(
            signal["closure"]["closureCursor"].as_u64().unwrap()
                < signal["cursor"].as_u64().unwrap()
        );
        assert_eq!(signal["closure"]["coverageQualified"], true);
        assert_eq!(
            signal
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["closure", "cursor", "recovery", "route", "todo", "type"]
        );
        assert!(signal["parentTodo"].is_null());
        assert_eq!(signal["recovery"]["available"], true);
        let signal_cursor = signal["cursor"].as_u64().unwrap();
        assert!(signals(&mut fx, signal_cursor).is_empty());
        // The barrier carries the only AllPathsTrusted mint.
        assert!(closure_journal(&fx).committed().any(|record| matches!(
            record,
            crate::child_report_closure::ClosureRecord::ClosureBarrier {
                qualification: crate::child_report::CoverageQualification::AllPathsTrusted,
                ..
            }
        )));
        // Repeating the done ACK is idempotent: no second closure or signal.
        call(&mut fx, false, "todo_state", todo(2, true));
        assert_eq!(signals(&mut fx, 0).len(), 1);
        // Egress is frozen until the parent requests recovery.
        let frozen = call(&mut fx, false, "report_path_attempt", path_attempt(2, "a"));
        assert_eq!(frozen["error"]["code"], "egress_frozen", "{frozen}");
        let mut mismatched = recovery_request(&signal);
        mismatched["stateDigest"] = json!(state_digest(1));
        assert_eq!(
            call(&mut fx, true, "report_recovery_request", mismatched)["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_request",
                recovery_request(&signal)
            )["error"]["code"],
            "grant_revoked",
            "only the signal's exact parent may request recovery"
        );
        let requested = call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(&signal),
        );
        assert_eq!(
            requested["result"]["type"], "report_recovery_request",
            "{requested}"
        );
        assert_eq!(requested["result"]["signalCursor"], signal_cursor);
        let recovery_cursor = requested["result"]["recoveryCursor"].as_u64().unwrap();
        let repeat = call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(&signal),
        );
        assert_eq!(repeat["result"]["recoveryCursor"], recovery_cursor);
        assert!(recovery_cursor > signal_cursor);
        let waited = call(
            &mut fx,
            false,
            "report_recovery_wait",
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":0}),
        );
        assert_eq!(waited["result"]["type"], "report_recovery_wait");
        assert_eq!(
            waited["result"]["requests"],
            json!([{"signalCursor":signal_cursor,"recoveryCursor":recovery_cursor,
                    "routeEpoch":signal["route"]["routeEpoch"],"localRoot":"child-root",
                    "localRevision":2,"stateDigest":state_digest(2)}])
        );
        // Exactly one delivery through the existing bound path.
        assert_eq!(
            call(&mut fx, false, "report_path_attempt", path_attempt(2, "a"))["result"]["type"],
            "report_path_attempt"
        );
        assert_eq!(
            call(&mut fx, false, "report_path_attempt", path_attempt(2, "bb"))["error"]["code"],
            "egress_frozen"
        );
        assert_eq!(
            call(&mut fx, false, "report_prepared", preparation(2, "a"))["result"]["type"],
            "report_prepared"
        );
        assert_eq!(
            call(&mut fx, false, "report_prepared", preparation(2, "bb"))["error"]["code"],
            "egress_frozen"
        );
        let delivered = call(&mut fx, false, "report_submit_parent", submission("a"));
        assert!(delivered["ok"].as_bool().unwrap(), "{delivered}");
        assert_eq!(signals(&mut fx, 0).len(), 1, "recovery adds no signal");
        // A fresh child stream now advertises recovery. Move the parent Pi
        // off this test process so the child is the only candidate.
        let parent_key = fx.parent_key.clone();
        install_trusted_test_pi(
            &mut fx.app,
            &fx.directory,
            &parent_key,
            std::process::id() + 1,
        );
        let mut renewed = connect(&fx.listener);
        let descriptor = bootstrap(&mut fx.listener, &mut fx.app, &mut renewed);
        assert_eq!(
            descriptor["result"]["parentReport"]["recoveryWaitMethod"], "report_recovery_wait",
            "{descriptor}"
        );
        finish(fx);
    }

    #[test]
    fn covered_closure_never_reports_missing_on_any_uncertain_path() {
        struct Case {
            label: &'static str,
            verifier: Option<u64>,
            expected: Option<(&'static str, &'static str)>,
        }
        let cases = [
            Case {
                label: "unverified",
                verifier: None,
                expected: Some(("report_unknown", "coverage_unqualified")),
            },
            Case {
                label: "late_enforcement",
                verifier: Some(1),
                expected: Some(("report_unknown", "coverage_unqualified")),
            },
            Case {
                label: "path_attempt",
                verifier: Some(0),
                expected: Some(("report_unknown", "path_attempt_uncertain")),
            },
            Case {
                label: "prepared",
                verifier: Some(0),
                expected: Some(("report_unknown", "prepared_not_admitted")),
            },
            Case {
                label: "in_flight",
                verifier: Some(0),
                expected: Some(("report_unknown", "in_flight")),
            },
            Case {
                label: "bypass",
                verifier: Some(0),
                expected: Some(("report_unknown", "coverage_unqualified")),
            },
            Case {
                label: "restart",
                verifier: Some(0),
                expected: Some(("report_unknown", "coverage_unqualified")),
            },
            Case {
                label: "process_gone",
                verifier: Some(0),
                expected: Some(("report_unknown", "child_process_gone")),
            },
            Case {
                label: "admitted",
                verifier: Some(0),
                expected: None,
            },
            Case {
                label: "readback_failure",
                verifier: Some(0),
                expected: None,
            },
        ];
        for case in cases {
            let mut fx = covered_fixture(true, true, case.verifier);
            assert_eq!(
                call(&mut fx, false, "todo_state", todo(1, false))["result"]["type"],
                "todo_state",
                "{}",
                case.label
            );
            let store = crate::mailbox::MailboxStore::existing(&fx.directory);
            match case.label {
                "path_attempt" => {
                    let marked = call(&mut fx, false, "report_path_attempt", path_attempt(1, "a"));
                    assert_eq!(marked["result"]["type"], "report_path_attempt", "{marked}");
                }
                "prepared" => {
                    call(&mut fx, false, "report_prepared", preparation(1, "a"));
                }
                "in_flight" => {
                    call(&mut fx, false, "report_prepared", preparation(1, "a"));
                    let prepared = store
                        .load()
                        .unwrap()
                        .child_report_events
                        .into_iter()
                        .find_map(|event| match event {
                            crate::child_report::ChildReportEvent::Prepared { preparation } => {
                                Some(preparation)
                            }
                            _ => None,
                        })
                        .unwrap();
                    store
                        .append_child_report_event(
                            crate::child_report::ChildReportEvent::PreparedAttempt {
                                preparation: prepared,
                            },
                        )
                        .unwrap();
                }
                "bypass" => {
                    store
                        .append_child_report_event(crate::child_report::ChildReportEvent::Bypass {
                            route: current_route(&fx),
                            path: crate::child_report::ReportBypassPath::HandoffPty,
                            message_id: "pty".into(),
                        })
                        .unwrap();
                }
                "restart" => {
                    fx.app.mailbox_bootstrap_boot_nonce = Some("f".repeat(64));
                }
                "process_gone" => {
                    let real = crate::platform::process_birth_identity(std::process::id()).unwrap();
                    let replacement = crate::platform::ProcessBirthIdentity {
                        pid: std::process::id() + 7,
                        start_ticks: real.start_ticks,
                    };
                    fx.app
                        .mailbox_bootstrap_test_process_births
                        .insert(replacement.pid, replacement);
                    fx.app
                        .managed_pi_launches
                        .get_mut(&fx.child_terminal)
                        .unwrap()
                        .process = Some(replacement);
                    let mut job =
                        fx.app.mailbox_bootstrap_test_foreground_jobs[&fx.child_terminal].clone();
                    job.process_group_id = replacement.pid;
                    job.processes[0].pid = replacement.pid;
                    fx.app.install_mailbox_bootstrap_test_foreground_job(
                        fx.child_terminal.clone(),
                        job,
                    );
                }
                "admitted" => {
                    call(&mut fx, false, "report_prepared", preparation(1, "a"));
                    let sent = call(&mut fx, false, "report_submit_parent", submission("a"));
                    assert!(sent["ok"].as_bool().unwrap(), "{sent}");
                }
                "readback_failure" => crate::child_report_closure::test_fail_closure_readback(true),
                _ => {}
            }
            let done = call(&mut fx, false, "todo_state", todo(2, true));
            crate::child_report_closure::test_fail_closure_readback(false);
            assert_eq!(
                done["result"]["type"], "todo_state",
                "{}: {done}",
                case.label
            );
            let page = signals(&mut fx, 0);
            match case.expected {
                Some((signal_type, reason)) => {
                    assert_eq!(page.len(), 1, "{}: {page:?}", case.label);
                    assert_eq!(page[0]["type"], signal_type, "{}", case.label);
                    assert_eq!(page[0]["reason"], reason, "{}", case.label);
                    assert_eq!(page[0]["recovery"]["available"], false, "{}", case.label);
                    let denied = call(
                        &mut fx,
                        true,
                        "report_recovery_request",
                        recovery_request(&page[0]),
                    );
                    assert!(denied["error"].is_object(), "{}: {denied}", case.label);
                }
                None => assert!(page.is_empty(), "{}: {page:?}", case.label),
            }
            match case.label {
                "path_attempt" => assert_eq!(page[0]["closure"]["pathAttemptCount"], 1),
                "prepared" | "in_flight" => {
                    assert_eq!(page[0]["closure"]["preparedCount"], 1)
                }
                "restart" | "process_gone" => {
                    assert!(closure_journal(&fx).records.iter().any(|record| matches!(
                        record,
                        crate::child_report_closure::ClosureRecord::DomainSuspended { .. }
                    )))
                }
                "admitted" => assert!(closure_journal(&fx).committed().any(|record| matches!(
                    record,
                    crate::child_report_closure::ClosureRecord::ClosureBarrier {
                        outcome: crate::child_report_closure::ClosureOutcome::Admitted,
                        ..
                    }
                ))),
                "readback_failure" => {
                    // The uncommitted freeze still restricts egress, and a
                    // repeated ACK cannot mint a late signal.
                    assert_eq!(
                        call(&mut fx, false, "report_path_attempt", path_attempt(2, "a"))["error"]
                            ["code"],
                        "egress_frozen"
                    );
                    call(&mut fx, false, "todo_state", todo(2, true));
                    assert!(signals(&mut fx, 0).is_empty());
                }
                _ => {}
            }
            if case.label != "readback_failure" && case.label != "admitted" {
                assert!(
                    !closure_journal(&fx).committed().any(|record| matches!(
                        record,
                        crate::child_report_closure::ClosureRecord::ClosureBarrier {
                            qualification:
                                crate::child_report::CoverageQualification::AllPathsTrusted,
                            ..
                        }
                    )),
                    "{}",
                    case.label
                );
            }
            finish(fx);
        }
    }

    #[test]
    fn parent_todo_binding_decline_and_reopen_stay_exact() {
        let mut fx = covered_fixture(true, true, Some(0));
        call(&mut fx, false, "todo_state", todo(1, false));
        let route = current_route(&fx);
        let child_session = serde_json::to_value(&route.child_session).unwrap();
        let bind = |pane: &str, session: &Value, task: u64| {
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"childPaneId":pane,
                   "childSession":session,"todoDelegationId":"AbCdEfGhIjKlMnOpQrSt_-",
                   "parentTaskId":task})
        };
        let pane = route.child_pane_id.clone();
        assert_eq!(
            call(
                &mut fx,
                false,
                "todo_delegation_bind",
                bind(&pane, &child_session, 7)
            )["error"]["code"],
            "invalid_request",
            "a child is not the parent of any delegation"
        );
        let mut other_session = child_session.clone();
        other_session["value"] = json!("/elsewhere.jsonl");
        assert_eq!(
            call(
                &mut fx,
                true,
                "todo_delegation_bind",
                bind(&pane, &other_session, 7)
            )["error"]["code"],
            "invalid_request",
            "no child delegation with that exact session"
        );
        assert_eq!(
            call(
                &mut fx,
                true,
                "todo_delegation_bind",
                bind("w0:p404", &child_session, 7)
            )["error"]["code"],
            "invalid_request"
        );
        let mut short_id = bind(&pane, &child_session, 7);
        short_id["todoDelegationId"] = json!("d1");
        assert_eq!(
            call(&mut fx, true, "todo_delegation_bind", short_id)["error"]["code"],
            "invalid_request"
        );
        let bound = call(
            &mut fx,
            true,
            "todo_delegation_bind",
            bind(&pane, &child_session, 7),
        );
        assert_eq!(bound["result"]["type"], "todo_delegation_bind", "{bound}");
        let binding_cursor = bound["result"]["cursor"].as_u64().unwrap();
        let again = call(
            &mut fx,
            true,
            "todo_delegation_bind",
            bind(&pane, &child_session, 7),
        );
        assert_eq!(again["result"]["cursor"], binding_cursor);
        assert_eq!(
            call(
                &mut fx,
                true,
                "todo_delegation_bind",
                bind(&pane, &child_session, 8)
            )["error"]["code"],
            "invalid_request",
            "conflicting rebind"
        );
        call(&mut fx, false, "todo_state", todo(2, true));
        let signal = signals(&mut fx, 0)[0].clone();
        assert_eq!(signal["type"], "missing_after_done", "{signal}");
        assert_eq!(
            signal["parentTodo"],
            json!({"delegationId":"AbCdEfGhIjKlMnOpQrSt_-","parentTaskId":7})
        );
        let signal_cursor = signal["cursor"].as_u64().unwrap();
        let decline = |recovery_cursor: u64, reason: &str| {
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"signalCursor":signal_cursor,
                   "recoveryCursor":recovery_cursor,"reason":reason})
        };
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_decline",
                decline(signal_cursor + 1, "no_exported_report")
            )["error"]["code"],
            "invalid_request",
            "no request, nothing to decline"
        );
        let requested = call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(&signal),
        );
        let recovery_cursor = requested["result"]["recoveryCursor"].as_u64().unwrap();
        assert_eq!(
            call(
                &mut fx,
                true,
                "report_recovery_decline",
                decline(recovery_cursor, "no_exported_report")
            )["error"]["code"],
            "grant_missing",
            "the parent is not the covered child"
        );
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_decline",
                decline(recovery_cursor + 1, "no_exported_report")
            )["error"]["code"],
            "invalid_request",
            "wrong recovery cursor"
        );
        let declined = call(
            &mut fx,
            false,
            "report_recovery_decline",
            decline(recovery_cursor, "no_exported_report"),
        );
        assert_eq!(
            declined["result"]["type"], "report_recovery_decline",
            "{declined}"
        );
        let decline_cursor = declined["result"]["cursor"].as_u64().unwrap();
        assert!(decline_cursor > recovery_cursor);
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_decline",
                decline(recovery_cursor, "no_exported_report")
            )["result"]["cursor"],
            decline_cursor
        );
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_decline",
                decline(recovery_cursor, "already_admitted")
            )["error"]["code"],
            "invalid_request"
        );
        assert_eq!(
            call(&mut fx, false, "report_path_attempt", path_attempt(2, "a"))["error"]["code"],
            "egress_frozen",
            "a declined recovery keeps egress frozen"
        );
        assert_eq!(
            call(
                &mut fx,
                false,
                "report_recovery_wake_request",
                json!({"protocol":crate::mailbox_v1::PROTOCOL,"signalCursor":signal_cursor})
            )["error"]["code"],
            "invalid_request",
            "no wake after a decline"
        );
        let page = signals(&mut fx, 0);
        assert_eq!(page.len(), 2, "{page:?}");
        assert_eq!(page[0]["recovery"]["available"], false);
        assert_eq!(page[1]["type"], "report_unknown");
        assert_eq!(page[1]["reason"], "recovery_declined_no_exported_report");
        assert_eq!(page[1]["parentTodo"], page[0]["parentTodo"]);
        assert!(page[1]["cursor"].as_u64().unwrap() > decline_cursor);
        let waited = call(
            &mut fx,
            false,
            "report_recovery_wait",
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":0}),
        );
        assert_eq!(
            waited["result"]["requests"],
            json!([]),
            "declined requests are settled"
        );
        // Reopened work makes the old signal non-current and unfreezes the
        // bound path for the new revision.
        call(&mut fx, false, "todo_state", todo(3, false));
        assert_eq!(
            call(&mut fx, false, "report_path_attempt", path_attempt(3, "b"))["result"]["type"],
            "report_path_attempt"
        );
        finish(fx);
    }

    #[test]
    fn recovery_is_refused_and_domain_suspended_after_the_child_process_changes() {
        let mut fx = covered_fixture(true, true, Some(0));
        call(&mut fx, false, "todo_state", todo(1, true));
        let signal = signals(&mut fx, 0)[0].clone();
        assert_eq!(signal["type"], "missing_after_done");
        let real = crate::platform::process_birth_identity(std::process::id()).unwrap();
        let replacement = crate::platform::ProcessBirthIdentity {
            pid: std::process::id() + 9,
            start_ticks: real.start_ticks,
        };
        fx.app
            .mailbox_bootstrap_test_process_births
            .insert(replacement.pid, replacement);
        fx.app
            .managed_pi_launches
            .get_mut(&fx.child_terminal)
            .unwrap()
            .process = Some(replacement);
        let mut job = fx.app.mailbox_bootstrap_test_foreground_jobs[&fx.child_terminal].clone();
        job.process_group_id = replacement.pid;
        job.processes[0].pid = replacement.pid;
        fx.app
            .install_mailbox_bootstrap_test_foreground_job(fx.child_terminal.clone(), job);
        assert_eq!(signals(&mut fx, 0)[0]["recovery"]["available"], false);
        assert_eq!(
            call(
                &mut fx,
                true,
                "report_recovery_request",
                recovery_request(&signal)
            )["error"]["code"],
            "grant_revoked"
        );
        assert!(closure_journal(&fx).records.iter().any(|record| matches!(
            record,
            crate::child_report_closure::ClosureRecord::DomainSuspended {
                reason: crate::child_report_closure::UnknownReason::ChildProcessGone,
                ..
            }
        )));
        finish(fx);
    }

    #[test]
    fn covered_child_generic_paths_are_denied_but_uncovered_beta_is_unchanged() {
        for covered in [true, false] {
            let mut fx = covered_fixture(true, covered, Some(0));
            let mut submit = submission("self");
            submit["stableId"] = json!("self-report");
            let result = call(&mut fx, false, "report_submit", submit);
            assert_eq!(result["ok"], !covered, "covered={covered}: {result}");
            let target = json!({"target": fx.parent_key.clone()});
            let provision = call(&mut fx, false, "mailbox.provision_recipient", target);
            if covered {
                assert_eq!(provision["error"]["code"], "grant_revoked", "{provision}");
            }
            finish(fx);
        }
    }

    #[test]
    fn feature_off_keeps_gen1_descriptor_and_methods() {
        let mut fx = covered_fixture(false, true, Some(0));
        assert!(fx.parent_descriptor["result"]["parentSignals"].is_null());
        call(&mut fx, false, "todo_state", todo(1, true));
        assert!(!fx
            .directory
            .join(crate::child_report_closure::CLOSURE_STREAM_FILE)
            .exists());
        for method in [
            "child_report_signals",
            "report_recovery_wait",
            "report_recovery_request",
            "report_recovery_decline",
            "child_delegation_bind_todo",
        ] {
            assert_eq!(
                call(
                    &mut fx,
                    true,
                    method,
                    json!({"protocol":crate::mailbox_v1::PROTOCOL})
                )["error"]["code"],
                "invalid_request",
                "{method}"
            );
        }
        assert_eq!(
            call(&mut fx, false, "report_path_attempt", path_attempt(1, "a"))["result"]["type"],
            "report_path_attempt"
        );
        finish(fx);
    }

    fn read_frame(stream: &mut UnixStream) -> Value {
        let mut response = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            stream.read_exact(&mut byte).expect("read response");
            if byte[0] == b'\n' {
                break;
            }
            response.push(byte[0]);
        }
        serde_json::from_slice(&response).unwrap()
    }

    #[test]
    fn signal_long_poll_parks_without_blocking_and_is_bounded() {
        let mut fx = covered_fixture(true, true, Some(0));
        let after = closure_journal(&fx).next_cursor() - 1;
        assert_eq!(
            call(
                &mut fx,
                true,
                "child_report_signals",
                json!({"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":after,
                        "waitMs":crate::child_report_closure::MAX_WAIT_MS + 1})
            )["error"]["code"],
            "invalid_request"
        );
        let frame = json!({"method":"child_report_signals","requestId":"poll",
            "bindingGeneration":fx.parent_binding,
            "params":{"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":after,"waitMs":20_000}});
        fx.parent
            .write_all(&serde_json::to_vec(&frame).unwrap())
            .unwrap();
        fx.parent.write_all(b"\n").unwrap();
        fx.listener.poll(&mut fx.app).unwrap();
        fx.parent.set_nonblocking(true).unwrap();
        let mut probe = [0_u8; 1];
        assert_eq!(
            fx.parent.read(&mut probe).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "an empty long-poll is parked, not answered"
        );
        fx.parent.set_nonblocking(false).unwrap();
        // The event loop keeps serving other streams while the parent waits.
        call(&mut fx, false, "todo_state", todo(1, true));
        fx.listener.poll(&mut fx.app).unwrap();
        let woken = read_frame(&mut fx.parent);
        assert_eq!(woken["requestId"], "poll");
        assert_eq!(
            woken["result"]["signals"][0]["type"], "missing_after_done",
            "{woken}"
        );
        let next = woken["result"]["throughCursor"].as_u64().unwrap();
        let frame = json!({"method":"child_report_signals","requestId":"expire",
            "bindingGeneration":fx.parent_binding,
            "params":{"protocol":crate::mailbox_v1::PROTOCOL,"afterCursor":next,"waitMs":1}});
        fx.parent
            .write_all(&serde_json::to_vec(&frame).unwrap())
            .unwrap();
        fx.parent.write_all(b"\n").unwrap();
        fx.listener.poll(&mut fx.app).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        fx.listener.poll(&mut fx.app).unwrap();
        let expired = read_frame(&mut fx.parent);
        assert_eq!(expired["requestId"], "expire");
        assert_eq!(expired["result"]["signals"], json!([]));
        finish(fx);
    }

    fn wake(fx: &mut Covered, as_parent: bool, signal_cursor: u64) -> Value {
        call(
            fx,
            as_parent,
            "report_recovery_wake_request",
            json!({"protocol":crate::mailbox_v1::PROTOCOL,"signalCursor":signal_cursor}),
        )
    }

    fn wake_heads(fx: &Covered) -> Vec<crate::mailbox::MailboxHead> {
        crate::mailbox::MailboxStore::existing(&fx.directory)
            .load()
            .unwrap()
            .heads
            .into_values()
            .filter(|head| head.kind == crate::child_report_closure::RECOVERY_WAKE_KIND)
            .collect()
    }

    #[test]
    fn recovery_wake_is_issued_once_per_signal_and_only_to_the_covered_child() {
        let mut fx = covered_fixture(true, true, Some(0));
        call(&mut fx, false, "todo_state", todo(1, true));
        let signal = signals(&mut fx, 0)[0].clone();
        let signal_cursor = signal["cursor"].as_u64().unwrap();
        assert_eq!(
            wake(&mut fx, false, signal_cursor)["error"]["code"],
            "invalid_request",
            "no wake without an open recovery request"
        );
        call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(&signal),
        );
        assert_eq!(
            wake(&mut fx, true, signal_cursor)["error"]["code"],
            "grant_missing",
            "the parent is not the covered child"
        );
        let binding = fx.child_binding.clone();
        fx.child_binding = "forged".into();
        assert_eq!(
            wake(&mut fx, false, signal_cursor)["error"]["code"],
            "grant_revoked",
            "a stale or forged child binding is refused"
        );
        fx.child_binding = binding;
        assert!(wake_heads(&fx).is_empty());
        let issued = wake(&mut fx, false, signal_cursor);
        assert_eq!(
            issued["result"]["type"], "report_recovery_wake_request",
            "{issued}"
        );
        assert_eq!(issued["result"]["signalCursor"], signal_cursor);
        let wake_cursor = issued["result"]["wakeCursor"].as_u64().unwrap();
        assert!(wake_cursor > signal_cursor);
        let heads = wake_heads(&fx);
        assert_eq!(heads.len(), 1);
        let head = &heads[0];
        assert_eq!(head.subject, "System recovery request");
        assert_eq!(head.revision, 1);
        assert_eq!(head.priority, "normal");
        assert_eq!(
            head.body,
            format!("{{\"signalCursor\":{signal_cursor},\"wakeCursor\":{wake_cursor}}}")
        );
        assert_eq!(
            head.recipient.recipient_id,
            current_route(&fx).child_terminal_id
        );
        // The server-minted head is never editable.
        assert!(crate::mailbox::MailboxStore::existing(&fx.directory)
            .edit_unclaimed_head(crate::mailbox::MailboxHeadEdit {
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                subject: "edited".into(),
                body: "edited".into(),
            })
            .is_err());
        let repeat = wake(&mut fx, false, signal_cursor);
        assert_eq!(repeat["result"]["wakeCursor"], wake_cursor);
        assert_eq!(wake_heads(&fx).len(), 1, "a repeat delivers nothing new");
        // The woken child's report uses the existing one-delivery unfreeze.
        assert_eq!(
            call(&mut fx, false, "report_prepared", preparation(1, "a"))["result"]["type"],
            "report_prepared"
        );
        let delivered = call(&mut fx, false, "report_submit_parent", submission("a"));
        assert!(delivered["ok"].as_bool().unwrap(), "{delivered}");
        // Across a restart the durable wake record and head stable ID still
        // allow exactly one head.
        fx.app.mailbox_bootstrap_boot_nonce = Some("f".repeat(64));
        assert_eq!(
            wake(&mut fx, false, signal_cursor)["result"]["wakeCursor"],
            wake_cursor
        );
        let restarted = crate::mailbox::MailboxStore::existing(&fx.directory);
        assert_eq!(
            restarted.append_server_recovery_wake(head.clone()),
            Ok(false)
        );
        assert_eq!(wake_heads(&fx).len(), 1);
        assert_eq!(
            closure_journal(&fx)
                .records
                .iter()
                .filter(|record| matches!(
                    record,
                    crate::child_report_closure::ClosureRecord::RecoveryWakeIssued { .. }
                ))
                .count(),
            1
        );
        finish(fx);
    }

    #[test]
    fn unissuable_recovery_wake_settles_unknown_through_the_decline_path() {
        let mut fx = covered_fixture(true, true, Some(0));
        call(&mut fx, false, "todo_state", todo(1, true));
        let signal = signals(&mut fx, 0)[0].clone();
        let signal_cursor = signal["cursor"].as_u64().unwrap();
        call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(&signal),
        );
        // Occupy the wake's stable ID so the server head cannot be minted.
        let blocker = crate::mailbox::MailboxHead {
            stable_id: crate::child_report_closure::recovery_wake_stable_id(signal_cursor),
            revision: 1,
            digest: "a".repeat(64),
            delivery_digest: "c".repeat(64),
            recipient: crate::mailbox::RecipientKey {
                recipient_id: fx.parent_key.clone(),
                generation: "1".into(),
            },
            subject: "s".into(),
            body: "b".into(),
            recipient_generation: "1".into(),
            sender: "x".into(),
            target: fx.parent_key.clone(),
            grant_id: "g".into(),
            message_id: "m".into(),
            kind: "info".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 0,
            accepted_at: 1,
        };
        crate::mailbox::MailboxStore::existing(&fx.directory)
            .append_offline_head(blocker)
            .unwrap();
        assert_eq!(
            wake(&mut fx, false, signal_cursor)["error"]["code"],
            "grant_revoked"
        );
        assert!(wake_heads(&fx).is_empty());
        let page = signals(&mut fx, 0);
        assert_eq!(page.len(), 2, "{page:?}");
        assert_eq!(page[1]["type"], "report_unknown");
        assert_eq!(page[1]["reason"], "recovery_declined_no_exported_report");
        // Settled: a repeat cannot issue a late wake.
        assert!(wake(&mut fx, false, signal_cursor)["error"].is_object());
        assert!(wake_heads(&fx).is_empty());
        finish(fx);
    }

    #[test]
    fn peers_and_generic_api_cannot_mint_wakes_or_call_accepted_stream_methods() {
        let mut fx = covered_fixture(true, false, Some(0));
        let mut forged = submission("wake");
        forged["kind"] = json!("recovery_wake");
        forged["subject"] = json!("System recovery request");
        let peer = call(&mut fx, false, "report_submit", forged.clone());
        assert_eq!(peer["ok"], false, "{peer}");
        let mut generic = forged.clone();
        generic["stableId"] = json!("generic-wake");
        let child_key = current_route_key(&mut fx);
        let response = fx.app.handle_api_request(crate::api::schema::Request {
            id: "forged-wake".into(),
            method: crate::api::schema::Method::MailboxOfflineSubmit(
                crate::api::schema::MailboxOfflineSubmitParams {
                    caller: child_key.clone(),
                    grant_id: format!("offline:{child_key}:1"),
                    recipient: crate::mailbox::RecipientKey {
                        recipient_id: child_key.clone(),
                        generation: "1".into(),
                    },
                    submit: serde_json::from_value(generic).unwrap(),
                },
            ),
        });
        assert!(response.contains("\"error\""), "{response}");
        assert!(wake_heads(&fx).is_empty());
        // Only the accepted Pi stream knows these methods; the generic API
        // request decoder rejects each of them before any dispatch.
        for method in [
            "todo_state",
            "report_path_attempt",
            "report_prepared",
            "report_submit_parent",
            "todo_delegation_bind",
            "child_report_signals",
            "report_recovery_request",
            "report_recovery_wait",
            "report_recovery_decline",
            "report_recovery_wake_request",
        ] {
            let line = json!({"id":"forged","method":method,"params":{
                "protocol":crate::mailbox_v1::PROTOCOL,"signalCursor":1,"afterCursor":0,
                "localRoot":"child-root","localRevision":1,"stateDigest":state_digest(1),
                "state":"done"}})
            .to_string();
            assert!(
                serde_json::from_str::<crate::api::schema::Request>(&line).is_err(),
                "{method} must not decode as a generic API request"
            );
        }
        finish(fx);
    }

    fn current_route_key(fx: &mut Covered) -> String {
        fx.child_terminal.to_string()
    }

    #[test]
    fn closure_cursors_are_closure_ordinals_even_when_the_mailbox_is_far_longer() {
        let mut fx = covered_fixture(true, true, Some(0));
        let store = crate::mailbox::MailboxStore::existing(&fx.directory);
        for index in 0..300 {
            store
                .provision_grant(crate::mailbox::MailboxGrant {
                    grant_id: format!("unrelated-{index}"),
                    sender: crate::mailbox::RecipientKey {
                        recipient_id: "unrelated-sender".into(),
                        generation: "1".into(),
                    },
                    recipient: crate::mailbox::RecipientKey {
                        recipient_id: "unrelated-recipient".into(),
                        generation: "1".into(),
                    },
                })
                .unwrap();
        }
        let done = call(&mut fx, false, "todo_state", todo(1, true));
        let todo_cursor = done["result"]["cursor"].as_u64().unwrap();
        let page = signals(&mut fx, 0);
        assert_eq!(page.len(), 1, "{page:?}");
        let signal = &page[0];
        assert_eq!(signal["type"], "missing_after_done", "{signal}");
        assert_eq!(signal["todo"]["todoStateCursor"], todo_cursor);
        let signal_cursor = signal["cursor"].as_u64().unwrap();
        let closure_cursor = signal["closure"]["closureCursor"].as_u64().unwrap();
        assert!(
            todo_cursor > 300 && signal_cursor < 20,
            "{todo_cursor} vs {signal_cursor}"
        );
        assert!(closure_cursor < signal_cursor);
        // Each closure cursor is its record's line ordinal in the closure file.
        let lines = std::fs::read_to_string(
            fx.directory
                .join(crate::child_report_closure::CLOSURE_STREAM_FILE),
        )
        .unwrap();
        for (ordinal, line) in lines.lines().enumerate() {
            let record: Value = serde_json::from_str(line).unwrap();
            assert_eq!(record["cursor"], ordinal as u64 + 1);
        }
        let requested = call(
            &mut fx,
            true,
            "report_recovery_request",
            recovery_request(signal),
        );
        assert!(requested["result"]["recoveryCursor"].as_u64().unwrap() > signal_cursor);
        let woken = wake(&mut fx, false, signal_cursor);
        assert!(
            woken["result"]["wakeCursor"].as_u64().unwrap() > signal_cursor,
            "{woken}"
        );
        finish(fx);
    }

    #[test]
    fn closed_streams_release_their_bindings() {
        let mut fx = covered_fixture(true, true, Some(0));
        let issued = fx.app.mailbox_bootstrap_bindings.len();
        assert!(fx
            .app
            .mailbox_bootstrap_bindings
            .contains_key(&fx.child_binding));
        let mut probe = connect(&fx.listener);
        let descriptor = bootstrap(&mut fx.listener, &mut fx.app, &mut probe);
        let probe_binding = descriptor["result"]["bindingGeneration"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(fx.app.mailbox_bootstrap_bindings.len(), issued + 1);
        drop(probe);
        fx.listener.poll(&mut fx.app).unwrap();
        assert!(!fx
            .app
            .mailbox_bootstrap_bindings
            .contains_key(&probe_binding));
        assert_eq!(fx.app.mailbox_bootstrap_bindings.len(), issued);
        // Live streams keep working.
        assert_eq!(
            call(&mut fx, false, "todo_state", todo(1, false))["result"]["type"],
            "todo_state"
        );
        finish(fx);
    }
}
