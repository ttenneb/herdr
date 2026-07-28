use super::*;
use crate::api::schema::PluginInvocationContext;

fn context() -> PluginInvocationContext {
    PluginInvocationContext {
        workspace_resource: None,
        repository_id: None,
        checkout: None,
        workspace_id: Some("workspace-exact".into()),
        workspace_label: None,
        workspace_cwd: None,
        worktree: None,
        tab_id: None,
        tab_label: None,
        focused_pane_id: None,
        focused_pane_cwd: None,
        focused_pane_agent: None,
        focused_pane_status: None,
        selected_text: None,
        invocation_source: Some("test".into()),
        correlation_id: Some("captured".into()),
        clicked_url: None,
        link_handler_id: None,
    }
}

fn test_app() -> App {
    let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(
        &crate::config::Config::default(),
        true,
        None,
        rx,
        crate::api::EventHub::default(),
    )
}

fn plugin(root: &std::path::Path) -> InstalledPluginInfo {
    std::fs::write(
        root.join("herdr-plugin.toml"),
        r#"
id = "example.choices-runtime"
name = "Choices runtime"
version = "0.1.0"
min_herdr_version = "0.7.0"
platforms = ["linux", "macos"]
"#,
    )
    .unwrap();
    super::super::load_plugin_manifest(&root.display().to_string(), true).unwrap()
}

fn root(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "herdr-{name}-{}-{}",
        std::process::id(),
        current_unix_ms()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn shell(script: &str) -> ChoicesProviderCompletion {
    let cwd = std::env::current_dir().expect("current directory");
    let mut command =
        crate::plugin_command::command_for_argv_in_dir("sh", &["-c".into(), script.into()], &cwd);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_choices_provider(
        command,
        &crate::app::PluginChoiceProviderCancellation::new(),
    )
}

struct ProcessGuard(u32);

impl ProcessGuard {
    fn from_pid_file(path: &std::path::Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(pid) = std::fs::read_to_string(path) {
                return Self(pid.trim().parse().unwrap());
            }
            assert!(
                Instant::now() < deadline,
                "descendant pid file was not written"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_gone(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_exists(self.0) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !process_exists(self.0),
            "descendant {} survived cleanup",
            self.0
        );
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.0 as i32, libc::SIGKILL);
        }
    }
}

