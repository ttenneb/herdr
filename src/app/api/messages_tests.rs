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
    /// Added to the test Pi's start tick so a later attach is a new execution.
    execution_offset: u64,
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
        execution_offset: 0,
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
    let mut birth = crate::platform::process_birth_identity(pid).unwrap();
    // Each attach_* call may model a different Pi process in the pane.
    birth.start_ticks += fixture.execution_offset;
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
        external_key: None,
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

pub(crate) fn scoped_test_head(
    stable_id: &str,
    sender: &str,
    recipient: &crate::mailbox::RecipientKey,
    digest_char: char,
) -> crate::mailbox::MailboxHead {
    crate::mailbox::MailboxHead {
        stable_id: stable_id.into(),
        revision: 1,
        digest: digest_char.to_string().repeat(64),
        delivery_digest: format!("{stable_id}-delivery")
            .bytes()
            .map(|b| format!("{:x}", b % 16))
            .collect::<String>()
            .chars()
            .chain(std::iter::repeat('0'))
            .take(64)
            .collect(),
        recipient: recipient.clone(),
        subject: "original".into(),
        body: "original body".into(),
        recipient_generation: recipient.generation.clone(),
        sender: sender.into(),
        target: recipient.recipient_id.clone(),
        grant_id: format!("test-grant-{stable_id}"),
        message_id: format!("message-{stable_id}"),
        kind: "advisory".into(),
        priority: "normal".into(),
        original_sequence: 1,
        enqueue_epoch: 0,
        accepted_at: 1,
        delivery: None,
    }
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
async fn a_non_pi_agent_pane_keeps_the_pty_bytes_unchanged() {
    let mut fixture = fixture();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(Some(Agent::Claude), AgentState::Idle);
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
    let delivery = delivery.expect("delivery");
    assert_eq!(
        (delivery.path.as_str(), delivery.reason.as_str()),
        ("pty", "not_pi"),
        "non-Pi agent: typed, and the sender is told why"
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
async fn a_pi_pane_without_an_attached_pi_queues_and_requests_a_wake() {
    let mut fixture = fixture();
    // This pane's Pi attached Messages before and is now restarting or asleep.
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .messages_capable = true;
    // Its Pi has exited: the pane is at a shell with no agent process.
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(None, AgentState::Unknown);
    assert!(fixture
        .app
        .resolve_agent_target(&fixture.app.public_pane_id(1, fixture.panes[1]).unwrap())
        .is_err());
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let sequence = fixture.app.event_hub.current_sequence();
    let response = fixture.app.handle_agent_prompt(
        "req".into(),
        AgentPromptParams {
            target,
            text: "run when you wake".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    let delivery = delivery.expect("queued in the pane's queue");
    assert_eq!(delivery.path, "mailbox");
    assert!(fixture.rx[1].try_recv().is_err(), "nothing typed");
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    let recovered = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap();
    let head = recovered.heads.values().next().unwrap();
    assert_eq!(head.recipient.recipient_id, format!("pane:{queue_key}"));
    let wakes: Vec<_> = fixture
        .app
        .event_hub
        .events_after(sequence)
        .into_iter()
        .filter(|(_, event)| {
            matches!(
                event.event,
                crate::api::schema::EventKind::PaneWakeRequested
            )
        })
        .collect();
    assert_eq!(wakes.len(), 1);
    let crate::api::schema::EventData::PaneWakeRequested {
        queue_key: woken,
        stable_id,
        reason,
        ..
    } = &wakes[0].1.data
    else {
        panic!("wake data")
    };
    assert_eq!(woken, &queue_key);
    assert_eq!(stable_id, &head.stable_id);
    assert_eq!(reason, "message_queued");
    // When the pane's Pi attaches, the backlog is its inbox and runs in order.
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(Some(Agent::Pi), AgentState::Idle);
    let session = attach_recipient(&mut fixture);
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert_eq!(claim["claim"]["stableId"], json!(head.stable_id));
    // With a Pi attached, a new send requests no wake.
    let sequence = fixture.app.event_hub.current_sequence();
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    fixture.app.handle_agent_prompt(
        "req2".into(),
        AgentPromptParams {
            target,
            text: "attached now".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    assert!(!fixture
        .app
        .event_hub
        .events_after(sequence)
        .into_iter()
        .any(|(_, event)| matches!(
            event.event,
            crate::api::schema::EventKind::PaneWakeRequested
        )));
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
async fn previous_session_heads_run_in_order_with_the_pin_shown_and_can_be_dropped() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["first", "drop me", "later"].iter().enumerate() {
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
    let pick = |body: &str| {
        heads
            .iter()
            .find(|head| head["body"] == body)
            .unwrap()
            .clone()
    };
    let state_of = |body: &str| {
        let head = pick(body);
        states
            .iter()
            .find(|state| state["stableId"] == head["stableId"])
            .unwrap()
            .clone()
    };
    // The pin is display information only.
    assert_eq!(state_of("first")["previousSession"], true);
    assert_eq!(
        state_of("first")["recipientSession"],
        "/sessions/s1-a.jsonl"
    );
    assert!(state_of("later").get("previousSession").is_none());
    // The human drops one waiting head (any held head, expectedRevision).
    let dropped = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.drop",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": pick("drop me")["stableId"],
               "expectedRevision": 1}),
    )
    .unwrap();
    assert_eq!(dropped["type"], "mailbox_dropped");
    assert_eq!(dropped["receipt"]["resolution"]["outcome"], "settled");
    assert!(
        dispatch(
            &mut fixture.app,
            &session,
            "mailbox.drop",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": pick("later")["stableId"],
               "expectedRevision": 9}),
        )
        .is_err(),
        "stale expectedRevision"
    );
    assert!(
        dispatch(
            &mut fixture.app,
            &session,
            "mailbox.repin",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": pick("first")["stableId"],
               "expectedRevision": 1}),
        )
        .is_err(),
        "repin is gone"
    );
    // Remaining heads run in normal order, previous session included.
    let mut order = Vec::new();
    for _ in 0..2 {
        let claim = dispatch(
            &mut fixture.app,
            &session,
            "mailbox.claim",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .unwrap();
        let stable = claim["claim"]["stableId"].clone();
        order.push(
            heads
                .iter()
                .find(|head| head["stableId"] == stable)
                .unwrap()["body"]
                .clone(),
        );
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
    }
    assert_eq!(order, vec![json!("first"), json!("later")]);
    let history = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.history_snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    // History keeps the human's Drop, marked as such.
    let snapshot = &history["snapshot"];
    let mut rows: Vec<(String, Option<String>)> = snapshot["heads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|head| {
            let state = snapshot["headStates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|state| state["stableId"] == head["stableId"])
                .unwrap();
            (
                head["body"].as_str().unwrap().to_string(),
                state["closedBy"].as_str().map(str::to_string),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("drop me".to_string(), Some("dropped".to_string())),
            ("first".to_string(), None),
            ("later".to_string(), None),
        ]
    );
}

#[test]
fn claims_are_bound_to_the_claiming_execution() {
    let directory = unique_dir();
    let store = crate::mailbox::MailboxStore::open(&directory).unwrap();
    let recipient = crate::app::messages::pane_recipient("0123456789abcdef0123456789abcdef");
    for (index, message) in ["one", "two"].iter().enumerate() {
        store
            .append_offline_head(crate::mailbox::MailboxHead {
                stable_id: format!("send.{index}"),
                revision: 1,
                digest: format!("{index}").repeat(64),
                delivery_digest: format!("{}", index + 5).repeat(64),
                recipient: recipient.clone(),
                subject: "s".into(),
                body: (*message).into(),
                recipient_generation: "1".into(),
                sender: "sender".into(),
                target: "t".into(),
                grant_id: "g".into(),
                message_id: (*message).into(),
                kind: "advisory".into(),
                priority: "normal".into(),
                original_sequence: 1,
                enqueue_epoch: 0,
                accepted_at: 1,
                delivery: None,
            })
            .unwrap();
    }
    let keys = vec![recipient];
    let a = store
        .claim_next_for_execution(&keys, "pid:1:1")
        .unwrap()
        .unwrap();
    assert_eq!(a.stable_id, "send.0");
    // A different execution (for example a restarted Pi) never gets A's
    // claim and is not blocked by it: it takes the next head.
    let b = store
        .claim_next_for_execution(&keys, "pid:2:2")
        .unwrap()
        .unwrap();
    assert_eq!(b.stable_id, "send.1");
    assert_eq!(
        store.claim_next_for_execution(&keys, "pid:1:1").unwrap(),
        Some(a.clone()),
        "A still sees only its own claim"
    );
    let recovered = store.load().unwrap();
    let view = crate::app::messages::inbox_snapshot(&recovered, &keys, "pid:2:2", None, &|_| false)
        .unwrap();
    let marks: std::collections::HashMap<_, _> = view
        .head_states
        .iter()
        .map(|state| (state.stable_id.clone(), state.claim_execution.clone()))
        .collect();
    assert_eq!(marks["send.0"].as_deref(), Some("other"));
    assert_eq!(marks["send.1"].as_deref(), Some("current"));
    assert_eq!(view.claim.unwrap().stable_id, "send.1");
    std::fs::remove_dir_all(directory).ok();
}

/// #135/#118 continuity: messages queued for a pane survive a Herdr server
/// restart. The restored pane keeps its queue key (restore test in
/// persist::restore), and the same pane's Pi receives each message exactly
/// once, one claim at a time in priority order.
#[tokio::test]
async fn queued_messages_survive_a_server_restart_and_arrive_exactly_once() {
    let mut before = fixture();
    let terminal_id = before.app.state.workspaces[1]
        .terminal_id(before.panes[1])
        .unwrap()
        .clone();
    before
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .messages_capable = true;
    let queue_key = before.app.pane_queue_key(&before.terminals[1]).unwrap();
    let sender = sender(&before);
    let recipient = before.terminals[1].clone();
    for (index, body) in ["low one", "high one", "normal one"].iter().enumerate() {
        let mut message = plain(body);
        message.priority = ["low", "high", "normal"][index].into();
        before
            .app
            .route_ordinary_send(
                &recipient,
                &sender,
                message,
                &MessageSendOptions {
                    send_new: index > 0,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    // Server restart: a fresh App over the same durable state. The pane is
    // restored with its persisted queue key and a new terminal ID.
    let mut after = fixture();
    std::fs::remove_dir_all(&after.directory).ok();
    after.app.sender_authority_dir = before.directory.clone();
    let restored_terminal = after.app.state.workspaces[1]
        .terminal_id(after.panes[1])
        .unwrap()
        .clone();
    assert_ne!(
        restored_terminal.to_string(),
        recipient,
        "terminal IDs are re-minted"
    );
    {
        let terminal = after
            .app
            .state
            .terminals
            .get_mut(&restored_terminal)
            .unwrap();
        terminal.queue_key = queue_key.clone();
        terminal.messages_capable = true;
    }
    // The wake integration point resolves the restart-stable key (bare or
    // `pane:` form) to the restored pane's CURRENT terminal.
    for key in [queue_key.clone(), format!("pane:{queue_key}")] {
        let (_, _, terminal) = after.app.resolve_wake_pane_key(&key).expect("resolves");
        assert_eq!(terminal, restored_terminal, "{key}");
    }
    let session = attach_recipient(&mut after);
    let mut delivered = Vec::new();
    loop {
        let claim = dispatch(
            &mut after.app,
            &session,
            "mailbox.claim",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .unwrap();
        if claim["claim"].is_null() {
            break;
        }
        // One at a time: the same outstanding claim until it settles.
        let again = dispatch(
            &mut after.app,
            &session,
            "mailbox.claim",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .unwrap();
        assert_eq!(again["claim"], claim["claim"]);
        let claim_id = claim["claim"]["claimId"].as_str().unwrap().to_string();
        for outcome in ["admitted", "settled"] {
            dispatch(
                &mut after.app,
                &session,
                "mailbox.resolve",
                json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": outcome}),
            )
            .unwrap();
        }
        let recovered = crate::mailbox::MailboxStore::open(&before.directory)
            .unwrap()
            .load()
            .unwrap();
        delivered.push(
            recovered.heads[claim["claim"]["stableId"].as_str().unwrap()]
                .body
                .clone(),
        );
    }
    assert_eq!(delivered, vec!["high one", "normal one", "low one"]);
    std::fs::remove_dir_all(&after.directory).ok();
}

/// The human's busy-time typing joins the same queue as a server head, only
/// in the typing pane's own inbox, for managed and receive-only bindings.
#[tokio::test]
async fn enqueue_self_queues_only_into_the_own_pane_inbox() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    assert!(session.recipient_only.is_some(), "allowed for receive-only");
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    let enqueue = |app: &mut App, body: &str, client: &str| {
        dispatch(
            app,
            &session,
            "mailbox.enqueue_self",
            json!({"protocol": crate::mailbox_v1::PROTOCOL, "subject": "Typed while busy",
                   "body": body, "priority": "normal", "clientId": client}),
        )
    };
    let first = enqueue(&mut fixture.app, "fix the test first", "typed-1").unwrap();
    assert_eq!(first["type"], "mailbox_enqueued");
    assert_eq!(first["duplicate"], false);
    assert_eq!(first["receipt"]["status"], "admitted");
    // A retry with the same clientId never queues twice.
    let again = enqueue(&mut fixture.app, "fix the test first", "typed-1").unwrap();
    assert_eq!(again["duplicate"], true);
    assert_eq!(again["stableId"], first["stableId"]);
    let heads = snapshot_heads(&mut fixture.app, &session);
    assert_eq!(heads.len(), 1);
    assert_eq!(
        heads[0]["recipient"]["recipientId"],
        format!("pane:{queue_key}")
    );
    assert_eq!(
        heads[0]["sender"],
        format!("human@{}", fixture.terminals[1])
    );
    assert_eq!(heads[0]["delivery"]["origin"], "human_typed");
    assert_eq!(
        heads[0]["delivery"]["senderLabel"],
        format!(
            "human at {}",
            fixture.app.public_pane_id(1, fixture.panes[1]).unwrap()
        )
    );
    // No selector can point it anywhere else, and bad input is refused.
    for params in [
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "subject": "s", "body": "b",
               "recipient": {"recipientId": format!("pane:{}", fixture.app.pane_queue_key(&fixture.terminals[0]).unwrap()), "generation": "1"}}),
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "subject": "s", "body": "b", "target": fixture.terminals[0]}),
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "subject": "s", "body": "b", "priority": "urgent"}),
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "subject": "s", "body": "\u{1b}[2J"}),
    ] {
        assert!(dispatch(&mut fixture.app, &session, "mailbox.enqueue_self", params).is_err());
    }
    let recovered = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(recovered.heads.len(), 1, "nothing reached another pane");
    // It runs in the same order as every other head: the Pi claims it.
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert_eq!(claim["claim"]["stableId"], first["stableId"]);
}

