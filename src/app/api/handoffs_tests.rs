use bytes::Bytes;

use crate::{
    agent_resume::{AgentSessionRef, AgentSessionRefKind},
    api::schema::{
        CanonicalHerdrIdentity, HandoffKind, HandoffSendParams, HandoffTransportOutcome,
        HerdrHandoff, ResponseResult, SuccessResponse,
    },
    app::{App, Mode},
    config::Config,
    detect::{Agent, AgentState},
    workspace::Workspace,
};

fn app_with_session(
    value: &str,
) -> (
    App,
    crate::layout::PaneId,
    tokio::sync::mpsc::Receiver<Bytes>,
) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("handoff")];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    app.state.selected = 0;
    app.state.mode = Mode::Terminal;
    let pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
    let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane]
        .attached_terminal_id
        .clone();
    let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
    terminal.set_agent_name("agent".into());
    terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
    terminal.set_agent_session_ref(
        "test".into(),
        "pi".into(),
        Some(AgentSessionRef {
            kind: AgentSessionRefKind::Id,
            value: value.into(),
        }),
        Some(7),
    );
    let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 4);
    app.state.insert_test_runtime(pane, runtime);
    (app, pane, rx)
}

fn identity(app: &App, pane: crate::layout::PaneId) -> CanonicalHerdrIdentity {
    let agent = app.agent_info(0, pane).unwrap();
    CanonicalHerdrIdentity {
        workspace_id: agent.workspace_id,
        pane_id: agent.pane_id,
        terminal_id: agent.terminal_id,
        agent_session: agent.agent_session.unwrap(),
    }
}

fn envelope(identity: CanonicalHerdrIdentity) -> HerdrHandoff {
    HerdrHandoff {
        version: 1,
        message_id: "message-1".into(),
        created_at: "unix:1".into(),
        sender: identity.clone(),
        recipient: identity,
        kind: HandoffKind::Info,
        correlation_id: None,
        reply_to_id: None,
        task: None,
        summary: "bounded message".into(),
        artifact_refs: vec![],
    }
}

