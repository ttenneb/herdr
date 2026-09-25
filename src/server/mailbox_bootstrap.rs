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
    pub report_submit: ReportSubmitAdvertisement,
    /// Offered only when the server can validate an exact active delegation
    /// parent. Admission here is a durable mailbox receipt, not Pi Gate
    /// admission or the parent's acceptance of the report.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_report: Option<ParentReportAdvertisement>,
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
    pub protocol: &'static str,
    pub recipient: crate::mailbox::RecipientKey,
    pub grant_id: String,
}

impl MailboxBootstrapDescriptor {
    fn from_session(session: &MailboxBootstrapSession, endpoint: &Path) -> Self {
        Self {
            protocol_version: MAILBOX_BOOTSTRAP_PROTOCOL_VERSION,
            report_submit: ReportSubmitAdvertisement {
                method: "report_submit",
                protocol: crate::mailbox_v1::PROTOCOL,
            },
            parent_report: session
                .parent_report
                .as_ref()
                .map(|route| ParentReportAdvertisement {
                    method: "report_submit_parent",
                    protocol: crate::mailbox_v1::PROTOCOL,
                    recipient: route.recipient.clone(),
                    grant_id: route.grant_id.clone(),
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
                    None => failure(None, MailboxBootstrapError::InvalidRequest),
                };
                if write_response(&mut connection.stream, &response).is_err() {
                    closed.push(*id);
                    break;
                }
            }
        }
        for id in closed {
            self.accepted.remove(&id);
        }
        Ok(())
    }

    fn handle_request(
        app: &mut App,
        connection: &mut AcceptedMailboxConnection,
        request: BootstrapRequest,
    ) -> String {
        let request_id = request.request_id;
        if request.method == "bootstrap" {
            if connection.session.is_some() {
                return failure(request_id, MailboxBootstrapError::InvalidRequest);
            }
            let session = match app.accept_mailbox_bootstrap_stream(connection.stream.as_raw_fd()) {
                Ok(session) => session,
                Err(error) => return failure(request_id, error),
            };
            let descriptor = MailboxBootstrapDescriptor::from_session(
                &session,
                &mailbox_bootstrap_socket_path(),
            );
            connection.session = Some(session);
            return serde_json::to_string(&BootstrapSuccess {
                ok: true,
                request_id,
                result: descriptor,
            })
            .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing));
        }

        let Some(session) = connection.session.as_ref() else {
            return failure(request_id, MailboxBootstrapError::GrantMissing);
        };
        if request.binding_generation.as_deref() != Some(&session.binding_generation) {
            return failure(request_id, MailboxBootstrapError::GrantRevoked);
        }
        let result = app.dispatch_mailbox_bootstrap(session, &request.method, request.params);
        match result {
            Ok(result) => serde_json::to_string(&BootstrapSuccess {
                ok: true,
                request_id,
                result,
            })
            .unwrap_or_else(|_| failure(None, MailboxBootstrapError::GrantMissing)),
            Err(error) => failure(request_id, error),
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
        std::env::temp_dir().join(format!(
            "herdr-mailbox-bootstrap-{}-{}",
            std::process::id(),
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
        (app, directory, sender)
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
        (sender, "recipient".into())
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
        app.state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
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
            descriptor["result"]["parentReport"]["recipient"]["recipientId"],
            parent
        );
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
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
        assert_eq!(
            crate::mailbox_v1::snapshot(
                &recovered,
                &crate::mailbox::RecipientKey {
                    recipient_id: parent.clone(),
                    generation: "1".into()
                }
            )
            .heads
            .len(),
            1
        );
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
        app.state
            .delegations
            .create(Some(child_pane), Some(parent_id), None)
            .unwrap();
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
        assert_eq!(
            descriptor["result"]["parentReport"]["recipient"]["recipientId"],
            parent
        );
        let binding = descriptor["result"]["bindingGeneration"].as_str().unwrap();
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