fn claim_and_admit(app: &mut App, session: &MailboxBootstrapSession) -> serde_json::Value {
    let claim = dispatch(
        app,
        session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    dispatch(
        app,
        session,
        "mailbox.resolve",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim["claim"]["claimId"], "outcome": "admitted"}),
    )
    .unwrap();
    claim["claim"].clone()
}

fn recovery_states(app: &mut App, session: &MailboxBootstrapSession) -> Vec<serde_json::Value> {
    let snapshot = dispatch(
        app,
        session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap()["snapshot"]
        .clone();
    let recovery: Vec<serde_json::Value> = snapshot["headStates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|state| state["recoveryNeeded"] == true)
        .cloned()
        .collect();
    // Another Pi's claim is never presented as this Pi's claim.
    assert!(!recovery
        .iter()
        .any(|state| state["stableId"] == snapshot["claim"]["stableId"]));
    if !snapshot["claim"].is_null() {
        assert_eq!(
            snapshot["claim"]["execution"],
            crate::app::messages::session_execution(session)
        );
    }
    recovery
}

/// A Pi killed mid-turn leaves its claim admitted. The next Pi in the pane sees
/// it as needing recovery (never auto-rerun, never blocking), and Drop or
/// Retry resolves it with a durable receipt.
#[tokio::test]
async fn a_killed_pis_admitted_claim_needs_recovery_and_drop_or_retry_resolves_it() {
    let mut fixture = fixture();
    let first = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["running when killed", "also running", "next"]
        .iter()
        .enumerate()
    {
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
    }
    let dropped_claim = claim_and_admit(&mut fixture.app, &first);
    // A second claim by the same Pi is refused while the first is outstanding.
    // Settle nothing: the Pi is killed mid-turn; its binding ends.
    fixture
        .app
        .release_mailbox_bootstrap_binding(&first.binding_generation);
    fixture.execution_offset = 1;
    let second = attach_recipient(&mut fixture);
    assert_ne!(
        crate::app::messages::session_execution(&first),
        crate::app::messages::session_execution(&second)
    );
    let recovery = recovery_states(&mut fixture.app, &second);
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0]["stableId"], dropped_claim["stableId"]);
    assert_eq!(recovery[0]["lifecycle"], "admitted");
    assert_eq!(recovery[0]["claimExecution"], "other");
    // Never auto-rerun and never stuck: the new Pi's claim skips it.
    let next = dispatch(
        &mut fixture.app,
        &second,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert_ne!(next["claim"]["stableId"], dropped_claim["stableId"]);
    // The new Pi can never re-admit (rerun) another execution's claim.
    assert!(dispatch(
        &mut fixture.app,
        &second,
        "mailbox.resolve",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": dropped_claim["claimId"], "outcome": "admitted"}),
    )
    .is_err());
    let dropped = dispatch(
        &mut fixture.app,
        &second,
        "mailbox.drop",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": dropped_claim["stableId"], "expectedRevision": 1}),
    )
    .unwrap();
    assert_eq!(dropped["type"], "mailbox_dropped");
    assert_eq!(dropped["receipt"]["resolution"]["outcome"], "settled");
    assert_eq!(
        dropped["receipt"]["claim"]["claimId"],
        dropped_claim["claimId"]
    );
    // Durable: a fresh load sees the settled resolution.
    let recovered = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        recovered.resolutions[dropped_claim["claimId"].as_str().unwrap()].outcome,
        crate::mailbox::ClaimResolutionOutcome::Settled
    );
    assert!(recovery_states(&mut fixture.app, &second).is_empty());

    // Retry: the second Pi is killed too, holding "also running"'s claim.
    let retried_claim = next["claim"].clone();
    dispatch(
        &mut fixture.app,
        &second,
        "mailbox.resolve",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": retried_claim["claimId"], "outcome": "admitted"}),
    )
    .unwrap();
    fixture
        .app
        .release_mailbox_bootstrap_binding(&second.binding_generation);
    fixture.execution_offset = 2;
    let third = attach_recipient(&mut fixture);
    let recovery = recovery_states(&mut fixture.app, &third);
    assert_eq!(recovery.len(), 1);
    let retry = json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": retried_claim["stableId"], "expectedRevision": 1});
    let retried = dispatch(&mut fixture.app, &third, "mailbox.retry", retry.clone()).unwrap();
    assert_eq!(retried["type"], "mailbox_retried");
    assert_eq!(retried["receipt"]["head"]["status"], "admitted");
    assert_eq!(retried["receipt"]["resolution"]["outcome"], "settled");
    // Idempotent: a repeated Retry returns the same re-delivery.
    let again = dispatch(&mut fixture.app, &third, "mailbox.retry", retry).unwrap();
    assert_eq!(again["newStableId"], retried["newStableId"]);
    assert!(recovery_states(&mut fixture.app, &third).is_empty());
    let closed = |app: &mut App, session: &MailboxBootstrapSession, stable: &serde_json::Value| {
        dispatch(
            app,
            session,
            "mailbox.snapshot",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .unwrap()["snapshot"]["headStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| &state["stableId"] == stable)
            .unwrap()["closedBy"]
            .clone()
    };
    assert_eq!(
        closed(&mut fixture.app, &third, &dropped_claim["stableId"]),
        "dropped"
    );
    assert_eq!(
        closed(&mut fixture.app, &third, &retried_claim["stableId"]),
        "retried"
    );
    // The re-delivered copy runs in normal order, marked as a retry.
    let mut claimed = Vec::new();
    for _ in 0..2 {
        let claim = dispatch(
            &mut fixture.app,
            &third,
            "mailbox.claim",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .unwrap();
        let stable = claim["claim"]["stableId"].as_str().unwrap().to_string();
        let claim_id = claim["claim"]["claimId"].clone();
        for outcome in ["admitted", "settled"] {
            dispatch(
                &mut fixture.app,
                &third,
                "mailbox.resolve",
                json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": outcome}),
            )
            .unwrap();
        }
        claimed.push(stable);
    }
    assert!(claimed.contains(&retried["newStableId"].as_str().unwrap().to_string()));
    let recovered = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap();
    let copy = &recovered.heads[retried["newStableId"].as_str().unwrap()];
    assert_eq!(copy.body, "also running");
    assert_eq!(
        copy.delivery.as_ref().unwrap().retry_of.as_deref(),
        retried_claim["stableId"].as_str()
    );
}

/// The same recovery across a Herdr server restart: the claim was taken by a
/// Pi in the old server; the restored pane's new Pi sees it as needing
/// recovery and Drop resolves it durably.
#[tokio::test]
async fn recovery_is_visible_and_droppable_after_a_server_restart() {
    let mut before = fixture();
    let first = attach_recipient(&mut before);
    let queue_key = before.app.pane_queue_key(&before.terminals[1]).unwrap();
    let sender = sender(&before);
    let recipient = before.terminals[1].clone();
    before
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("mid-turn at restart"),
            &Default::default(),
        )
        .unwrap();
    let claim = claim_and_admit(&mut before.app, &first);
    // Server restart: fresh App, same durable state, restored pane key.
    let mut after = fixture();
    std::fs::remove_dir_all(&after.directory).ok();
    after.app.sender_authority_dir = before.directory.clone();
    // The Pi in the restored pane is a new process.
    after.execution_offset = 1;
    let restored_terminal = after.app.state.workspaces[1]
        .terminal_id(after.panes[1])
        .unwrap()
        .clone();
    {
        let terminal = after
            .app
            .state
            .terminals
            .get_mut(&restored_terminal)
            .unwrap();
        terminal.queue_key = queue_key;
        terminal.messages_capable = true;
    }
    let session = attach_recipient(&mut after);
    let recovery = recovery_states(&mut after.app, &session);
    assert_eq!(recovery.len(), 1);
    assert_eq!(recovery[0]["stableId"], claim["stableId"]);
    let next = dispatch(
        &mut after.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert!(next["claim"].is_null(), "never auto-rerun");
    let dropped = dispatch(
        &mut after.app,
        &session,
        "mailbox.drop",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": claim["stableId"], "expectedRevision": 1}),
    )
    .unwrap();
    assert_eq!(dropped["receipt"]["resolution"]["outcome"], "settled");
    assert!(recovery_states(&mut after.app, &session).is_empty());
}

/// rc2 property, extended to pane queues and receive-only Pis: the pane's
/// currently attached Pi may settle the claim a previous Pi left admitted;
/// the ended Pi's binding and any other pane's Pi may not.
#[tokio::test]
async fn only_the_panes_attached_pi_may_settle_a_leftover_claim() {
    let mut fixture = fixture();
    let old = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("left admitted"),
            &Default::default(),
        )
        .unwrap();
    let leftover = claim_and_admit(&mut fixture.app, &old);
    fixture
        .app
        .release_mailbox_bootstrap_binding(&old.binding_generation);
    fixture.execution_offset = 1;
    let current = attach_recipient(&mut fixture);
    let settle = json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": leftover["claimId"], "outcome": "settled"});
    // The ended Pi's binding is gone.
    assert!(dispatch(&mut fixture.app, &old, "mailbox.resolve", settle.clone()).is_err());
    // A Pi attached to another pane never sees this pane's claim.
    let other_pane_terminal = fixture.app.state.workspaces[0]
        .terminal_id(fixture.panes[0])
        .unwrap()
        .clone();
    let other = {
        let mut other = current.clone();
        other.caller = other_pane_terminal.to_string();
        other
    };
    assert!(dispatch(&mut fixture.app, &other, "mailbox.resolve", settle.clone()).is_err());
    // The pane's attached Pi settles it; history records it as recovered.
    let resolved = dispatch(&mut fixture.app, &current, "mailbox.resolve", settle).unwrap();
    assert_eq!(resolved["resolution"]["outcome"], "settled");
    assert_eq!(resolved["resolution"]["closedBy"], "recovered");
    assert!(recovery_states(&mut fixture.app, &current).is_empty());
}

#[tokio::test]
async fn a_held_head_is_editable_while_another_head_is_claimed_and_admitted() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["running", "waiting"].iter().enumerate() {
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
    }
    let protocol = json!({"protocol": crate::mailbox_v1::PROTOCOL});
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        protocol.clone(),
    )
    .unwrap();
    let claim_id = claim["claim"]["claimId"].as_str().unwrap().to_string();
    dispatch(
        &mut fixture.app,
        &session,
        "mailbox.resolve",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": "admitted"}),
    )
    .unwrap();
    let heads = snapshot_heads(&mut fixture.app, &session);
    let waiting = heads
        .iter()
        .find(|head| head["body"] == "waiting")
        .unwrap()
        .clone();
    let running = heads
        .iter()
        .find(|head| head["body"] == "running")
        .unwrap()
        .clone();
    assert_eq!(claim["claim"]["stableId"], running["stableId"]);
    // The recipient edits the held head while the other head's turn runs.
    let edited = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.edit",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": waiting["stableId"],
               "revision": waiting["revision"], "digest": waiting["digest"],
               "subject": "Message from tpm", "body": "waiting, edited mid-turn"}),
    )
    .unwrap();
    let snapshot = &edited["snapshot"];
    // The live claim is untouched and still the snapshot's outstanding claim.
    assert_eq!(snapshot["claim"]["claimId"], json!(claim_id));
    let state = |stable: &serde_json::Value| {
        snapshot["headStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| &state["stableId"] == stable)
            .unwrap()
            .clone()
    };
    assert_eq!(state(&running["stableId"])["lifecycle"], "admitted");
    assert_eq!(state(&waiting["stableId"])["lifecycle"], "held");
    assert_eq!(state(&waiting["stableId"])["revision"], 2);
    // The sender's --edit-pending takes the same per-head path.
    let sender_edit = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("waiting, edited again by the sender"),
            &MessageSendOptions {
                edit_pending: Some(waiting["stableId"].as_str().unwrap().into()),
                expect_revision: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(sender_edit, SendRoute::Mailbox(ref d) if d.revision == Some(3)));
    // The claimed head itself stays immutable.
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.edit",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": running["stableId"],
               "revision": running["revision"], "digest": running["digest"],
               "subject": "x", "body": "y"}),
    )
    .is_err());
    // Settling the running turn then delivers the latest edited revision.
    dispatch(
        &mut fixture.app,
        &session,
        "mailbox.resolve",
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "claimId": claim_id, "outcome": "settled"}),
    )
    .unwrap();
    let next = dispatch(&mut fixture.app, &session, "mailbox.claim", protocol).unwrap();
    assert_eq!(next["claim"]["stableId"], waiting["stableId"]);
    assert_eq!(next["claim"]["revision"], 3);
}