#[tokio::test]
async fn exact_session_handoff_uses_normal_prompt_transaction() {
    let (mut app, pane, mut rx) = app_with_session("session-1");
    let response = app.handle_handoff_send(
        "req".into(),
        HandoffSendParams {
            envelope: envelope(identity(&app, pane)),
            send: Default::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::HandoffTransport { receipt } = success.result else {
        panic!("expected receipt")
    };
    assert_eq!(
        receipt.outcome,
        HandoffTransportOutcome::RuntimeTransactionAdmitted
    );
    let text = rx.recv().await.unwrap();
    assert!(String::from_utf8_lossy(&text).contains("HERDR HANDOFF v1"));
    assert_eq!(rx.recv().await.unwrap(), Bytes::from_static(b"\r"));
}

#[tokio::test]
async fn replacement_session_is_rejected_without_writing() {
    let (mut app, pane, mut rx) = app_with_session("session-1");
    let current = identity(&app, pane);
    let mut stale = envelope(current);
    stale.recipient.agent_session.value = "replaced-session".into();
    let response = app.handle_handoff_send(
        "req".into(),
        HandoffSendParams {
            envelope: stale,
            send: Default::default(),
        },
    );
    let success: SuccessResponse = serde_json::from_str(&response).unwrap();
    let ResponseResult::HandoffTransport { receipt } = success.result else {
        panic!("expected receipt")
    };
    assert_eq!(
        receipt.outcome,
        HandoffTransportOutcome::RecipientIdentityMismatch
    );
    assert!(rx.try_recv().is_err());
}

// #173: a Pi started without a trusted managed launch (hand-typed, `agent
// start` without --session, Collection helper) keeps the session its
// herdr-agent-state integration reports, as herdr 0.8.4 did. It is usable for
// identity matching only and never grants mailbox, route or report authority.
fn reported_pi_pair() -> (
    App,
    Vec<crate::layout::PaneId>,
    Vec<tokio::sync::mpsc::Receiver<Bytes>>,
) {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        true,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.workspaces = vec![Workspace::test_new("parent"), Workspace::test_new("child")];
    app.state.ensure_test_terminals();
    app.state.active = Some(0);
    app.state.selected = 0;
    app.state.mode = Mode::Terminal;
    let mut panes = Vec::new();
    let mut receivers = Vec::new();
    for ws in 0..2 {
        let pane = app.state.workspaces[ws].tabs[0].root_pane.unwrap();
        let terminal_id = app.state.workspaces[ws].tabs[0].panes[&pane]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, rx) = crate::terminal::TerminalRuntime::test_with_channel_capacity(80, 24, 8);
        app.state.insert_test_runtime(pane, runtime);
        let public = app.public_pane_id(ws, pane).unwrap();
        app.handle_pane_report_agent_session(
            "report".into(),
            crate::api::schema::PaneReportAgentSessionParams {
                pane_id: public,
                source: "herdr:pi".into(),
                agent: "pi".into(),
                seq: Some(10 + ws as u64),
                agent_session_id: None,
                agent_session_path: Some(format!(
                    "/home/user/.pi/agent/sessions/x/reported-{ws}.jsonl"
                )),
                session_start_source: Some("startup".into()),
            },
        );
        panes.push(pane);
        receivers.push(rx);
    }
    (app, panes, receivers)
}

fn identity_at(app: &App, ws: usize, pane: crate::layout::PaneId) -> CanonicalHerdrIdentity {
    let agent = app.agent_info(ws, pane).unwrap();
    CanonicalHerdrIdentity {
        workspace_id: agent.workspace_id,
        pane_id: agent.pane_id,
        terminal_id: agent.terminal_id,
        agent_session: agent.agent_session.expect("reported Pi session is visible"),
    }
}

#[tokio::test]
async fn hand_typed_pi_reported_session_is_visible_marked_reported_and_handoffs_match() {
    let (mut app, panes, mut rx) = reported_pi_pair();
    let parent = app.agent_info(0, panes[0]).unwrap();
    let session = parent
        .agent_session
        .clone()
        .expect("reported session shown");
    assert_eq!(session.source, "herdr:pi");
    assert_eq!(session.agent, "pi");
    assert_eq!(session.kind, AgentSessionRefKind::Path);
    assert_eq!(
        session.value,
        "/home/user/.pi/agent/sessions/x/reported-0.jsonl"
    );
    assert_eq!(
        parent.agent_session_trust,
        Some(crate::api::schema::AgentSessionTrust::Reported)
    );
    for (from, to) in [(0usize, 1usize), (1, 0)] {
        let mut envelope = envelope(identity_at(&app, from, panes[from]));
        envelope.recipient = identity_at(&app, to, panes[to]);
        envelope.message_id = format!("m-{from}-{to}");
        let response = app.handle_handoff_send(
            "req".into(),
            HandoffSendParams {
                envelope,
                send: Default::default(),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::HandoffTransport { receipt } = success.result else {
            panic!("expected receipt")
        };
        assert_eq!(
            receipt.outcome,
            HandoffTransportOutcome::RuntimeTransactionAdmitted,
            "handoff {from}->{to}"
        );
        let text = rx[to].recv().await.unwrap();
        assert!(String::from_utf8_lossy(&text).contains("HERDR HANDOFF v1"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reported_only_pi_session_never_grants_mailbox_route_or_report_authority() {
    use std::os::fd::AsRawFd;
    let (mut app, panes, _rx) = reported_pi_pair();
    let terminals: Vec<_> = (0..2)
        .map(|ws| {
            app.state.workspaces[ws]
                .terminal_id(panes[ws])
                .unwrap()
                .clone()
        })
        .collect();
    for (ws, terminal_id) in terminals.iter().enumerate() {
        assert!(app
            .agent_info(ws, panes[ws])
            .unwrap()
            .agent_session
            .is_some());
        assert!(
            app.trusted_managed_pi_session(app.state.terminals.get(terminal_id).unwrap())
                .is_none(),
            "a report is never a trusted managed identity"
        );
    }
    // Bound route: a delegation edge between two reported-only Pis is not ready.
    let parent = app
        .state
        .delegations
        .create(Some(panes[0]), None, None)
        .unwrap();
    let child = app
        .state
        .delegations
        .create(Some(panes[1]), Some(parent), None)
        .unwrap();
    assert!(app.ready_route_shape(child, parent).is_none());
    // Bound report: no legacy child/parent report identity exists for them.
    assert!(app
        .legacy_child_parent_report_identity(&terminals[1].to_string(), &terminals[0].to_string())
        .is_none());
    // Mailbox bootstrap trust: nothing to accept for a reported-only Pi.
    let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(matches!(
        app.accept_mailbox_bootstrap_stream(stream.as_raw_fd()),
        Err(crate::app::mailbox::MailboxBootstrapError::GrantMissing)
    ));
}
