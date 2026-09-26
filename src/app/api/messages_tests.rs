//! Editable Messages: ordinary sends routed into the recipient's queue, the
//! pending-exists choice, the recipient session pin with Retry/Drop, and the
//! recipient-only binding for Pis without a trusted launch.

use std::os::fd::AsRawFd;

use bytes::Bytes;
use serde_json::json;

use crate::{
    api::schema::{
        AgentPromptParams, CanonicalHerdrIdentity, HandoffKind, HandoffSendParams,
        HandoffTransportOutcome, HerdrHandoff, MessageSendOptions, MessageTransport,
        ResponseResult, SuccessResponse,
    },
    app::{
        messages::{OutgoingMessage, SendRefusal, SendRoute, SenderAttribution},
        App, MailboxBootstrapSession, Mode,
    },
    config::Config,
    detect::{Agent, AgentState},
    workspace::Workspace,
};

struct Fixture {
    app: App,
    panes: Vec<crate::layout::PaneId>,
    terminals: Vec<String>,
    rx: Vec<tokio::sync::mpsc::Receiver<Bytes>>,
    directory: std::path::PathBuf,
    _peer: Option<(
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    )>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn unique_dir() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "herdr-messages-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Two reported (untrusted) Pi panes: 0 is the sender, 1 the recipient.
fn fixture() -> Fixture {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![
        Workspace::test_new("sender"),
        Workspace::test_new("recipient"),
    ];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    app.state.selected = 0;
    app.state.mode = Mode::Terminal;
    let directory = unique_dir();
    std::fs::create_dir_all(&directory).unwrap();
    app.sender_authority_dir = directory.clone();
    let mut panes = Vec::new();
    let mut terminals = Vec::new();
    let mut receivers = Vec::new();
    for ws in 0..2 {
        let pane = app.state.workspaces[ws].tabs[0].root_pane.unwrap();
        let terminal_id = app.state.workspaces[ws].terminal_id(pane).unwrap().clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 8);
        app.state.insert_test_runtime(pane, runtime);
        report_session(&mut app, ws, pane, &format!("/sessions/s{ws}-a.jsonl"), 10);
        panes.push(pane);
        terminals.push(terminal_id.to_string());
        receivers.push(rx);
    }
    Fixture {
        app,
        panes,
        terminals,
        rx: receivers,
        directory,
        _peer: None,
    }
}

fn report_session(app: &mut App, ws: usize, pane: crate::layout::PaneId, path: &str, seq: u64) {
    let public = app.public_pane_id(ws, pane).unwrap();
    app.handle_pane_report_agent_session(
        "report".into(),
        crate::api::schema::PaneReportAgentSessionParams {
            pane_id: public,
            source: "herdr:pi".into(),
            agent: "pi".into(),
            seq: Some(seq),
            agent_session_id: None,
            agent_session_path: Some(path.into()),
            session_start_source: Some("startup".into()),
        },
    );
}

/// Makes the recipient (pane 1) look like this test process's foreground Pi
/// and attaches a recipient-only Messages stream from this process.
fn attach_recipient(fixture: &mut Fixture) -> MailboxBootstrapSession {
    let pid = std::process::id();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    fixture.app.install_mailbox_bootstrap_test_foreground_job(
        terminal_id,
        crate::platform::ForegroundJob {
            process_group_id: pid,
            processes: vec![crate::platform::ForegroundProcess {
                pid,
                name: "pi".into(),
                argv0: None,
                argv: Some(vec!["pi".into()]),
                cmdline: Some("pi".into()),
            }],
        },
    );
    let birth = crate::platform::process_birth_identity(pid).unwrap();
    fixture
        .app
        .mailbox_bootstrap_test_process_births
        .insert(pid, birth);
    fixture.app.unmanaged_pi_messages = true;
    let pair = std::os::unix::net::UnixStream::pair().unwrap();
    let session = fixture
        .app
        .accept_mailbox_bootstrap_stream(pair.0.as_raw_fd())
        .expect("recipient-only stream accepted");
    fixture._peer = Some(pair);
    session
}