/// Puts pane 1 to sleep the way `herdr agent sleep` leaves it: a Pi launch
/// recipe, a sleep record, no agent process, and a fresh shell runtime.
fn put_recipient_to_sleep(fixture: &mut Fixture) -> tokio::sync::mpsc::Receiver<Bytes> {
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    fixture
        .app
        .terminal_runtimes
        .insert(terminal_id.clone(), runtime);
    let terminal = fixture.app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.set_detected_state(None, AgentState::Unknown);
    terminal.launch_recipe = crate::launch_recipe::LaunchRecipe::capture("owner", "pi", &[], &[]);
    terminal.sleep = Some(App::new_pane_sleep("owner".into(), 1));
    input
}

fn wake_records(fixture: &Fixture) -> Vec<serde_json::Value> {
    std::fs::read_dir(fixture.directory.join("pane-wakes"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
                .collect()
        })
        .unwrap_or_default()
}

/// A head appended for a pane Herdr put to sleep calls wake_pane with the
/// pane's durable key; the message waits in the queue for the woken Pi.
#[tokio::test]
async fn a_message_for_a_sleeping_pane_wakes_it_by_its_durable_key() {
    let mut fixture = fixture();
    let mut launch = put_recipient_to_sleep(&mut fixture);
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    // Addressed by the name it slept under.
    let response = fixture.app.handle_agent_prompt(
        "req".into(),
        AgentPromptParams {
            target: "owner".into(),
            text: "please continue".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    assert_eq!(delivery.unwrap().path, "mailbox");
    let records = wake_records(&fixture);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["paneKey"], format!("pane:{queue_key}"));
    assert_eq!(records[0]["trigger"]["cause"], "head_appended");
    assert_eq!(
        records[0]["trigger"]["recipientId"],
        format!("pane:{queue_key}")
    );
    assert!(
        launch.try_recv().is_ok(),
        "the managed launch was submitted"
    );
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    assert!(fixture.app.pane_wakes.contains_key(&terminal_id));
    // A second message coalesces into the outstanding wake.
    fixture
        .app
        .route_ordinary_send(
            &fixture.terminals[1].clone(),
            &sender(&fixture),
            plain("and this"),
            &MessageSendOptions {
                send_new: true,
                ..Default::default()
            },
        )
        .unwrap();
    fixture
        .app
        .request_pane_wake_if_detached(&fixture.terminals[1].clone(), "second");
    // The second message joins the outstanding wake without another call:
    // one wake, one durable record, no duplicate records.
    let records = wake_records(&fixture);
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(fixture.app.pane_wakes.contains_key(&terminal_id));
}

/// A pane that is not asleep (hand-quit Pi, hand-typed pi, helper) is never
/// woken: its message just waits for the next Pi.
#[tokio::test]
async fn a_pane_not_put_to_sleep_is_never_woken() {
    let mut fixture = fixture();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    {
        let terminal = fixture.app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.messages_capable = true;
        terminal.set_detected_state(None, AgentState::Unknown);
    }
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    fixture.app.handle_agent_prompt(
        "req".into(),
        AgentPromptParams {
            target,
            text: "wait for the next Pi".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    assert!(wake_records(&fixture).is_empty());
    assert!(fixture.app.pane_wakes.is_empty());
}

/// After a server restart a slept pane is restored asleep; the first backlog
/// sweep wakes it with restore_backlog, and repeated sweeps coalesce.
#[tokio::test]
async fn the_backlog_sweep_wakes_a_sleeping_pane_with_queued_messages() {
    let mut fixture = fixture();
    let _launch = put_recipient_to_sleep(&mut fixture);
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    // A head queued while the server was down (written straight to the store).
    crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .append_offline_head(crate::mailbox::MailboxHead {
            stable_id: "send.backlog".into(),
            revision: 1,
            digest: "a".repeat(64),
            delivery_digest: "b".repeat(64),
            recipient: crate::app::messages::pane_recipient(&queue_key),
            subject: "s".into(),
            body: "queued before restart".into(),
            recipient_generation: "1".into(),
            sender: "external".into(),
            target: fixture.terminals[1].clone(),
            grant_id: "send:external".into(),
            message_id: "m".into(),
            kind: "advisory".into(),
            priority: "normal".into(),
            original_sequence: 1,
            enqueue_epoch: 0,
            accepted_at: 1,
            delivery: None,
        })
        .unwrap();
    let start = fixture.app.server_started_at;
    assert!(!fixture.app.maybe_sweep_sleeping_backlog(start));
    assert!(
        wake_records(&fixture).is_empty(),
        "not before the first-sweep delay"
    );
    fixture
        .app
        .maybe_sweep_sleeping_backlog(start + std::time::Duration::from_secs(4));
    let records = wake_records(&fixture);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["trigger"]["cause"], "restore_backlog");
    assert_eq!(records[0]["trigger"]["headId"], "send.backlog");
    assert_eq!(records[0]["outcome"], "started");
    fixture
        .app
        .maybe_sweep_sleeping_backlog(start + std::time::Duration::from_secs(40));
    // A repeat sweep while the wake is outstanding writes no record.
    assert_eq!(wake_records(&fixture).len(), 1);
}

/// Final sender contract (a6d1b1f8): error.pending is the array of waiting
/// messages, newest first, and --edit-pending names one by stableId.
/// `newest` and `pendingCount` are additive conveniences.
#[tokio::test]
async fn edit_pending_needs_an_explicit_stable_id_and_the_pending_list_is_newest_first() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    let mut ids = Vec::new();
    for (index, body) in ["oldest", "middle", "newest"].iter().enumerate() {
        let SendRoute::Mailbox(delivery) = fixture
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
            .unwrap()
        else {
            panic!("mailbox")
        };
        ids.push(delivery.stable_id.unwrap());
    }
    let refusal = fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("another"), &Default::default())
        .unwrap_err();
    let json: serde_json::Value = serde_json::from_str(
        &crate::app::messages::pending_error_json("id".into(), &refusal).unwrap(),
    )
    .unwrap();
    assert_eq!(json["error"]["code"], "pending_exists");
    assert_eq!(json["error"]["pendingCount"], 3);
    assert_eq!(json["error"]["newest"]["stableId"], json!(ids[2]));
    assert_eq!(json["error"]["pending"][0], json["error"]["newest"]);
    // No implicit "newest": an empty stableId matches nothing.
    assert!(matches!(
        fixture.app.route_ordinary_send(
            &recipient,
            &sender,
            plain("no id"),
            &MessageSendOptions {
                edit_pending: Some(String::new()),
                ..Default::default()
            },
        ),
        Err(SendRefusal::PendingChanged(_))
    ));
    assert_eq!(json["error"]["pending"][0]["stableId"], json!(ids[2]));
    assert_eq!(json["error"]["pending"][2]["stableId"], json!(ids[0]));
    let edited = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("newest, edited"),
            &MessageSendOptions {
                edit_pending: Some(ids[2].clone()),
                expect_revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        matches!(edited, SendRoute::Mailbox(ref d) if d.edited && d.stable_id.as_deref() == Some(ids[2].as_str()))
    );
    let bodies: Vec<_> = snapshot_heads(&mut fixture.app, &session)
        .iter()
        .map(|head| head["body"].as_str().unwrap().to_string())
        .collect();
    assert!(bodies.contains(&"oldest".to_string()));
    assert!(bodies.contains(&"middle".to_string()));
    assert!(bodies.contains(&"newest, edited".to_string()));
}