fn process_exists(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn handle_provider_events_until_idle(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.state.plugin_action_choices_providers_in_flight > 0 {
        match app.event_rx.try_recv() {
            Ok(event) => app.handle_internal_event(event),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                assert!(Instant::now() < deadline, "provider cleanup did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                panic!("provider event channel disconnected")
            }
        }
    }
}

#[test]
fn provider_success_malformed_nonzero_and_spawn_failure() {
    let valid = shell(
        r#"printf '%s' '{"version":1,"choices":[{"id":"one","label":"One","payload":null}]}'"#,
    );
    assert_eq!(valid.result.unwrap().choices[0].id, "one");
    assert!(shell("printf bad")
        .result
        .unwrap_err()
        .contains("invalid choices JSON"));
    let failed = shell("printf diagnostic >&2; exit 7");
    assert_eq!(failed.exit_code, Some(7));
    assert!(failed.result.unwrap_err().contains("status 7"));
    assert_eq!(failed.stderr, "diagnostic");

    let oversized_stderr = shell("head -c 20000 /dev/zero >&2; exit 1");
    assert!(oversized_stderr.stderr.len() < 17 * 1024);
    assert!(oversized_stderr
        .stderr
        .contains("truncated plugin output after 16384 bytes"));

    let oversized_stdout = shell("head -c 70000 /dev/zero");
    assert!(oversized_stdout
        .result
        .unwrap_err()
        .contains("choices output exceeds 64 KiB"));
    assert!(oversized_stdout.stdout.len() < 66 * 1024);
    assert!(oversized_stdout
        .stdout
        .contains("truncated plugin output after 65536 bytes"));

    let mut missing = Command::new("/definitely/missing/herdr-provider");
    missing.stdout(Stdio::piped()).stderr(Stdio::piped());
    assert!(run_choices_provider(
        missing,
        &crate::app::PluginChoiceProviderCancellation::new(),
    )
    .result
    .unwrap_err()
    .contains("failed to spawn"));
}

#[test]
fn provider_timeout_terminates_process_group() {
    let root = root("choices-timeout-tree");
    let pid_file = root.join("descendant.pid");
    let started = Instant::now();
    let result = shell(&format!(
        "sleep 30 & echo $! > {}; wait",
        pid_file.display()
    ))
    .result
    .unwrap_err();
    let descendant = ProcessGuard::from_pid_file(&pid_file);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.contains("timed out after 2 seconds"));
    descendant.assert_gone();
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn provider_completion_kills_descendant_that_retains_output_pipes() {
    let root = root("choices-completion-tree");
    let pid_file = root.join("descendant.pid");
    let started = Instant::now();
    let completion = shell(&format!(
        r#"sleep 30 & echo $! > {}; printf '%s' '{{"version":1,"choices":[]}}'"#,
        pid_file.display()
    ));
    let descendant = ProcessGuard::from_pid_file(&pid_file);

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "provider completion waited for an inherited descendant pipe"
    );
    assert_eq!(completion.exit_code, Some(0));
    assert!(completion.result.is_ok(), "{:?}", completion.result);
    descendant.assert_gone();
    std::fs::remove_dir_all(root).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn provider_completion_does_not_wait_for_escaped_descendant_pipes() {
    let root = root("choices-escaped-completion");
    let pid_file = root.join("descendant.pid");
    let started = Instant::now();
    let completion = shell(&format!(
        r#"setsid sh -c 'echo $$ > {}; sleep 30' & printf '%s' '{{"version":1,"choices":[]}}'"#,
        pid_file.display()
    ));
    let _descendant = ProcessGuard::from_pid_file(&pid_file);

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "provider completion waited for an escaped descendant pipe"
    );
    assert_eq!(completion.exit_code, Some(0));
    assert!(completion.result.is_ok(), "{:?}", completion.result);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn provider_timeout_does_not_wait_for_escaped_descendant_pipes() {
    let root = root("choices-escaped-timeout");
    let pid_file = root.join("descendant.pid");
    let started = Instant::now();
    let result = shell(&format!(
        r#"setsid sh -c 'echo $$ > {}; sleep 30' & wait"#,
        pid_file.display()
    ))
    .result
    .unwrap_err();
    let _descendant = ProcessGuard::from_pid_file(&pid_file);

    assert!(
        started.elapsed() < Duration::from_secs(4),
        "provider timeout waited for an escaped descendant pipe"
    );
    assert!(result.contains("timed out after 2 seconds"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn provider_is_async_correlated_and_accounted() {
    let root = root("choices-async");
    let plugin = plugin(&root);
    let mut app = test_app();
    app.start_plugin_action_choices_provider(
        "request-42".into(),
        &plugin,
        "choose".into(),
        vec![
            "sh".into(),
            "-c".into(),
            r#"sleep 0.1; printf '{"version":1,"choices":[]}'"#.into(),
        ],
        &context(),
    )
    .unwrap();
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 1);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .contains("request-42"));
    assert_eq!(
        app.state.plugin_command_logs[0].status,
        PluginCommandStatus::Running
    );
    let logs = app.handle_plugin_log_list(
        "reentrant-api".into(),
        crate::api::schema::PluginLogListParams {
            plugin_id: Some(plugin.plugin_id.clone()),
            limit: Some(1),
        },
    );
    assert!(
        logs.contains("plugin-log-1"),
        "unexpected API response: {logs}"
    );
    let event = app.event_rx.blocking_recv().unwrap();
    match &event {
        crate::events::AppEvent::PluginActionChoicesFinished {
            request_id,
            plugin_id,
            action_id,
            result,
            ..
        } => {
            assert_eq!(request_id, "request-42");
            assert_eq!(plugin_id, "example.choices-runtime");
            assert_eq!(action_id, "choose");
            assert!(result.is_ok());
        }
        other => panic!("unexpected event: {other:?}"),
    }
    app.handle_internal_event(event);
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert_eq!(
        app.state.plugin_command_logs[0].status,
        PluginCommandStatus::Succeeded
    );
    std::fs::remove_dir_all(root).ok();
}

#[cfg(target_os = "linux")]
#[test]
fn escaped_descendant_pipes_do_not_hold_provider_capacity() {
    let root = root("choices-escaped-capacity");
    let pid_file = root.join("descendant.pid");
    let plugin = plugin(&root);
    let mut app = test_app();
    let started = Instant::now();
    app.start_plugin_action_choices_provider(
        "escaped-request".into(),
        &plugin,
        "choose".into(),
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                r#"setsid sh -c 'echo $$ > {}; sleep 30' & printf '{{"version":1,"choices":[]}}'"#,
                pid_file.display()
            ),
        ],
        &context(),
    )
    .unwrap();

    let event = app.event_rx.blocking_recv().unwrap();
    let _descendant = ProcessGuard::from_pid_file(&pid_file);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "escaped output pipes held provider capacity"
    );
    app.handle_internal_event(event);
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn provider_completion_accounting_survives_log_eviction_and_duplicates() {
    let root = root("choices-accounting-eviction");
    let plugin = plugin(&root);
    let mut app = test_app();
    app.start_plugin_action_choices_provider(
        "evicted-request".into(),
        &plugin,
        "choose".into(),
        vec![
            "sh".into(),
            "-c".into(),
            r#"printf '{"version":1,"choices":[]}'"#.into(),
        ],
        &context(),
    )
    .unwrap();
    let event = app.event_rx.blocking_recv().unwrap();
    for index in 0..PLUGIN_COMMAND_LOG_LIMIT {
        app.push_plugin_command_log(PluginCommandLogInfo {
            log_id: format!("eviction-{index}"),
            plugin_id: plugin.plugin_id.clone(),
            action_id: None,
            event: None,
            command: vec!["done".into()],
            status: PluginCommandStatus::Succeeded,
            started_unix_ms: 0,
            finished_unix_ms: Some(0),
            exit_code: Some(0),
            stdout: None,
            stderr: None,
            error: None,
        });
    }
    assert!(app
        .state
        .plugin_command_logs
        .iter()
        .all(|log| log.log_id != "plugin-log-1"));
    app.handle_internal_event(event);
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .is_empty());

    app.handle_internal_event(crate::events::AppEvent::PluginActionChoicesFinished {
        request_id: "evicted-request".into(),
        plugin_id: plugin.plugin_id.clone(),
        action_id: "choose".into(),
        log_id: "plugin-log-1".into(),
        finished_unix_ms: current_unix_ms(),
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        result: Ok(crate::api::schema::PluginActionChoices {
            version: 1,
            choices: vec![],
        }),
        cleanup_pending: false,
    });
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn provider_admission_is_dedicated_and_bounded_to_four() {
    let root = root("choices-limit");
    let plugin = plugin(&root);
    let mut app = test_app();
    app.state.plugin_commands_in_flight = MAX_PLUGIN_COMMANDS_IN_FLIGHT;
    for index in 0..MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT {
        app.start_plugin_action_choices_provider(
            format!("request-{index}"),
            &plugin,
            "choose".into(),
            vec![
                "sh".into(),
                "-c".into(),
                r#"sleep 0.1; printf '{"version":1,"choices":[]}'"#.into(),
            ],
            &context(),
        )
        .unwrap();
    }
    let rejected = app.start_plugin_action_choices_provider(
        "rejected".into(),
        &plugin,
        "choose".into(),
        vec!["sh".into(), "-c".into(), "exit 0".into()],
        &context(),
    );
    assert!(matches!(
        rejected,
        Err(("plugin_action_choices_provider_limit_reached", _))
    ));
    for _ in 0..MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT {
        let event = app.event_rx.blocking_recv().unwrap();
        app.handle_internal_event(event);
    }
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn dismiss_and_reopen_releases_capacity_only_after_cancelled_workers_finish() {
    use crate::app::state::{
        ContextMenuKind, ContextMenuPluginState, ContextMenuState, ContextMenuTarget, Mode,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let root = root("choices-dismiss-reopen");
    let plugin = plugin(&root);
    let mut app = test_app();
    for index in 0..MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT {
        app.start_plugin_action_choices_provider(
            format!("context-menu-11-{index}"),
            &plugin,
            "choose".into(),
            vec!["sh".into(), "-c".into(), "sleep 30".into()],
            &context(),
        )
        .unwrap();
    }
    let mut menu = ContextMenuState::new(ContextMenuKind::Workspace { ws_idx: 0 }, 0, 0);
    menu.plugin = Some(ContextMenuPluginState {
        generation: 11,
        context: context(),
        target: ContextMenuTarget::Workspace("gone".into()),
        providers: vec![],
        entries: vec![],
    });
    app.state.context_menu = Some(menu);
    app.state.mode = Mode::ContextMenu;

    app.handle_context_menu_key_via_api(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

    assert_eq!(
        app.state.plugin_action_choices_providers_in_flight,
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    assert_eq!(
        app.state.plugin_action_choices_requests_in_flight.len(),
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    assert_eq!(
        app.plugin_choice_provider_cancellations.len(),
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    let rejected_while_cancelling = app.start_plugin_action_choices_provider(
        "context-menu-12-too-early".into(),
        &plugin,
        "choose".into(),
        vec!["sh".into(), "-c".into(), "exit 0".into()],
        &context(),
    );
    assert!(matches!(
        rejected_while_cancelling,
        Err(("plugin_action_choices_provider_limit_reached", _))
    ));
    handle_provider_events_until_idle(&mut app);
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .is_empty());
    assert!(app.plugin_choice_provider_cancellations.is_empty());
    assert_eq!(
        app.state
            .plugin_command_logs
            .iter()
            .filter(|log| {
                log.status == PluginCommandStatus::Failed
                    && log.error.as_deref() == Some("choices provider cancelled")
                    && log.finished_unix_ms.is_some()
            })
            .count(),
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    for index in 0..MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT {
        app.start_plugin_action_choices_provider(
            format!("context-menu-12-{index}"),
            &plugin,
            "choose".into(),
            vec!["sh".into(), "-c".into(), "sleep 30".into()],
            &context(),
        )
        .unwrap();
    }
    assert_eq!(
        app.state.plugin_action_choices_providers_in_flight,
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    app.cancel_context_menu_plugin_generation(12);
    assert_eq!(
        app.state.plugin_action_choices_providers_in_flight,
        MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
    );
    handle_provider_events_until_idle(&mut app);
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn deferred_reaping_retains_admission_until_cleanup_event() {
    let mut app = test_app();
    app.state.plugin_action_choices_providers_in_flight = 1;
    app.state
        .plugin_action_choices_requests_in_flight
        .insert("deferred-request".into());
    app.plugin_choice_provider_cancellations.insert(
        "deferred-request".into(),
        crate::app::PluginChoiceProviderCancellation::new(),
    );
    app.push_plugin_command_log(PluginCommandLogInfo {
        log_id: "plugin-log-1".into(),
        plugin_id: "example.choices-runtime".into(),
        action_id: Some("choose".into()),
        event: None,
        command: vec!["provider".into()],
        status: PluginCommandStatus::Running,
        started_unix_ms: 0,
        finished_unix_ms: None,
        exit_code: None,
        stdout: None,
        stderr: None,
        error: None,
    });

    app.handle_internal_event(crate::events::AppEvent::PluginActionChoicesFinished {
        request_id: "deferred-request".into(),
        plugin_id: "example.choices-runtime".into(),
        action_id: "choose".into(),
        log_id: "plugin-log-1".into(),
        finished_unix_ms: current_unix_ms(),
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        result: Err("cleanup deferred".into()),
        cleanup_pending: true,
    });
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 1);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .contains("deferred-request"));

    app.handle_internal_event(
        crate::events::AppEvent::PluginActionChoicesCleanupFinished {
            request_id: "deferred-request".into(),
        },
    );
    assert_eq!(app.state.plugin_action_choices_providers_in_flight, 0);
    assert!(app
        .state
        .plugin_action_choices_requests_in_flight
        .is_empty());
}

#[test]
fn provider_cancelled_before_worker_start_never_executes_command() {
    let root = root("choices-pre-spawn-cancel");
    let marker = root.join("executed");
    let mut command = crate::plugin_command::command_for_argv_in_dir(
        "sh",
        &["-c".into(), format!("touch {}", marker.display())],
        &root,
    );
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cancellation = crate::app::PluginChoiceProviderCancellation::new();
    cancellation.cancel();

    let completion = run_choices_provider(command, &cancellation);

    assert_eq!(completion.result.unwrap_err(), "choices provider cancelled");
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn normal_command_choice_env_is_canonical_and_provider_env_removes_stale_choice() {
    let root = root("choice-env");
    let plugin = plugin(&root);
    let context = context();
    let mut provider_command = Command::new("sh");
    provider_command
        .env("HERDR_CLIENT_SOCKET_PATH", "/stale/client.sock")
        .env("HERDR_SESSION", "stale-session")
        .env("HERDR_PLUGIN_STALE", "stale")
        .env("HERDR_PLUGIN_ROOT", "/spoofed/root")
        .env("HERDR_PLUGIN_CONFIG_DIR", "/spoofed/config")
        .env("HERDR_PLUGIN_STATE_DIR", "/spoofed/state")
        .env("HERDR_PLUGIN_ACTION_ID", "spoofed-action")
        .env("HERDR_PLUGIN_CONTEXT_JSON", "spoofed-context")
        .env(PLUGIN_ACTION_CHOICE_ENV, "spoofed-choice");
    let env = plugin_command_env(
        &plugin,
        &context,
        Some("choose"),
        None,
        None,
        None,
        serde_json::to_string(&context).unwrap(),
    );
    configure_plugin_command(&mut provider_command, &root, &env);
    let command_env = provider_command.get_envs().collect::<Vec<_>>();
    let env_value = |name: &str| {
        command_env
            .iter()
            .find(|(key, _)| *key == name)
            .and_then(|(_, value)| value.and_then(std::ffi::OsStr::to_str))
    };
    assert_eq!(env_value(PLUGIN_ACTION_CHOICE_ENV), None);
    assert_eq!(env_value("HERDR_CLIENT_SOCKET_PATH"), None);
    assert_eq!(env_value("HERDR_SESSION"), None);
    assert_eq!(env_value("HERDR_PLUGIN_STALE"), None);
    assert_eq!(
        env_value("HERDR_PLUGIN_ROOT"),
        Some(plugin.plugin_root.as_str())
    );
    assert_eq!(
        env_value("HERDR_PLUGIN_CONFIG_DIR"),
        Some(
            super::super::env::plugin_config_dir(&plugin.plugin_id)
                .to_str()
                .unwrap()
        )
    );
    assert_eq!(
        env_value("HERDR_PLUGIN_STATE_DIR"),
        Some(
            super::super::env::plugin_state_dir(&plugin.plugin_id)
                .to_str()
                .unwrap()
        )
    );
    assert_eq!(env_value("HERDR_PLUGIN_ACTION_ID"), Some("choose"));
    let expected_context = serde_json::to_string(&context).unwrap();
    assert_eq!(
        env_value("HERDR_PLUGIN_CONTEXT_JSON"),
        Some(expected_context.as_str())
    );
    assert!(command_env.iter().any(|(key, value)| {
        *key == "HERDR_WORKSPACE_ID"
            && value.and_then(std::ffi::OsStr::to_str) == Some("workspace-exact")
    }));
    assert!(command_env
        .iter()
        .any(|(key, value)| *key == "HERDR_TAB_ID" && value.is_none()));

    let choice = crate::api::schema::PluginActionChoice {
        id: "opaque".into(),
        label: "Opaque".into(),
        payload: serde_json::json!({"z": 1}),
    };
    let choice_json = serde_json::to_string(&choice).unwrap();
    let mut action_command = Command::new("sh");
    let action_env = plugin_command_env(
        &plugin,
        &context,
        Some("choose"),
        None,
        None,
        Some(&choice_json),
        expected_context.clone(),
    );
    configure_plugin_command(&mut action_command, &root, &action_env);
    assert!(action_command.get_envs().any(|(key, value)| {
        key == PLUGIN_ACTION_CHOICE_ENV
            && value.and_then(std::ffi::OsStr::to_str) == Some(choice_json.as_str())
    }));

    let capture = root.join("choice.json");
    let mut app = test_app();
    app.start_plugin_command_with_choice(
        &plugin,
        "choose".into(),
        vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf '%s' \"$HERDR_PLUGIN_ACTION_CHOICE_JSON\" > {}",
                capture.display()
            ),
        ],
        &context,
        &choice,
    )
    .unwrap();
    let event = app.event_rx.blocking_recv().unwrap();
    app.handle_internal_event(event);
    assert_eq!(
        std::fs::read_to_string(capture).unwrap(),
        serde_json::to_string(&choice).unwrap()
    );
    std::fs::remove_dir_all(root).ok();
}