fn sender(fixture: &Fixture) -> SenderAttribution {
    SenderAttribution {
        terminal: Some(fixture.terminals[0].clone()),
        label: "tpm".into(),
        session: Some("/sessions/s0-a.jsonl".into()),
    }
}

fn plain(body: &str) -> OutgoingMessage {
    OutgoingMessage {
        origin: "agent_prompt",
        subject: "Message from tpm".into(),
        body: body.into(),
        priority: "normal".into(),
        kind: "advisory".into(),
        message_id: None,
        correlation: None,
        replace_pending: false,
    }
}

fn dispatch(
    app: &mut App,
    session: &MailboxBootstrapSession,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, crate::app::MailboxBootstrapError> {
    app.dispatch_mailbox_bootstrap(session, method, params)
}

fn snapshot_heads(app: &mut App, session: &MailboxBootstrapSession) -> Vec<serde_json::Value> {
    let value = dispatch(
        app,
        session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    value["snapshot"]["heads"].as_array().unwrap().clone()
}

#[tokio::test]
async fn recipient_without_messages_keeps_the_pty_bytes_unchanged() {
    let mut fixture = fixture();
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let text = "[[pi-input-gate:ingress:v3:eyJ4IjoxfQ]]\nlegacy body";
    let response = fixture.app.handle_agent_prompt(
        "req".into(),
        AgentPromptParams {
            target,
            text: text.into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    assert_eq!(
        delivery, None,
        "no Messages stream: PTY path, no delivery field"
    );
    let typed = fixture.rx[1].recv().await.unwrap();
    assert!(String::from_utf8_lossy(&typed).contains(text));
    assert!(crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads
        .is_empty());
}

#[tokio::test]
async fn prompt_to_a_recipient_with_messages_is_queued_not_typed() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let response = fixture.app.handle_agent_prompt(
        "req".into(),
        AgentPromptParams {
            target,
            text: "please rebase".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    let delivery = delivery.expect("mailbox delivery");
    assert_eq!(delivery.path, "mailbox");
    assert!(
        fixture.rx[1].try_recv().is_err(),
        "nothing typed into the PTY"
    );
    let heads = snapshot_heads(&mut fixture.app, &session);
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0]["body"], "please rebase");
    assert_eq!(heads[0]["kind"], "advisory");
    assert_eq!(heads[0]["delivery"]["origin"], "agent_prompt");
    assert_eq!(
        heads[0]["delivery"]["recipientSession"],
        "/sessions/s1-a.jsonl"
    );
    // Forcing the PTY path still types it.
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    fixture.app.handle_agent_prompt(
        "req2".into(),
        AgentPromptParams {
            target,
            text: "typed anyway".into(),
            wait: None,
            send: MessageSendOptions {
                transport: Some(MessageTransport::Pty),
                ..Default::default()
            },
        },
    );
    assert!(String::from_utf8_lossy(&fixture.rx[1].recv().await.unwrap()).contains("typed anyway"));
}

#[tokio::test]
async fn second_send_asks_the_sender_to_edit_or_send_new() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    let first = fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("first"), &Default::default())
        .unwrap();
    let SendRoute::Mailbox(first) = first else {
        panic!("mailbox")
    };
    let first_id = first.stable_id.clone().unwrap();
    let refusal = fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("second"), &Default::default())
        .unwrap_err();
    let SendRefusal::PendingExists(pending) = &refusal else {
        panic!("pending_exists")
    };
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].stable_id, first_id);
    assert!(
        !first_id.contains('\0'),
        "stableId must be usable on a command line"
    );
    // --send-new queues a second waiting message; the list is newest first.
    let second = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("second"),
            &MessageSendOptions {
                send_new: true,
                ..Default::default()
            },
        )
        .unwrap();
    let SendRoute::Mailbox(second) = second else {
        panic!("mailbox")
    };
    let refusal = fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("third"), &Default::default())
        .unwrap_err();
    let json: serde_json::Value = serde_json::from_str(
        &crate::app::messages::pending_error_json("id".into(), &refusal).unwrap(),
    )
    .unwrap();
    assert_eq!(json["error"]["code"], "pending_exists");
    let listed = json["error"]["pending"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed[0]["stableId"],
        json!(second.stable_id.clone().unwrap())
    );
    assert_eq!(listed[1]["stableId"], json!(first_id));
    for key in [
        "stableId",
        "revision",
        "digest",
        "subject",
        "priority",
        "enqueuedAt",
        "ageSeconds",
    ] {
        assert!(listed[0].get(key).is_some(), "{key}");
    }
    // A stale expected revision or an unknown stableId is refused with the
    // current waiting list, and nothing is applied.
    for options in [
        MessageSendOptions {
            edit_pending: Some(first_id.clone()),
            expect_revision: Some(7),
            ..Default::default()
        },
        MessageSendOptions {
            edit_pending: Some("send.unknown".into()),
            ..Default::default()
        },
    ] {
        let refused = fixture
            .app
            .route_ordinary_send(&recipient, &sender, plain("nope"), &options)
            .unwrap_err();
        assert!(matches!(refused, SendRefusal::PendingChanged(ref list) if list.len() == 2));
    }
    // --edit-pending <stableId> edits exactly that (older) message.
    let edited = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("first, edited in place"),
            &MessageSendOptions {
                edit_pending: Some(first_id.clone()),
                expect_revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    let SendRoute::Mailbox(edited) = edited else {
        panic!("mailbox")
    };
    assert!(edited.edited);
    assert_eq!(edited.stable_id, Some(first_id));
    assert_eq!(edited.revision, Some(2));
    let heads = snapshot_heads(&mut fixture.app, &session);
    let bodies: Vec<_> = heads.iter().map(|head| head["body"].clone()).collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.contains(&json!("first, edited in place")));
    assert!(bodies.contains(&json!("second")));
    // F3 through the recipient view: the edited head has its exact receipt.
    let value = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    for head in value["snapshot"]["heads"].as_array().unwrap() {
        assert!(value["snapshot"]["receipts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|receipt| receipt["stableId"] == head["stableId"]
                && receipt["revision"] == head["revision"]
                && receipt["digest"] == head["digest"]
                && receipt["deliveryDigest"] == head["deliveryDigest"]));
    }
    // The recipient can still claim normally after sender edits.
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert!(claim["claim"].is_object());
}