/// The recipient's reprioritize: a held head changes priority as a new
/// revision with its receipt, claim order follows it, and stale or claimed
/// heads are refused. Arrival order (enqueueEpoch, acceptedAt) and the sender
/// label are in every snapshot head.
#[tokio::test]
async fn reprioritize_changes_claim_order_with_a_durable_receipt() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["first normal", "second normal"].iter().enumerate() {
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
    }
    let heads = snapshot_heads(&mut fixture.app, &session);
    let pick = |body: &str| {
        heads
            .iter()
            .find(|head| head["body"] == body)
            .unwrap()
            .clone()
    };
    let first = pick("first normal");
    let second = pick("second normal");
    assert!(first["enqueueEpoch"].as_u64() < second["enqueueEpoch"].as_u64());
    assert!(first["acceptedAt"].as_u64().is_some());
    assert_eq!(first["delivery"]["senderLabel"], "tpm");
    let request = |stable: &serde_json::Value, revision: u64, priority: &str| {
        json!({"protocol": crate::mailbox_v1::PROTOCOL, "stableId": stable,
               "expectedRevision": revision, "priority": priority})
    };
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.reprioritize",
        request(&second["stableId"], 9, "high")
    )
    .is_err());
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.reprioritize",
        request(&second["stableId"], 1, "urgent")
    )
    .is_err());
    let done = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.reprioritize",
        request(&second["stableId"], 1, "high"),
    )
    .unwrap();
    assert_eq!(done["type"], "mailbox_reprioritized");
    assert_eq!(done["revision"], 2);
    assert_eq!(done["priority"], "high");
    assert_eq!(done["receipt"]["revision"], 2);
    assert_eq!(done["receipt"]["status"], "admitted");
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert_eq!(
        claim["claim"]["stableId"], second["stableId"],
        "high runs first"
    );
    assert_eq!(claim["claim"]["revision"], 2);
    // A claimed head can no longer be reprioritized.
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.reprioritize",
        request(&second["stableId"], 2, "low")
    )
    .is_err());
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
    // This fixture is a hand-typed Pi: both streams are recipient-only and
    // both advertise the watch.
    for session in [&first, &second] {
        assert!(session.recipient_only.is_some());
        let descriptor = crate::server::mailbox_bootstrap::descriptor_value(session);
        assert_eq!(descriptor["binding"], "recipient_only");
        assert_eq!(descriptor["messages"]["watchMethod"], "mailbox.watch");
        assert_eq!(descriptor["messages"]["watchMaxWaitMs"], 30000);
    }
    // The watch stream sees the next append while the first stays current.
    let watched = fixture.app.mailbox_watch_marker(&second).unwrap();
    fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("two"),
            &MessageSendOptions {
                send_new: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_ne!(fixture.app.mailbox_watch_marker(&second).unwrap(), watched);
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
    fixture.app.unmanaged_pi_messages = false;
    let pair = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(fixture
        .app
        .accept_mailbox_bootstrap_stream(pair.0.as_raw_fd())
        .is_err());
    let session = attach_recipient(&mut fixture);
    let binding = session.recipient_only.expect("recipient-only binding");
    let descriptor = crate::server::mailbox_bootstrap::descriptor_value(&session);
    assert_eq!(descriptor["binding"], "recipient_only");
    assert!(descriptor.get("childDoneSignals").is_none());
    assert!(descriptor.get("parentSignals").is_none());
    assert!(
        descriptor.get("recipientOnly").is_none(),
        "only the agreed binding field"
    );
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    assert_eq!(
        descriptor["grantId"],
        format!("recipient-only:pane:{queue_key}")
    );
    assert_eq!(
        descriptor["messages"]["inbox"]["recipientId"],
        format!("pane:{queue_key}")
    );
    for absent in ["reportSubmit", "parentReport"] {
        assert!(
            descriptor.get(absent).is_none(),
            "{absent} must not be advertised"
        );
    }
    assert_eq!(descriptor["messages"]["dropMethod"], "mailbox.drop");
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

/// Authority scope on the Messages stream (the same code serves managed and
/// recipient-only sessions): edit, drop, reprioritize, retry and resolve of
/// a head addressed to another pane are refused and change nothing.
#[tokio::test]
async fn a_stream_never_touches_another_panes_heads() {
    let mut fixture = fixture();
    let x = attach_recipient(&mut fixture);
    let y = crate::mailbox::RecipientKey {
        recipient_id: fixture.terminals[0].clone(),
        generation: "1".into(),
    };
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    store
        .append_offline_head(scoped_test_head("for-y", "term_z", &y, 'b'))
        .unwrap();
    let before = store.load().unwrap();
    let protocol = crate::mailbox_v1::PROTOCOL;
    for (method, params) in [
        (
            "mailbox.edit",
            json!({"protocol": protocol, "stableId": "for-y", "revision": 1,
                   "digest": "b".repeat(64), "subject": "tampered", "body": "tampered"}),
        ),
        (
            "mailbox.drop",
            json!({"protocol": protocol, "stableId": "for-y", "expectedRevision": 1}),
        ),
        (
            "mailbox.reprioritize",
            json!({"protocol": protocol, "stableId": "for-y", "expectedRevision": 1, "priority": "high"}),
        ),
        (
            "mailbox.retry",
            json!({"protocol": protocol, "stableId": "for-y", "expectedRevision": 1}),
        ),
    ] {
        assert!(
            matches!(
                dispatch(&mut fixture.app, &x, method, params),
                Err(crate::app::MailboxBootstrapError::HeadOutOfScope)
            ),
            "{method} must refuse a head addressed to another pane"
        );
    }
    // A claim held in Y's inbox cannot be resolved from X's stream either.
    store
        .claim(crate::mailbox::Claim {
            claim_id: "y-claim".into(),
            stable_id: "for-y".into(),
            revision: 1,
            digest: "b".repeat(64),
            recipient: y.clone(),
            execution: None,
        })
        .unwrap();
    assert!(matches!(
        dispatch(
            &mut fixture.app,
            &x,
            "mailbox.resolve",
            json!({"protocol": protocol, "claimId": "y-claim", "outcome": "settled"}),
        ),
        Err(crate::app::MailboxBootstrapError::HeadOutOfScope)
    ));
    let after = store.load().unwrap();
    assert_eq!(after.heads, before.heads, "nothing was changed");
    assert!(after.resolutions.is_empty());
}

/// A claim held by another execution is recoverable (drop, retry, settle)
/// by the pane's attached Pi only when that execution is gone; a live other
/// execution's claim is shown as alive and refused.
#[tokio::test]
async fn only_a_gone_executions_claim_can_be_dropped_or_retried() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["live", "gone-drop", "gone-retry"].iter().enumerate() {
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
    }
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    let recovered = store.load().unwrap();
    let head_of = |body: &str| {
        recovered
            .heads
            .values()
            .find(|head| head.body.contains(body))
            .cloned()
            .unwrap()
    };
    // A live other execution and two gone ones (no process).
    let live_pid = 4_000_000_001_u32;
    fixture
        .app
        .messages_test_live_executions
        .insert(format!("pid:{live_pid}:5"));
    for (body, execution) in [
        ("live", format!("pid:{live_pid}:5")),
        ("gone-drop", "pid:4000000002:5".to_string()),
        ("gone-retry", "pid:4000000003:5".to_string()),
    ] {
        let head = head_of(body);
        store
            .claim(crate::mailbox::Claim {
                claim_id: format!("claim-{body}"),
                recipient: head.recipient.clone(),
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                execution: Some(execution),
            })
            .unwrap();
    }
    let snapshot = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let state_of = |body: &str| {
        let id = head_of(body).stable_id;
        snapshot["snapshot"]["headStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["stableId"] == json!(id))
            .cloned()
            .unwrap()
    };
    assert_eq!(state_of("live")["claimExecution"], "other");
    assert_eq!(state_of("live")["claimExecutionAlive"], true);
    assert!(state_of("live").get("recoveryNeeded").is_none());
    assert_eq!(state_of("gone-drop")["claimExecutionAlive"], false);
    assert_eq!(state_of("gone-drop")["recoveryNeeded"], true);

    let protocol = crate::mailbox_v1::PROTOCOL;
    let live = head_of("live");
    for (method, params) in [
        (
            "mailbox.drop",
            json!({"protocol": protocol, "stableId": live.stable_id, "expectedRevision": 1}),
        ),
        (
            "mailbox.retry",
            json!({"protocol": protocol, "stableId": live.stable_id, "expectedRevision": 1}),
        ),
        (
            "mailbox.resolve",
            json!({"protocol": protocol, "claimId": "claim-live", "outcome": "settled"}),
        ),
    ] {
        assert!(
            matches!(
                dispatch(&mut fixture.app, &session, method, params),
                Err(crate::app::MailboxBootstrapError::ClaimExecutionAlive)
            ),
            "{method} must refuse a live execution's claim"
        );
    }
    assert!(
        store.load().unwrap().resolutions.is_empty(),
        "nothing changed"
    );
    // Gone executions: Drop and Retry both work, with durable receipts.
    let dropped = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.drop",
        json!({"protocol": protocol, "stableId": head_of("gone-drop").stable_id, "expectedRevision": 1}),
    )
    .unwrap();
    assert_eq!(dropped["type"], "mailbox_dropped");
    let retried = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.retry",
        json!({"protocol": protocol, "stableId": head_of("gone-retry").stable_id, "expectedRevision": 1}),
    )
    .unwrap();
    assert_eq!(retried["type"], "mailbox_retried");
    // The retried message is held again and is the next claim here.
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": protocol}),
    )
    .unwrap();
    assert_eq!(claim["claim"]["stableId"], retried["newStableId"]);
}

/// A named Pi agent whose Pi was quit by hand (or is restarting) is still
/// addressed by its name: the prompt is queued in its pane, not refused with
/// agent_not_found, and the pane is not woken (it was not put to sleep).
#[tokio::test]
async fn a_named_agent_with_no_attached_pi_still_receives_queued_messages() {
    let mut fixture = fixture();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    {
        let terminal = fixture.app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("worker".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        terminal.messages_capable = true;
    }
    // The human quits Pi: the process exits and the pane is back at a shell.
    fixture
        .app
        .handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: fixture.panes[1],
            agent: None,
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });
    let response = fixture.app.handle_agent_prompt(
        "by-name".into(),
        AgentPromptParams {
            target: "worker".into(),
            text: "for when you are back".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response)
        .unwrap_or_else(|_| panic!("queued, not refused: {response}"));
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    assert_eq!(delivery.expect("delivery").path, "mailbox");
    assert!(fixture.rx[1].try_recv().is_err(), "nothing typed");
    assert!(
        fixture.app.pane_wakes.is_empty(),
        "a hand quit is never woken"
    );
}

