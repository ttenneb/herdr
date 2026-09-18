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
    let response = app.handle_handoff_send("req".into(), HandoffSendParams { envelope: stale });
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