#[tokio::test]
async fn structured_prompt_header_becomes_head_fields_and_replace_pending_edits() {
    use base64::Engine as _;
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    let header = |revision: u64, replace: bool| {
        let mut metadata = json!({"kind":"pi-input-gate.message","version":1,"subject":"Status",
            "correlation":{"namespace":"ns","key":"k","revision":revision}});
        if replace {
            metadata["supersession"] = json!({"mode":"replace_pending"});
        }
        let header = json!({"priority":"high","mediaType":"application/vnd.pi-input-gate.message+json;v=1","metadata":metadata});
        format!(
            "[[pi-input-gate:ingress:v3:{}]]\nbody r{revision}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header.to_string())
        )
    };
    let first = crate::app::messages::parse_structured_prompt(&header(1, false)).unwrap();
    assert_eq!(first.priority, "high");
    assert_eq!(first.subject, "Status");
    assert_eq!(first.body, "body r1");
    fixture
        .app
        .route_ordinary_send(&recipient, &sender, first.clone(), &Default::default())
        .unwrap();
    // Identical correlated send: the existing head, no duplicate.
    let again = fixture
        .app
        .route_ordinary_send(&recipient, &sender, first, &Default::default())
        .unwrap();
    assert!(matches!(again, SendRoute::Mailbox(ref d) if d.duplicate));
    // replace_pending with the same correlation edits the waiting head.
    let replacement = crate::app::messages::parse_structured_prompt(&header(2, true)).unwrap();
    let replaced = fixture
        .app
        .route_ordinary_send(&recipient, &sender, replacement, &Default::default())
        .unwrap();
    assert!(matches!(replaced, SendRoute::Mailbox(ref d) if d.edited));
    let heads = snapshot_heads(&mut fixture.app, &session);
    assert_eq!(heads.len(), 1);
    assert_eq!(heads[0]["body"], "body r2");
    assert_eq!(heads[0]["priority"], "high");
    // Once the correlated head was picked up, replace_pending asks again.
    dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let late = crate::app::messages::parse_structured_prompt(&header(3, true)).unwrap();
    assert!(matches!(
        fixture
            .app
            .route_ordinary_send(&recipient, &sender, late, &Default::default()),
        Err(SendRefusal::PendingChanged(_))
    ));
    assert!(crate::app::messages::parse_structured_prompt("plain text").is_none());
}