/// Upgrade safety: a claim written before per-execution claims (no
/// execution) is never the new Pi's own. The new Pi is not handed it to run
/// again, skips it and takes the next head; the old claim shows as another,
/// gone execution's (recovery needed) and only an explicit Retry, Drop or
/// settle resolves it.
#[tokio::test]
async fn a_pre_upgrade_claim_without_an_execution_is_never_current() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["admitted before the upgrade", "queued after"]
        .iter()
        .enumerate()
    {
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
    }
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    let old = store
        .load()
        .unwrap()
        .heads
        .values()
        .find(|head| head.body.contains("before the upgrade"))
        .cloned()
        .unwrap();
    store
        .claim(crate::mailbox::Claim {
            claim_id: "legacy-claim".into(),
            recipient: old.recipient.clone(),
            stable_id: old.stable_id.clone(),
            revision: old.revision,
            digest: old.digest.clone(),
            execution: None,
        })
        .unwrap();
    store
        .resolve_claim(
            "legacy-claim",
            crate::mailbox::ClaimResolutionOutcome::Admitted,
        )
        .unwrap();
    let protocol = crate::mailbox_v1::PROTOCOL;
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": protocol}),
    )
    .unwrap();
    assert_ne!(
        claim["claim"]["stableId"],
        json!(old.stable_id),
        "the old admitted claim is never handed to the new Pi"
    );
    let snapshot = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": protocol}),
    )
    .unwrap();
    let state = snapshot["snapshot"]["headStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["stableId"] == json!(old.stable_id))
        .cloned()
        .unwrap();
    assert_eq!(state["claimExecution"], "other");
    assert_eq!(state["claimExecutionAlive"], false);
    assert_eq!(state["recoveryNeeded"], true);
    assert_ne!(
        snapshot["snapshot"]["claim"]["stableId"],
        json!(old.stable_id),
        "not presented as this Pi's current claim"
    );
    // It can be admitted by nobody here, only settled/retried/dropped.
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.resolve",
        json!({"protocol": protocol, "claimId": "legacy-claim", "outcome": "admitted"}),
    )
    .is_err());
    let retried = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.retry",
        json!({"protocol": protocol, "stableId": old.stable_id, "expectedRevision": old.revision}),
    )
    .unwrap();
    assert_eq!(retried["type"], "mailbox_retried");
}

/// PM requirement for option (A): the SENDER's cross-pane edit of its waiting
/// message goes through server-side routing (agent prompt --edit-pending),
/// not mailbox.*, so it keeps working; a third process claiming to be the
/// recipient on the main socket's mailbox.edit is refused with no effect.
#[tokio::test]
async fn cross_pane_edit_pending_works_and_a_process_posing_as_the_recipient_is_refused() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    // R is busy: it holds a claim, so S's next message waits.
    fixture
        .app
        .route_ordinary_send(&recipient, &sender, plain("first"), &Default::default())
        .unwrap();
    dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let waiting = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("draft"),
            &MessageSendOptions {
                send_new: true,
                ..Default::default()
            },
        )
        .unwrap();
    let crate::app::messages::SendRoute::Mailbox(waiting) = waiting else {
        panic!("queued")
    };
    let stable_id = waiting.stable_id.unwrap();
    let edited = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &sender,
            plain("final"),
            &MessageSendOptions {
                edit_pending: Some(stable_id.clone()),
                expect_revision: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
    let crate::app::messages::SendRoute::Mailbox(edited) = edited else {
        panic!("edited")
    };
    assert!(edited.edited);
    assert_eq!(edited.revision, Some(2));
    // A third process names R as the caller on the main socket.
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    let head = store.load().unwrap().heads[&stable_id].clone();
    let own = crate::platform::process_birth_identity(std::process::id()).unwrap();
    let response = fixture.app.handle_api_request(crate::api::schema::Request {
        id: "posing".into(),
        method: crate::api::schema::Method::MailboxEdit(crate::api::schema::MailboxEditParams {
            caller: recipient.clone(),
            grant_id: format!("offline:{recipient}:1"),
            recipient: crate::mailbox::RecipientKey {
                recipient_id: recipient.clone(),
                generation: "1".into(),
            },
            edit: crate::mailbox_v1::Edit {
                protocol: crate::mailbox_v1::PROTOCOL.into(),
                stable_id: stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                subject: "tampered".into(),
                body: "tampered".into(),
            },
            api_peer: Some(crate::api::schema::ApiPeer::Process {
                pid: 4_000_000_020,
                start_ticks: own.start_ticks,
            }),
        }),
    });
    assert!(
        response.contains("mailbox_caller_unauthenticated"),
        "{response}"
    );
    assert_eq!(store.load().unwrap().heads[&stable_id], head, "unchanged");
}

/// messages_capable is persisted at once (not only on the debounced save)
/// when the pane's first Messages stream attaches.
#[tokio::test]
async fn the_messages_capable_flip_is_saved_at_once() {
    let mut fixture = fixture();
    let session_path = fixture.directory.join("session.json");
    fixture.app.no_session = false;
    fixture.app.session_save_path = session_path.clone();
    assert!(!session_path.exists());
    attach_recipient(&mut fixture);
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&session_path).expect("saved at once")).unwrap();
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    let text = saved.to_string();
    assert!(text.contains(&queue_key), "the pane's queue key is saved");
    assert!(
        text.contains("\"messages_capable\":true") || text.contains("\"messagesCapable\":true"),
        "the flip is saved: {text}"
    );
}

/// A live Pi that has not attached Messages within 30 s of starting gets NEW
/// messages typed; before that, and in a pane with no Pi, they queue; a Pi
/// that attached once keeps queueing even while its stream is down.
#[tokio::test]
async fn a_live_pi_without_messages_after_30s_gets_new_messages_typed() {
    let mut fixture = fixture();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let key = fixture.terminals[1].clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .messages_capable = true;
    // No Pi in the pane (plain shell / restarting): queue.
    assert!(fixture.app.pane_takes_messages(&key));
    // An older Pi without Messages starts in the pane.
    let old_pi = 4_000_000_030_u32;
    let mut birth = crate::platform::process_birth_identity(std::process::id()).unwrap();
    birth.start_ticks = 11;
    fixture
        .app
        .mailbox_bootstrap_test_process_births
        .insert(old_pi, birth);
    fixture.app.install_mailbox_bootstrap_test_foreground_job(
        terminal_id.clone(),
        crate::platform::ForegroundJob {
            process_group_id: old_pi,
            processes: vec![crate::platform::ForegroundProcess {
                pid: old_pi,
                name: "pi".into(),
                argv0: None,
                argv: Some(vec!["pi".into()]),
                cmdline: Some("pi".into()),
            }],
        },
    );
    fixture
        .app
        .messages_test_process_ages
        .insert(old_pi, std::time::Duration::from_secs(5));
    assert!(
        fixture.app.pane_takes_messages(&key),
        "within 30 s it may still attach: queue"
    );
    fixture
        .app
        .route_ordinary_send(
            &key,
            &sender(&fixture),
            plain("queued early"),
            &Default::default(),
        )
        .unwrap();
    fixture
        .app
        .messages_test_process_ages
        .insert(old_pi, std::time::Duration::from_secs(31));
    assert!(
        !fixture.app.pane_takes_messages(&key),
        "no Messages after 30 s: new messages are typed"
    );
    let response = fixture.app.handle_agent_prompt(
        "typed".into(),
        AgentPromptParams {
            target: fixture.app.public_pane_id(1, fixture.panes[1]).unwrap(),
            text: "typed now".into(),
            wait: None,
            send: MessageSendOptions::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::AgentPrompted { delivery, .. } = success.result else {
        panic!("prompted")
    };
    // Typed, and the sender is told it went ahead of the queued message.
    let delivery = delivery.expect("typed-ahead delivery");
    assert_eq!(delivery.path, "pty");
    assert_eq!(delivery.typed_ahead_of_queued, Some(1));
    // The early head still waits in the queue.
    let heads = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads;
    assert_eq!(
        heads
            .values()
            .filter(|head| !head.stable_id.starts_with("typed."))
            .count(),
        1
    );
    assert!(
        heads
            .values()
            .any(|head| head.stable_id.starts_with("typed.")
                && head
                    .delivery
                    .as_ref()
                    .and_then(|d| d.typed_reason.as_deref())
                    == Some("fallback_30s")),
        "the typed fallback is in history"
    );
    // A Pi that attached once keeps queueing even with its stream down.
    fixture.app.messages_attached_pis.insert((old_pi, 11), None);
    assert!(fixture.app.pane_takes_messages(&key));
}

fn human_key(
    app: &mut App,
    terminal: &crate::terminal::TerminalId,
    code: crossterm::event::KeyCode,
) {
    let key = crate::input::TerminalKey::new(code, crossterm::event::KeyModifiers::NONE);
    app.note_human_key(terminal, &key);
}

fn deferral_events(app: &App, after: u64) -> Vec<crate::api::schema::EventData> {
    app.event_hub
        .events_after(after)
        .into_iter()
        .filter(|(_, event)| {
            matches!(
                event.event,
                crate::api::schema::EventKind::DeliveryDeferredDelivered
                    | crate::api::schema::EventKind::DeliveryDeferredFailed
            )
        })
        .map(|(_, event)| event.data)
        .collect()
}

fn prompt_pane(
    fixture: &mut Fixture,
    text: &str,
    transport: Option<MessageTransport>,
) -> serde_json::Value {
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let response = fixture.app.handle_agent_prompt(
        "p".into(),
        AgentPromptParams {
            target,
            text: text.into(),
            wait: None,
            send: MessageSendOptions {
                transport,
                ..Default::default()
            },
        },
    );
    serde_json::from_str(&response).unwrap()
}

/// On a Pi pane with Messages nothing is ever typed, even while the human
/// has a draft: the message is queued.
#[tokio::test]
async fn a_messages_pi_pane_is_never_typed_into() {
    let mut fixture = fixture();
    attach_recipient(&mut fixture);
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    human_key(
        &mut fixture.app,
        &terminal,
        crossterm::event::KeyCode::Char('d'),
    );
    let response = prompt_pane(&mut fixture, "queued", None);
    assert_eq!(
        response["result"]["delivery"]["path"], "mailbox",
        "{response}"
    );
    assert!(fixture.rx[1].try_recv().is_err(), "nothing typed");
}

/// Typed fallback: a human draft holds typed deliveries (auto and explicit
/// pty, in order) until Enter clears it; backspacing to empty is no draft;
/// API keys and typed deliveries never count as a draft.
#[tokio::test]
async fn typed_delivery_waits_for_the_humans_draft_then_types_in_order() {
    use crossterm::event::KeyCode;
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    // No Messages in this pane: sends are typed.
    assert!(!fixture.app.pane_takes_messages(&fixture.terminals[1]));
    // Typed and fully backspaced: Herdr cannot prove the editor empty, so
    // the draft stays possibly present until a submit/clear key.
    for _ in 0..3 {
        human_key(&mut fixture.app, &terminal, KeyCode::Char('x'));
    }
    for _ in 0..4 {
        human_key(&mut fixture.app, &terminal, KeyCode::Backspace);
    }
    assert!(fixture.app.pane_draft_pending(&terminal));
    let ctrl_u =
        crate::input::TerminalKey::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL);
    fixture.app.note_human_key(&terminal, &ctrl_u);
    assert!(!fixture.app.pane_draft_pending(&terminal));
    // API send-keys never counts as human input.
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    fixture.app.handle_agent_send_keys(
        "keys".into(),
        crate::api::schema::AgentSendKeysParams {
            target: target.clone(),
            keys: vec!["a".into(), "b".into()],
            expected_terminal_id: None,
            expected_name: None,
        },
    );
    assert!(!fixture.app.pane_draft_pending(&terminal));
    while fixture.rx[1].try_recv().is_ok() {}
    // The human starts a draft.
    human_key(&mut fixture.app, &terminal, KeyCode::Char('h'));
    human_key(&mut fixture.app, &terminal, KeyCode::Char('i'));
    assert!(fixture.app.pane_draft_pending(&terminal));
    let sequence = fixture.app.event_hub.current_sequence();
    let first = prompt_pane(&mut fixture, "first", None);
    let second = prompt_pane(&mut fixture, "second", Some(MessageTransport::Pty));
    for response in [&first, &second] {
        assert_eq!(
            response["result"]["delivery"]["path"], "pty_deferred",
            "{response}"
        );
        assert!(response["result"]["delivery"]["deferral_id"]
            .as_str()
            .unwrap()
            .starts_with("defer."));
    }
    assert!(
        fixture.rx[1].try_recv().is_err(),
        "nothing typed into the draft"
    );
    // Enter submits the human's draft; the held messages follow, in order.
    human_key(&mut fixture.app, &terminal, KeyCode::Enter);
    assert!(fixture.app.typed_deferrals.is_empty());
    let mut typed = Vec::new();
    while let Ok(Some(bytes)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), fixture.rx[1].recv()).await
    {
        typed.push(String::from_utf8_lossy(&bytes).to_string());
        if typed.iter().filter(|chunk| chunk.contains('\r')).count() >= 2 {
            break;
        }
    }
    let joined = typed.concat();
    let (a, b) = (
        joined.find("first").unwrap(),
        joined.find("second").unwrap(),
    );
    assert!(a < b, "in order: {joined:?}");
    let events = deferral_events(&fixture.app, sequence);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|event| matches!(
        event,
        crate::api::schema::EventData::DeliveryDeferredDelivered { .. }
    )));
    // The typed delivery itself did not create a draft.
    assert!(!fixture.app.pane_draft_pending(&terminal));
    let direct = prompt_pane(&mut fixture, "direct", None);
    assert_eq!(
        direct["result"]["delivery"]["path"], "pty",
        "typed at once: {direct}"
    );
    assert_eq!(direct["result"]["delivery"]["reason"], "no_messages");
    assert_eq!(direct["result"]["delivery"]["editable"], false);
}

