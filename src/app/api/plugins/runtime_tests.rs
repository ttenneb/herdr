use super::*;
use crate::api::schema::PluginInvocationContext;

fn context() -> PluginInvocationContext {
    PluginInvocationContext {
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
    let mut command = crate::plugin_command::command_for_argv("sh", &["-c".into(), script.into()]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_choices_provider(command)
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

    let mut missing = Command::new("/definitely/missing/herdr-provider");
    missing.stdout(Stdio::piped()).stderr(Stdio::piped());
    assert!(run_choices_provider(missing)
        .result
        .unwrap_err()
        .contains("failed to spawn"));
}

#[test]
fn provider_timeout_terminates_process_group() {
    let started = Instant::now();
    let result = shell("sleep 30 & wait").result.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(result.contains("timed out after 2 seconds"));
}

#[test]
fn provider_completion_kills_descendant_that_retains_output_pipes() {
    let started = Instant::now();
    let completion = shell(r#"sleep 30 & printf '%s' '{"version":1,"choices":[]}'"#);

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "provider completion waited for an inherited descendant pipe"
    );
    assert_eq!(completion.exit_code, Some(0));
    assert!(completion.result.is_ok(), "{:?}", completion.result);
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