#[tokio::test]
async fn handoff_to_a_recipient_with_messages_is_queued_with_the_exact_envelope_text() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let identity = |app: &App, ws: usize, pane| {
        let agent = app.agent_info(ws, pane).unwrap();
        CanonicalHerdrIdentity {
            workspace_id: agent.workspace_id,
            pane_id: agent.pane_id,
            terminal_id: agent.terminal_id,
            agent_session: agent.agent_session.unwrap(),
        }
    };
    let envelope = HerdrHandoff {
        version: 1,
        message_id: "handoff-1".into(),
        created_at: "unix:1".into(),
        sender: identity(&fixture.app, 0, fixture.panes[0]),
        recipient: identity(&fixture.app, 1, fixture.panes[1]),
        kind: HandoffKind::Assignment,
        correlation_id: None,
        reply_to_id: None,
        task: None,
        summary: "do the thing".into(),
        artifact_refs: vec![],
    };
    let response = fixture.app.handle_handoff_send(
        "req".into(),
        HandoffSendParams {
            envelope: envelope.clone(),
            send: Default::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::HandoffTransport { receipt } = success.result else {
        panic!("receipt")
    };
    assert_eq!(receipt.outcome, HandoffTransportOutcome::MailboxAdmitted);
    assert!(fixture.rx[1].try_recv().is_err());
    let heads = snapshot_heads(&mut fixture.app, &session);
    assert_eq!(heads[0]["body"], envelope.prompt_text());
    assert_eq!(heads[0]["kind"], "assignment");
    // Resending the same messageId returns the same head.
    let response = fixture.app.handle_handoff_send(
        "req".into(),
        HandoffSendParams {
            envelope,
            send: Default::default(),
        },
    );
    assert!(response.contains("\"duplicate\":true"), "{response}");
    assert_eq!(snapshot_heads(&mut fixture.app, &session).len(), 1);
}

#[tokio::test]
async fn previous_session_heads_never_run_and_can_be_repinned_or_dropped() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["keep me", "drop me", "later"].iter().enumerate() {
        fixture
            .app
            .route_ordinary_send(
                &recipient,
                &sender,
                plain(body),
                &MessageSendOptions {
                    send_new: index > 0,
                    ..Default::default()
                },
            )
            .unwrap();
        if index == 1 {
            // The human restarts Pi in the pane: a new session is reported.
            report_session(
                &mut fixture.app,
                1,
                fixture.panes[1],
                "/sessions/s1-b.jsonl",
                20,
            );
        }
    }
    let snapshot = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap()["snapshot"]
        .clone();
    let heads = snapshot["heads"].as_array().unwrap().clone();
    let states = snapshot["headStates"].as_array().unwrap().clone();
    assert_eq!(heads.len(), 3, "previous-session heads stay visible");
    let state_of = |body: &str| {
        let head = heads.iter().find(|head| head["body"] == body).unwrap();
        states
            .iter()
            .find(|state| state["stableId"] == head["stableId"])
            .unwrap()
            .clone()
    };
    assert_eq!(state_of("keep me")["previousSession"], true);
    assert_eq!(
        state_of("keep me")["recipientSession"],
        "/sessions/s1-a.jsonl"
    );
    assert_eq!(state_of("drop me")["previousSession"], true);
    assert!(state_of("later").get("previousSession").is_none());
    assert_eq!(
        state_of("later")["recipientSession"],
        "/sessions/s1-b.jsonl"
    );
    // Claim skips the previous-session heads instead of blocking on them.
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let later = heads.iter().find(|head| head["body"] == "later").unwrap();
    assert_eq!(claim["claim"]["stableId"], later["stableId"]);
    let claim_id = claim["claim"]["claimId"].as_str().unwrap().to_string();
    for outcome in ["admitted", "settled"] {
        dispatch(
            &mut fixture.app,
            &session,
            "mailbox.resolve",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": outcome}),
        )
        .unwrap();
    }
    let pick = |body: &str| {
        heads
            .iter()
            .find(|head| head["body"] == body)
            .unwrap()
            .clone()
    };
    let version = |head: &serde_json::Value, revision: u64| {
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": head["stableId"],
               "expectedRevision": revision})
    };
    // Stale expectedRevision is refused.
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.repin",
        version(&pick("keep me"), 9)
    )
    .is_err());
    let repinned = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.repin",
        version(&pick("keep me"), 1),
    )
    .unwrap();
    assert_eq!(repinned["type"], "mailbox_repinned");
    assert_eq!(repinned["revision"], 2);
    assert_eq!(repinned["receipt"]["revision"], 2);
    assert_eq!(repinned["receipt"]["status"], "admitted");
    assert_eq!(repinned["recipientSession"], "/sessions/s1-b.jsonl");
    let dropped = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.drop",
        version(&pick("drop me"), 1),
    )
    .unwrap();
    assert_eq!(dropped["type"], "mailbox_dropped");
    assert_eq!(dropped["receipt"]["resolution"]["outcome"], "settled");
    let current = snapshot_heads(&mut fixture.app, &session);
    let bodies: Vec<_> = current.iter().map(|head| head["body"].clone()).collect();
    assert!(!bodies.contains(&json!("drop me")));
    assert!(bodies.contains(&json!("keep me")));
    // A current-session head cannot be repinned or dropped.
    let kept = current
        .iter()
        .find(|head| head["body"] == "keep me")
        .unwrap();
    assert!(dispatch(&mut fixture.app, &session, "mailbox.drop", version(kept, 2)).is_err());
    // The repinned head is now claimable; the dropped one never appears in history.
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert_eq!(claim["claim"]["stableId"], kept["stableId"]);
    let claim_id = claim["claim"]["claimId"].as_str().unwrap().to_string();
    for outcome in ["admitted", "settled"] {
        dispatch(
            &mut fixture.app,
            &session,
            "mailbox.resolve",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": outcome}),
        )
        .unwrap();
    }
    let history = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.history_snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let mut bodies: Vec<_> = history["snapshot"]["heads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|head| head["body"].as_str().unwrap().to_string())
        .collect();
    bodies.sort();
    assert_eq!(bodies, vec!["keep me", "later"]);
}