/// After 10 minutes a held delivery fails with agent_input_busy; a sender
/// Pi with Messages gets the failure in its inbox. An agent process exit
/// clears the draft.
#[tokio::test]
async fn a_held_delivery_fails_after_the_limit_and_the_sender_is_told() {
    use crossterm::event::KeyCode;
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let sender_terminal = fixture.app.state.workspaces[0]
        .terminal_id(fixture.panes[0])
        .unwrap()
        .clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&sender_terminal)
        .unwrap()
        .messages_capable = true;
    human_key(&mut fixture.app, &terminal, KeyCode::Char('z'));
    let sequence = fixture.app.event_hub.current_sequence();
    let id = fixture.app.defer_typed_delivery(
        terminal.clone(),
        "never typed".into(),
        Agent::Pi,
        "recipient".into(),
        sender(&fixture),
        "agent_prompt",
        "no_messages".into(),
    );
    fixture.app.typed_deferrals[0].deadline =
        std::time::Instant::now() - std::time::Duration::from_secs(1);
    assert!(fixture.app.flush_typed_deferrals(std::time::Instant::now()));
    assert!(fixture.app.typed_deferrals.is_empty());
    assert!(fixture.rx[1].try_recv().is_err(), "never typed");
    let events = deferral_events(&fixture.app, sequence);
    let [crate::api::schema::EventData::DeliveryDeferredFailed {
        deferral_id,
        code,
        sender_terminal_id,
        ..
    }] = events.as_slice()
    else {
        panic!("one failure event: {events:?}")
    };
    assert_eq!(deferral_id, &id);
    assert_eq!(code, "agent_input_busy");
    assert_eq!(
        sender_terminal_id.as_deref(),
        Some(fixture.terminals[0].as_str())
    );
    let heads = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads;
    let note = heads
        .values()
        .find(|head| head.subject.starts_with("Not delivered"))
        .expect("failure note in the sender's inbox");
    assert!(note.body.contains("agent_input_busy") && note.body.contains("never typed"));
    // An agent process exit clears the draft.
    fixture
        .app
        .handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: fixture.panes[1],
            agent: None,
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });
    assert!(!fixture.app.pane_draft_pending(&terminal));
}

/// Human keys and pastes from an attached client reach the draft estimate.
#[tokio::test]
async fn client_keys_and_pastes_count_as_the_humans_draft() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut fixture = fixture();
    let focused = fixture.app.state.workspaces[0]
        .terminal_id(fixture.panes[0])
        .unwrap()
        .clone();
    let key = |code| {
        crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
            code,
            KeyModifiers::NONE,
        ))
    };
    fixture
        .app
        .route_client_events(vec![key(KeyCode::Char('a'))], false);
    assert!(fixture.app.pane_draft_pending(&focused));
    fixture
        .app
        .route_client_events(vec![key(KeyCode::Enter)], false);
    assert!(!fixture.app.pane_draft_pending(&focused));
    fixture.app.route_client_events(
        vec![crate::raw_input::RawInputEvent::Paste("pasted text".into())],
        false,
    );
    assert!(fixture.app.pane_draft_pending(&focused));
    let ctrl_u = crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ));
    fixture.app.route_client_events(vec![ctrl_u], false);
    assert!(!fixture.app.pane_draft_pending(&focused));
}

/// QA 2b #1: a gone Pi's claim that was only claimed (never admitted) can't
/// be settled as recovered; it needs Drop or Retry. An admitted one can.
#[tokio::test]
async fn recovered_settle_needs_an_admitted_claim() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let sender = sender(&fixture);
    for (index, body) in ["claimed only", "admitted"].iter().enumerate() {
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
    }
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    let recovered = store.load().unwrap();
    for (body, claim_id) in [
        ("claimed only", "gone-claimed"),
        ("admitted", "gone-admitted"),
    ] {
        let head = recovered
            .heads
            .values()
            .find(|head| head.body.contains(body))
            .cloned()
            .unwrap();
        store
            .claim(crate::mailbox::Claim {
                claim_id: claim_id.into(),
                recipient: head.recipient.clone(),
                stable_id: head.stable_id.clone(),
                revision: head.revision,
                digest: head.digest.clone(),
                execution: Some("pid:4000000050:1".into()),
            })
            .unwrap();
    }
    store
        .resolve_claim(
            "gone-admitted",
            crate::mailbox::ClaimResolutionOutcome::Admitted,
        )
        .unwrap();
    let protocol = crate::mailbox_v1::PROTOCOL;
    assert!(matches!(
        dispatch(
            &mut fixture.app,
            &session,
            "mailbox.resolve",
            json!({"protocol": protocol, "claimId": "gone-claimed", "outcome": "settled"}),
        ),
        Err(crate::app::MailboxBootstrapError::RecoveryNeedsDropOrRetry)
    ));
    assert!(!store
        .load()
        .unwrap()
        .resolutions
        .contains_key("gone-claimed"));
    let settled = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.resolve",
        json!({"protocol": protocol, "claimId": "gone-admitted", "outcome": "settled"}),
    )
    .unwrap();
    assert_eq!(settled["resolution"]["closedBy"], "recovered", "{settled}");
}

/// QA 2b #2: a pid execution counts as alive only while it is the pane's
/// foreground Pi and not stopped. A SIGSTOPped or no-longer-foreground old
/// Pi does not block Drop/Retry.
#[tokio::test]
async fn a_stopped_or_backgrounded_old_pi_is_not_alive() {
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let key = fixture.terminals[1].clone();
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = child.id();
    let ticks = crate::platform::process_birth_identity(pid)
        .unwrap()
        .start_ticks;
    let execution = format!("pid:{pid}:{ticks}");
    let foreground = |pid| crate::platform::ForegroundJob {
        process_group_id: pid,
        processes: vec![crate::platform::ForegroundProcess {
            pid,
            name: "pi".into(),
            argv0: None,
            argv: Some(vec!["pi".into()]),
            cmdline: Some("pi".into()),
        }],
    };
    fixture
        .app
        .install_mailbox_bootstrap_test_foreground_job(terminal.clone(), foreground(pid));
    assert!(
        fixture.app.execution_alive(&execution, &key),
        "running foreground Pi"
    );
    unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
    let stopped = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        crate::platform::process_stopped(pid)
    });
    assert!(stopped);
    assert!(!fixture.app.execution_alive(&execution, &key), "stopped");
    unsafe { libc::kill(pid as i32, libc::SIGCONT) };
    let resumed = (0..100).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(10));
        !crate::platform::process_stopped(pid)
    });
    assert!(resumed);
    assert!(fixture.app.execution_alive(&execution, &key));
    // Another process is now the pane's foreground Pi.
    fixture
        .app
        .install_mailbox_bootstrap_test_foreground_job(terminal, foreground(std::process::id()));
    assert!(
        !fixture.app.execution_alive(&execution, &key),
        "backgrounded"
    );
    let _ = child.kill();
    let _ = child.wait();
}

/// QA 2b #3b: a Pi that attached once but whose streams have been closed for
/// more than 30 s (its Messages extension died) gets new messages typed.
#[tokio::test]
async fn a_pi_whose_messages_streams_closed_over_30s_ago_gets_typed_input() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let key = fixture.terminals[1].clone();
    let pi = fixture.app.foreground_pi_identity(&key).expect("pane Pi");
    fixture
        .app
        .messages_test_process_ages
        .insert(pi.0, std::time::Duration::from_secs(300));
    assert!(fixture.app.pane_takes_messages(&key), "attached");
    fixture
        .app
        .release_mailbox_bootstrap_binding(&session.binding_generation);
    assert!(
        fixture.app.messages_attached_pis[&pi].is_some(),
        "closing time recorded"
    );
    assert!(
        fixture.app.pane_takes_messages(&key),
        "within 30 s: still queued"
    );
    fixture.app.messages_attached_pis.insert(
        pi,
        Some(std::time::Instant::now() - std::time::Duration::from_secs(31)),
    );
    assert!(
        !fixture.app.pane_takes_messages(&key),
        "streams closed for over 30 s: typed"
    );
    // Re-attaching clears it.
    attach_recipient(&mut fixture);
    assert!(fixture.app.pane_takes_messages(&key));
}

/// QA 2b #4: senders outside every pane are keyed per process identity, so
/// one external script cannot replace another's waiting message.
#[tokio::test]
async fn external_senders_cannot_replace_each_others_waiting_messages() {
    let mut fixture = fixture();
    attach_recipient(&mut fixture);
    let recipient = fixture.terminals[1].clone();
    let external = |key: &str| SenderAttribution {
        terminal: None,
        label: "external".into(),
        session: None,
        external_key: Some(key.into()),
    };
    let correlated = |body: &str, revision: u64| {
        let mut message = plain(body);
        message.correlation = Some(crate::mailbox::SendCorrelation {
            namespace: "ci".into(),
            key: "status".into(),
            revision,
        });
        message.replace_pending = true;
        message
    };
    let first = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &external("external:1000:sid:100:5"),
            correlated("script A", 1),
            &Default::default(),
        )
        .unwrap();
    let second = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &external("external:1000:sid:200:9"),
            correlated("script B", 1),
            &MessageSendOptions {
                send_new: true,
                ..Default::default()
            },
        )
        .unwrap();
    let (
        crate::app::messages::SendRoute::Mailbox(first),
        crate::app::messages::SendRoute::Mailbox(second),
    ) = (first, second)
    else {
        panic!("queued")
    };
    assert!(!second.edited, "B did not replace A's message");
    assert_ne!(first.stable_id, second.stable_id);
    let heads = crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads;
    assert!(heads.values().any(|head| head.body.contains("script A")));
    // The same script replaces its own.
    let again = fixture
        .app
        .route_ordinary_send(
            &recipient,
            &external("external:1000:sid:100:5"),
            correlated("script A v2", 2),
            &Default::default(),
        )
        .unwrap();
    let crate::app::messages::SendRoute::Mailbox(again) = again else {
        panic!("queued")
    };
    assert!(again.edited);
    assert_eq!(again.stable_id, first.stable_id);
    // Different login sessions give different keys.
    let own = crate::app::messages::external_sender_key(std::process::id());
    let mut child = std::process::Command::new("setsid")
        .args(["sleep", "5"])
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(100));
    let other = crate::app::messages::external_sender_key(child.id());
    assert!(own.starts_with("external:") && other.starts_with("external:"));
    assert_ne!(own, other);
    let _ = child.kill();
    let _ = child.wait();
}