#[tokio::test]
async fn a_second_accepted_stream_never_revokes_the_first() {
    let mut fixture = fixture();
    let first = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("one"), &Default::default())
        .unwrap();
    let claim = dispatch(
        &mut fixture.app,
        &first,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    // Pi opens a dedicated watch stream for the same execution.
    let pair = std::os::unix::net::UnixStream::pair().unwrap();
    let second = fixture
        .app
        .accept_mailbox_bootstrap_stream(pair.0.as_raw_fd())
        .expect("second stream accepted");
    assert_ne!(second.binding_generation, first.binding_generation);
    assert!(fixture
        .app
        .mailbox_bootstrap_session_current(&first)
        .is_ok());
    assert!(fixture
        .app
        .mailbox_bootstrap_session_current(&second)
        .is_ok());
    // The first stream's outstanding claim still resolves through it.
    let claim_id = claim["claim"]["claimId"].as_str().unwrap().to_string();
    for outcome in ["admitted", "settled"] {
        dispatch(
            &mut fixture.app,
            &first,
            "mailbox.resolve",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": outcome}),
        )
        .unwrap();
    }
    assert!(fixture.app.mailbox_watch_marker(&second).is_ok());
    // Closing the watch stream releases only its own binding.
    fixture
        .app
        .release_mailbox_bootstrap_binding(&second.binding_generation);
    assert!(fixture
        .app
        .mailbox_bootstrap_session_current(&first)
        .is_ok());
    assert!(fixture
        .app
        .attached_messages_recipient(&recipient)
        .is_some());
    drop(pair);
}