fn report_editor(fixture: &mut Fixture, has_text: bool) {
    // Sampled strictly after any key the test sent before this report.
    std::thread::sleep(std::time::Duration::from_millis(2));
    report_editor_sampled(
        fixture,
        has_text,
        Some(crate::app::typed_deferral::unix_ms()),
    );
}

fn report_editor_sampled(fixture: &mut Fixture, has_text: bool, sampled_at_ms: Option<u64>) {
    let pane_id = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let response = fixture.app.handle_pane_report_agent(
        "editor".into(),
        crate::api::schema::PaneReportAgentParams {
            pane_id,
            source: "herdr:pi".into(),
            agent: "pi".into(),
            state: crate::api::schema::PaneAgentState::Idle,
            message: None,
            seq: None,
            agent_session_id: None,
            agent_session_path: None,
            editor_has_text: Some(has_text),
            editor_sampled_at_ms: sampled_at_ms,
        },
    );
    assert!(response.contains("\"result\""), "{response}");
}

/// QA4 draft race: a stale, in-flight Pi "false" (sampled before a newer
/// human key) must not clear Herdr's key-based draft flag; a report without
/// a sample never clears it; a false sampled after the key does.
#[tokio::test]
async fn a_stale_pi_editor_false_never_clears_a_newer_key_draft() {
    use crossterm::event::KeyCode;
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    // Pi's editor had text; Pi samples it empty at `stale` ...
    report_editor(&mut fixture, true);
    let stale = crate::app::typed_deferral::unix_ms();
    std::thread::sleep(std::time::Duration::from_millis(5));
    // ... the human types a new character before that report arrives ...
    human_key(&mut fixture.app, &terminal, KeyCode::Char('n'));
    assert!(fixture.app.pane_draft_pending(&terminal));
    // ... and the stale true→false edge arrives afterwards.
    report_editor_sampled(&mut fixture, false, Some(stale));
    assert!(
        fixture.app.pane_draft_pending(&terminal),
        "a stale false must not clear the newer key draft"
    );
    let held = prompt_pane(&mut fixture, "never over the new draft", None);
    assert_eq!(held["result"]["delivery"]["path"], "pty_deferred", "{held}");
    assert!(fixture.rx[1].try_recv().is_err(), "nothing typed");
    // A report without a sample (an older Pi asset) never clears it either.
    report_editor_sampled(&mut fixture, true, None);
    report_editor_sampled(&mut fixture, false, None);
    assert!(fixture.app.pane_draft_pending(&terminal));
    // A true→false edge sampled after the key clears it; the held message
    // is typed.
    report_editor(&mut fixture, true);
    report_editor(&mut fixture, false);
    assert!(!fixture.app.pane_draft_pending(&terminal));
    let typed = tokio::time::timeout(std::time::Duration::from_secs(2), fixture.rx[1].recv())
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&typed).contains("never over the new draft"));
}

/// Pi's own `editor_has_text` report wins over Herdr's input count for a
/// Pi pane, and is shown on agent get and pane get.
#[tokio::test]
async fn pis_editor_has_text_report_wins_over_the_count() {
    use crossterm::event::KeyCode;
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    // Pi says its editor has text; Herdr counted nothing.
    report_editor(&mut fixture, true);
    assert!(fixture.app.pane_draft_pending(&terminal));
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let got = fixture.app.handle_agent_get(
        "get".into(),
        crate::api::schema::AgentTarget {
            target: target.clone(),
        },
    );
    let got: serde_json::Value = serde_json::from_str(&got).unwrap();
    assert_eq!(got["result"]["agent"]["editor_has_text"], true, "{got}");
    let pane = fixture.app.pane_info(1, fixture.panes[1]).unwrap();
    assert_eq!(pane.editor_has_text, Some(true));
    let held = prompt_pane(&mut fixture, "held by Pi's flag", None);
    assert_eq!(held["result"]["delivery"]["path"], "pty_deferred", "{held}");
    // Pi reports the editor clear: the held message is typed at once.
    report_editor(&mut fixture, false);
    assert!(fixture.app.typed_deferrals.is_empty());
    let typed = tokio::time::timeout(std::time::Duration::from_secs(2), fixture.rx[1].recv())
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&typed).contains("held by Pi's flag"));
    // QA batch 3 #3: a stale Pi "false" does not override fresh keys (OR)...
    human_key(&mut fixture.app, &terminal, KeyCode::Char('q'));
    assert!(fixture.app.pane_draft_pending(&terminal));
    // ...a repeated false (no edge) leaves Herdr's flag...
    report_editor(&mut fixture, false);
    assert!(fixture.app.pane_draft_pending(&terminal));
    // ...and Pi's true→false edge clears it.
    report_editor(&mut fixture, true);
    assert!(fixture.app.pane_draft_pending(&terminal));
    report_editor(&mut fixture, false);
    assert!(!fixture.app.pane_draft_pending(&terminal));
    // The report is dropped when the agent process exits.
    report_editor(&mut fixture, true);
    fixture
        .app
        .handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: fixture.panes[1],
            agent: None,
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: std::time::Instant::now(),
        });
    assert_eq!(fixture.app.state.terminals[&terminal].editor_has_text, None);
}

/// Draft defect: a pane that takes Messages is never typed into. Text that
/// Messages cannot carry (oversize body, multi-line or oversize structured
/// subject) is refused, not typed over the human's unsent editor draft.
#[tokio::test]
async fn a_messages_pi_is_never_typed_into_even_for_text_messages_cannot_carry() {
    let mut fixture = fixture();
    let terminal_id = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    fixture
        .app
        .state
        .terminals
        .get_mut(&terminal_id)
        .unwrap()
        .set_detected_state(Some(Agent::Pi), AgentState::Idle);
    let _session = attach_recipient(&mut fixture);
    let target = fixture.app.public_pane_id(1, fixture.panes[1]).unwrap();
    let oversize = "x".repeat(16 * 1024 + 1);
    let control = "bell \u{7} here".to_string();
    for text in [oversize, control] {
        let response: serde_json::Value = serde_json::from_str(&fixture.app.handle_agent_prompt(
            "req".into(),
            AgentPromptParams {
                target: target.clone(),
                text,
                wait: None,
                send: MessageSendOptions::default(),
            },
        ))
        .unwrap();
        assert_eq!(
            response["error"]["code"], "messages_unavailable",
            "{response}"
        );
    }
    assert!(
        fixture.rx[1].try_recv().is_err(),
        "nothing was typed into the Messages Pi's editor"
    );
    assert!(crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads
        .is_empty());
}

/// VQRO 1a: a bracketed paste ending in a newline stays in the unsent
/// draft; Esc and a modified Enter do not prove the editor empty; only a
/// real, unmodified Enter clears.
#[tokio::test]
async fn only_a_real_enter_clears_the_draft() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut fixture = fixture();
    let focused = fixture.app.state.workspaces[0]
        .terminal_id(fixture.panes[0])
        .unwrap()
        .clone();
    let key = |code, modifiers| {
        crate::raw_input::RawInputEvent::Key(crate::input::TerminalKey::new(code, modifiers))
    };
    fixture.app.route_client_events(
        vec![crate::raw_input::RawInputEvent::Paste(
            "pasted line\n".into(),
        )],
        false,
    );
    assert!(
        fixture.app.pane_draft_pending(&focused),
        "paste ending in a newline"
    );
    for (code, modifiers) in [
        (KeyCode::Esc, KeyModifiers::NONE),
        (KeyCode::Enter, KeyModifiers::SHIFT),
        (KeyCode::Enter, KeyModifiers::ALT),
        (KeyCode::Char('j'), KeyModifiers::CONTROL),
    ] {
        fixture
            .app
            .route_client_events(vec![key(code, modifiers)], false);
        assert!(
            fixture.app.pane_draft_pending(&focused),
            "{code:?} {modifiers:?} must not clear the draft"
        );
    }
    fixture
        .app
        .route_client_events(vec![key(KeyCode::Enter, KeyModifiers::NONE)], false);
    assert!(!fixture.app.pane_draft_pending(&focused));
    // A pasted lone newline is still unsent input.
    fixture.app.route_client_events(
        vec![crate::raw_input::RawInputEvent::Paste("\n".into())],
        false,
    );
    assert!(fixture.app.pane_draft_pending(&focused));
}

/// VQRO 1b: a held message is pinned to the recipient's execution. If
/// another agent of the same kind replaced it by the time the draft clears,
/// the message fails with agent_replaced (sender told) and is never typed.
#[tokio::test]
async fn a_held_message_is_never_typed_into_a_replacement_agent() {
    use crossterm::event::KeyCode;
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let pi = |pid: u32| crate::platform::ForegroundJob {
        process_group_id: pid,
        processes: vec![crate::platform::ForegroundProcess {
            pid,
            name: "pi".into(),
            argv0: None,
            argv: Some(vec!["pi".into()]),
            cmdline: Some("pi".into()),
        }],
    };
    let mut birth = crate::platform::process_birth_identity(std::process::id()).unwrap();
    for (pid, ticks) in [(4_000_000_060_u32, 21_u64), (4_000_000_061, 22)] {
        birth.start_ticks = ticks;
        fixture
            .app
            .mailbox_bootstrap_test_process_births
            .insert(pid, birth);
    }
    fixture
        .app
        .install_mailbox_bootstrap_test_foreground_job(terminal.clone(), pi(4_000_000_060));
    human_key(&mut fixture.app, &terminal, KeyCode::Char('d'));
    let sequence = fixture.app.event_hub.current_sequence();
    let held = prompt_pane(&mut fixture, "for the original Pi", None);
    assert_eq!(held["result"]["delivery"]["path"], "pty_deferred", "{held}");
    // The Pi is replaced by another Pi in the same pane.
    fixture
        .app
        .install_mailbox_bootstrap_test_foreground_job(terminal.clone(), pi(4_000_000_061));
    human_key(&mut fixture.app, &terminal, KeyCode::Enter);
    assert!(fixture.app.typed_deferrals.is_empty());
    assert!(
        fixture.rx[1].try_recv().is_err(),
        "never typed into the new agent"
    );
    let events = deferral_events(&fixture.app, sequence);
    assert!(
        matches!(
            events.as_slice(),
            [crate::api::schema::EventData::DeliveryDeferredFailed { code, .. }] if code == "agent_replaced"
        ),
        "{events:?}"
    );
    // The same execution still gets its held message.
    human_key(&mut fixture.app, &terminal, KeyCode::Char('e'));
    let held = prompt_pane(&mut fixture, "for the same Pi", None);
    assert_eq!(held["result"]["delivery"]["path"], "pty_deferred");
    human_key(&mut fixture.app, &terminal, KeyCode::Enter);
    let typed = tokio::time::timeout(std::time::Duration::from_secs(2), fixture.rx[1].recv())
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&typed).contains("for the same Pi"));
}