#[tokio::test]
async fn recipient_only_binding_never_grants_send_report_or_route_authority() {
    let mut fixture = fixture();
    // Switch off: the untrusted Pi is refused outright.
    let pid = std::process::id();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    fixture.app.install_mailbox_bootstrap_test_foreground_job(
        terminal_id,
        crate::platform::ForegroundJob {
            process_group_id: pid,
            processes: vec![crate::platform::ForegroundProcess {
                pid,
                name: "pi".into(),
                argv0: None,
                argv: Some(vec!["pi".into()]),
                cmdline: Some("pi".into()),
            }],
        },
    );
    let pair = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(fixture
        .app
        .accept_mailbox_bootstrap_stream(pair.0.as_raw_fd())
        .is_err());
    let session = attach_recipient(&mut fixture);
    let binding = session.recipient_only.expect("recipient-only binding");
    let descriptor = crate::server::mailbox_bootstrap::descriptor_value(&session);
    assert_eq!(descriptor["binding"], "recipient_only");
    assert_eq!(
        descriptor["grantId"],
        format!("recipient-only:{}", fixture.terminals[1])
    );
    for absent in ["reportSubmit", "parentReport"] {
        assert!(
            descriptor.get(absent).is_none(),
            "{absent} must not be advertised"
        );
    }
    assert_eq!(descriptor["messages"]["repinMethod"], "mailbox.repin");
    assert_eq!(binding.foreground_pid, pid);
    assert!(session.parent_report.is_none());
    for (method, params) in [
        (
            "mailbox.offline_submit",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        ),
        ("report_submit", json!({})),
        ("report_submit_parent", json!({})),
        ("todo_state", json!({})),
        ("report_prepared", json!({})),
        ("report_path_attempt", json!({})),
        ("report_coverage", json!({})),
        ("mailbox.provision_recipient", json!({"target": "anyone"})),
        (
            "delegation.child_report_disposition",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "childDelegationId": "d1"}),
        ),
    ] {
        assert!(
            dispatch(&mut fixture.app, &session, method, params).is_err(),
            "{method} must be refused for a recipient-only session"
        );
    }
    // Never a trusted identity, never a ready route.
    let terminal = fixture
        .app
        .state
        .terminals
        .values()
        .find(|terminal| terminal.id.to_string() == fixture.terminals[1])
        .unwrap();
    assert!(fixture.app.trusted_managed_pi_session(terminal).is_none());
    let parent = fixture
        .app
        .state
        .delegations
        .create(Some(fixture.panes[0]), None, None)
        .unwrap();
    let child = fixture
        .app
        .state
        .delegations
        .create(Some(fixture.panes[1]), Some(parent), None)
        .unwrap();
    assert!(fixture.app.ready_route_shape(child, parent).is_none());
    // Its own inbox works, and turning the switch off revokes the session.
    assert!(snapshot_heads(&mut fixture.app, &session).is_empty());
    fixture.app.unmanaged_pi_messages = false;
    assert!(fixture
        .app
        .mailbox_bootstrap_session_current(&session)
        .is_err());
    assert!(fixture
        .app
        .attached_messages_recipient(&fixture.terminals[1])
        .is_none());
}

#[tokio::test]
async fn watch_marker_changes_on_every_append_and_bindings_release() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let before = fixture.app.mailbox_watch_marker(&session).unwrap();
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("wake"), &Default::default())
        .unwrap();
    assert_ne!(fixture.app.mailbox_watch_marker(&session).unwrap(), before);
    assert!(fixture
        .app
        .attached_messages_recipient(&recipient)
        .is_some());
    fixture
        .app
        .release_mailbox_bootstrap_binding(&session.binding_generation);
    assert!(fixture
        .app
        .attached_messages_recipient(&recipient)
        .is_none());
}