/// VQRO 2: every send result says how it was delivered and why, and typed
/// deliveries leave a settled, never-claimable history row
/// (closedBy "typed", delivery.typedReason) in the pane's queue.
#[tokio::test]
async fn every_send_reports_its_method_and_typed_sends_leave_history() {
    let mut fixture = fixture();
    // Typed: the pane has no Messages yet.
    let typed = prompt_pane(&mut fixture, "typed hello", None);
    assert_eq!(typed["result"]["delivery"]["path"], "pty", "{typed}");
    assert_eq!(typed["result"]["delivery"]["reason"], "no_messages");
    let explicit = prompt_pane(
        &mut fixture,
        "typed on purpose",
        Some(MessageTransport::Pty),
    );
    assert_eq!(explicit["result"]["delivery"]["reason"], "explicit_pty");
    // Queued once the pane's Pi attached Messages.
    let session = attach_recipient(&mut fixture);
    let queued = prompt_pane(&mut fixture, "queued hello", None);
    assert_eq!(queued["result"]["delivery"]["path"], "mailbox", "{queued}");
    assert_eq!(queued["result"]["delivery"]["reason"], "messages");
    assert_eq!(queued["result"]["delivery"]["editable"], true);
    // History shows the typed ones as closed rows; they are never claimed.
    let snapshot = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    let heads = snapshot["snapshot"]["heads"].as_array().unwrap();
    let typed_rows: Vec<_> = heads
        .iter()
        .filter(|head| head["stableId"].as_str().unwrap().starts_with("typed."))
        .collect();
    assert_eq!(typed_rows.len(), 2, "{snapshot}");
    let reasons: Vec<_> = typed_rows
        .iter()
        .map(|head| head["delivery"]["typedReason"].as_str().unwrap())
        .collect();
    assert!(reasons.contains(&"no_messages") && reasons.contains(&"explicit_pty"));
    for row in &typed_rows {
        let state = snapshot["snapshot"]["headStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["stableId"] == row["stableId"])
            .unwrap();
        assert_eq!(state["lifecycle"], "settled");
        assert_eq!(state["closedBy"], "typed");
        // An admitted receipt with the row's exact revision and digest.
        let receipt = snapshot["snapshot"]["receipts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|receipt| receipt["stableId"] == row["stableId"])
            .expect("typed row has a receipt");
        assert_eq!(receipt["status"], "admitted");
        assert_eq!(receipt["revision"], row["revision"]);
        assert_eq!(receipt["digest"], row["digest"]);
        assert_eq!(receipt["deliveryDigest"], row["deliveryDigest"]);
        // QA4: the row keeps its own typed: claimId, so Pi verifies it.
        assert_eq!(
            state["claimId"],
            format!("typed:{}", row["stableId"].as_str().unwrap()),
            "{state}"
        );
        assert!(state.get("recoveryNeeded").is_none());
    }
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": crate::mailbox_v1::PROTOCOL}),
    )
    .unwrap();
    assert!(
        claim["claim"]["stableId"]
            .as_str()
            .unwrap()
            .starts_with("send."),
        "only the queued message is claimable: {claim}"
    );
    let history = fixture
        .app
        .dispatch_mailbox_bootstrap(
            &session,
            "mailbox.history_snapshot",
            json!({"protocol": crate::mailbox_v1::PROTOCOL}),
        )
        .expect("history");
    assert!(
        history.to_string().contains("\"closedBy\":\"typed\""),
        "{history}"
    );
}

/// A pane that takes Messages is never typed into: oversize text is refused
/// with messages_unavailable and reason "oversized"; nothing is typed or
/// queued.
#[tokio::test]
async fn oversize_text_to_a_messages_pane_is_refused_not_typed() {
    let mut fixture = fixture();
    attach_recipient(&mut fixture);
    let big = "x".repeat(20 * 1024);
    let refused = prompt_pane(&mut fixture, &big, None);
    assert_eq!(
        refused["error"]["code"], "messages_unavailable",
        "{refused}"
    );
    assert_eq!(refused["error"]["reason"], "oversized");
    assert!(fixture.rx[1].try_recv().is_err(), "nothing typed");
    assert!(crate::mailbox::MailboxStore::open(&fixture.directory)
        .unwrap()
        .load()
        .unwrap()
        .heads
        .is_empty());
}

/// QA batch 3, 1b+: a held message is also pinned to the addressed agent
/// name and agent session; a rename or a new session fails it with
/// agent_replaced instead of typing.
#[tokio::test]
async fn a_held_message_fails_when_the_agent_name_or_session_changes() {
    use crossterm::event::KeyCode;
    for change in ["name", "session"] {
        let mut fixture = fixture();
        let terminal = fixture.app.state.workspaces[1]
            .terminal_id(fixture.panes[1])
            .unwrap()
            .clone();
        fixture
            .app
            .state
            .terminals
            .get_mut(&terminal)
            .unwrap()
            .set_agent_name("worker".into());
        human_key(&mut fixture.app, &terminal, KeyCode::Char('d'));
        let sequence = fixture.app.event_hub.current_sequence();
        let held = prompt_pane(&mut fixture, "for worker", None);
        assert_eq!(held["result"]["delivery"]["path"], "pty_deferred", "{held}");
        if change == "name" {
            fixture
                .app
                .state
                .terminals
                .get_mut(&terminal)
                .unwrap()
                .set_agent_name("someone-else".into());
        } else {
            report_session(
                &mut fixture.app,
                1,
                fixture.panes[1],
                "/sessions/s1-new.jsonl",
                99,
            );
        }
        human_key(&mut fixture.app, &terminal, KeyCode::Enter);
        assert!(fixture.rx[1].try_recv().is_err(), "{change}: never typed");
        let events = deferral_events(&fixture.app, sequence);
        assert!(
            matches!(
                events.as_slice(),
                [crate::api::schema::EventData::DeliveryDeferredFailed { code, .. }] if code == "agent_replaced"
            ),
            "{change}: {events:?}"
        );
    }
}

/// QA batch 3, 1a+: keys that can insert text without a printable key
/// (history recall, Tab completion, Ctrl-R, Ctrl-Y, Backspace) mark the
/// draft; Ctrl-J and Ctrl-M are not submits; only plain Enter, Ctrl-C and
/// Ctrl-U clear.
#[tokio::test]
async fn any_forwarded_key_marks_a_possible_draft() {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut fixture = fixture();
    let terminal = fixture.app.state.workspaces[1]
        .terminal_id(fixture.panes[1])
        .unwrap()
        .clone();
    let press = |app: &mut App, code, modifiers| {
        app.note_human_key(&terminal, &crate::input::TerminalKey::new(code, modifiers));
    };
    for (code, modifiers) in [
        (KeyCode::Up, KeyModifiers::NONE),
        (KeyCode::Tab, KeyModifiers::NONE),
        (KeyCode::Char('r'), KeyModifiers::CONTROL),
        (KeyCode::Char('y'), KeyModifiers::CONTROL),
        (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Char('j'), KeyModifiers::CONTROL),
        (KeyCode::Char('m'), KeyModifiers::CONTROL),
        (KeyCode::Esc, KeyModifiers::NONE),
    ] {
        press(&mut fixture.app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!fixture.app.pane_draft_pending(&terminal));
        press(&mut fixture.app, code, modifiers);
        assert!(
            fixture.app.pane_draft_pending(&terminal),
            "{code:?} {modifiers:?} marks a possible draft"
        );
    }
    for (code, modifiers) in [
        (KeyCode::Enter, KeyModifiers::NONE),
        (KeyCode::Char('c'), KeyModifiers::CONTROL),
        (KeyCode::Char('u'), KeyModifiers::CONTROL),
    ] {
        press(&mut fixture.app, KeyCode::Up, KeyModifiers::NONE);
        press(&mut fixture.app, code, modifiers);
        assert!(
            !fixture.app.pane_draft_pending(&terminal),
            "{code:?} {modifiers:?} clears"
        );
    }
}

/// QA re-check: a crash between the typed history appends never turns the
/// row into a deliverable or recoverable message. Simulated by writing the
/// head alone, and the head plus its claim without a resolution.
#[tokio::test]
async fn a_partly_written_typed_history_row_is_never_delivered_or_recovered() {
    let mut fixture = fixture();
    let session = attach_recipient(&mut fixture);
    let queue_key = fixture.app.pane_queue_key(&fixture.terminals[1]).unwrap();
    let store = crate::mailbox::MailboxStore::open(&fixture.app.sender_authority_dir).unwrap();
    let typed_head = |stable_id: &str| crate::mailbox::MailboxHead {
        delivery: Some(crate::mailbox::ServerDelivery {
            origin: "agent_prompt".into(),
            sender_label: "tpm".into(),
            sender_session: None,
            recipient_session: None,
            correlation: None,
            retry_of: None,
            typed_reason: Some("no_messages".into()),
        }),
        ..scoped_test_head(
            stable_id,
            "term_s",
            &crate::app::messages::pane_recipient(&queue_key),
            'e',
        )
    };
    // Crash after the head (and its receipt) alone.
    let head_only = typed_head("typed.headonly");
    store.append_offline_head(head_only.clone()).unwrap();
    // Crash after the claim, before the settled resolution.
    let with_claim = typed_head("typed.withclaim");
    store.append_offline_head(with_claim.clone()).unwrap();
    store
        .claim(crate::mailbox::Claim {
            claim_id: "typed:typed.withclaim".into(),
            recipient: with_claim.recipient.clone(),
            stable_id: with_claim.stable_id.clone(),
            revision: 1,
            digest: with_claim.digest.clone(),
            execution: None,
        })
        .unwrap();
    let protocol = crate::mailbox_v1::PROTOCOL;
    let claim = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.claim",
        json!({"protocol": protocol}),
    )
    .unwrap();
    assert!(claim["claim"].is_null(), "never delivered: {claim}");
    let snapshot = dispatch(
        &mut fixture.app,
        &session,
        "mailbox.snapshot",
        json!({"protocol": protocol}),
    )
    .unwrap();
    assert!(
        !snapshot.to_string().contains("\"recoveryNeeded\":true"),
        "never offered for recovery: {snapshot}"
    );
    // Projection: both torn rows read as settled, closedBy "typed", never
    // claimable and no held row, in snapshot and history. A row whose
    // `typed:` claim record survived keeps that claimId (QA4), so clients
    // can verify it; a head-only row has none.
    for (stable_id, claim_id) in [
        ("typed.headonly", None),
        ("typed.withclaim", Some("typed:typed.withclaim")),
    ] {
        let state = snapshot["snapshot"]["headStates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|state| state["stableId"] == stable_id)
            .unwrap_or_else(|| panic!("{stable_id} listed: {snapshot}"));
        assert_eq!(state["lifecycle"], "settled", "{state}");
        assert_eq!(state["closedBy"], "typed");
        assert_eq!(
            state.get("claimId").and_then(|v| v.as_str()),
            claim_id,
            "{state}"
        );
        assert!(state.get("claimExecution").is_none(), "{state}");
    }
    assert!(snapshot["snapshot"]
        .get("claim")
        .is_none_or(|claim| claim.is_null()));
    let history = fixture
        .app
        .dispatch_mailbox_bootstrap(
            &session,
            "mailbox.history_snapshot",
            json!({"protocol": protocol}),
        )
        .expect("history")
        .to_string();
    for stable_id in ["typed.headonly", "typed.withclaim"] {
        assert!(
            history.contains(stable_id),
            "{stable_id} in history: {history}"
        );
    }
    assert!(dispatch(
        &mut fixture.app,
        &session,
        "mailbox.retry",
        json!({"protocol": protocol, "stableId": "typed.withclaim", "expectedRevision": 1}),
    )
    .is_err());
    assert_eq!(
        fixture.app.unsettled_queue_len(&fixture.terminals[1]),
        0,
        "not counted as queued"
    );
    // The single-lock writer refuses a non-typed head and writes a full row.
    let full = typed_head("typed.full");
    store.append_typed_history(full).unwrap();
    let recovered = store.load().unwrap();
    let resolution = &recovered.resolutions["typed:typed.full"];
    assert_eq!(resolution.closed_by.as_deref(), Some("typed"));
    assert!(recovered
        .receipts
        .values()
        .any(|r| r.stable_id == "typed.full"));
    let mut plain = scoped_test_head(
        "send.plain",
        "term_s",
        &crate::app::messages::pane_recipient(&queue_key),
        'f',
    );
    plain.delivery = None;
    assert!(store.append_typed_history(plain).is_err());
}
