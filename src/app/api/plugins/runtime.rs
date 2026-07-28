use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::manifest::{effective_platforms, ensure_platform_supported};
use super::plugin_manifest_available;
use crate::api::schema::{
    InstalledPluginInfo, PluginActionChoice, PluginCommandLogInfo, PluginCommandStatus,
    PluginInvocationContext,
};
use crate::app::App;

const PLUGIN_COMMAND_OUTPUT_MAX_BYTES: usize = 64 * 1024;
const PLUGIN_CHOICES_STDERR_MAX_BYTES: usize = 16 * 1024;
const PLUGIN_CHOICES_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const MAX_PLUGIN_COMMANDS_IN_FLIGHT: usize = 32;
pub(crate) const MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT: usize = 4;
const PLUGIN_COMMAND_LOG_LIMIT: usize = 200;
const PLUGIN_ACTION_CHOICE_ENV: &str = "HERDR_PLUGIN_ACTION_CHOICE_JSON";

const MANAGED_PLUGIN_ENV: &[&str] = &[
    crate::api::SOCKET_PATH_ENV_VAR,
    crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
    crate::session::SESSION_ENV_VAR,
    "HERDR_ENV",
    "HERDR_PLUGIN_ID",
    "HERDR_PLUGIN_CONTEXT_JSON",
    "HERDR_BIN_PATH",
    "HERDR_PLUGIN_ACTION_ID",
    "HERDR_PLUGIN_EVENT",
    "HERDR_PLUGIN_EVENT_JSON",
    "HERDR_WORKSPACE_ID",
    "HERDR_TAB_ID",
    "HERDR_PANE_ID",
    "HERDR_PLUGIN_CLICKED_URL",
    "HERDR_PLUGIN_LINK_HANDLER_ID",
    PLUGIN_ACTION_CHOICE_ENV,
];

impl App {
    pub(super) fn start_plugin_command(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        command: Vec<String>,
        context: &PluginInvocationContext,
        event_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        self.start_plugin_command_with_optional_choice(
            plugin, action_id, event, command, context, event_json, None,
        )
    }

    /// Invoke a normal plugin action with one canonical opaque choice in its
    /// environment. The action argv is intentionally unchanged.
    pub(crate) fn start_plugin_command_with_choice(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: String,
        command: Vec<String>,
        context: &PluginInvocationContext,
        choice: &PluginActionChoice,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        let choice_json = serde_json::to_string(choice)
            .map_err(|err| ("invalid_plugin_action_choice", err.to_string()))?;
        self.start_plugin_command_with_optional_choice(
            plugin,
            Some(action_id),
            None,
            command,
            context,
            None,
            Some(choice_json),
        )
    }

    fn start_plugin_command_with_optional_choice(
        &mut self,
        plugin: &InstalledPluginInfo,
        action_id: Option<String>,
        event: Option<String>,
        command: Vec<String>,
        context: &PluginInvocationContext,
        event_json: Option<String>,
        choice_json: Option<String>,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        let Some(program) = command.first().cloned() else {
            return Err((
                "invalid_plugin_command",
                "command must not be empty".to_string(),
            ));
        };
        let args = command.iter().skip(1).cloned().collect::<Vec<_>>();
        let context_json = serde_json::to_string(context)
            .map_err(|err| ("invalid_plugin_context", err.to_string()))?;
        super::env::ensure_plugin_user_dirs(plugin)
            .map_err(|err| ("plugin_user_dir_create_failed", err.to_string()))?;
        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        let env = plugin_command_env(
            plugin,
            context,
            action_id.as_deref(),
            event.as_deref(),
            event_json.as_deref(),
            choice_json.as_deref(),
            context_json,
        );
        if self.state.plugin_commands_in_flight >= MAX_PLUGIN_COMMANDS_IN_FLIGHT {
            let message = format!(
                "maximum concurrent plugin commands reached ({MAX_PLUGIN_COMMANDS_IN_FLIGHT})"
            );
            let log = PluginCommandLogInfo {
                log_id,
                plugin_id: plugin.plugin_id.clone(),
                action_id,
                event,
                command,
                status: PluginCommandStatus::Failed,
                started_unix_ms,
                finished_unix_ms: Some(started_unix_ms),
                exit_code: None,
                stdout: Some(String::new()),
                stderr: Some(String::new()),
                error: Some(message.clone()),
            };
            self.push_plugin_command_log(log);
            return Err(("plugin_command_limit_reached", message));
        }
        let plugin_root = std::path::PathBuf::from(&plugin.plugin_root);
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin.plugin_id.clone(),
            action_id,
            event,
            command: command.clone(),
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        self.push_plugin_command_log(log.clone());
        self.state.plugin_commands_in_flight += 1;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let mut command =
                crate::plugin_command::command_for_argv_in_dir(&program, &args, &plugin_root);
            configure_plugin_command(&mut command, &plugin_root, &env);
            let child = command.spawn();
            let finished = match child {
                Ok(mut child) => {
                    let stdout = child.stdout.take();
                    let stderr = child.stderr.take();
                    let stdout_reader = stdout.map(|stdout| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stdout, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    let stderr_reader = stderr.map(|stderr| {
                        std::thread::spawn(move || {
                            read_capped_plugin_output(stderr, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                        })
                    });
                    match child.wait() {
                        Ok(status) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: status.code(),
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            error: None,
                        },
                        Err(err) => crate::events::AppEvent::PluginCommandFinished {
                            log_id,
                            finished_unix_ms: current_unix_ms(),
                            exit_code: None,
                            stdout: stdout_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            stderr: stderr_reader
                                .and_then(|reader| reader.join().ok())
                                .unwrap_or_default(),
                            error: Some(err.to_string()),
                        },
                    }
                }
                Err(err) => crate::events::AppEvent::PluginCommandFinished {
                    log_id,
                    finished_unix_ms: current_unix_ms(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                },
            };
            let _ = event_tx.blocking_send(finished);
        });
        Ok(log)
    }

    /// Start a neutral asynchronous choices provider for an exact captured
    /// invocation context. Admission is global and independent of normal
    /// plugin commands.
    pub(crate) fn start_plugin_action_choices_provider(
        &mut self,
        request_id: String,
        plugin: &InstalledPluginInfo,
        action_id: String,
        command: Vec<String>,
        context: &PluginInvocationContext,
    ) -> Result<PluginCommandLogInfo, (&'static str, String)> {
        let Some(program) = command.first().cloned() else {
            return Err((
                "invalid_plugin_command",
                "choices command must not be empty".to_string(),
            ));
        };
        let args = command.iter().skip(1).cloned().collect::<Vec<_>>();
        let context_json = serde_json::to_string(context)
            .map_err(|err| ("invalid_plugin_context", err.to_string()))?;
        super::env::ensure_plugin_user_dirs(plugin)
            .map_err(|err| ("plugin_user_dir_create_failed", err.to_string()))?;

        let log_id = format!("plugin-log-{}", self.state.next_plugin_command_log_id);
        self.state.next_plugin_command_log_id += 1;
        let started_unix_ms = current_unix_ms();
        if self
            .state
            .plugin_action_choices_requests_in_flight
            .contains(&request_id)
        {
            return Err((
                "duplicate_plugin_action_choices_request",
                "choices provider request identity is already in flight".to_string(),
            ));
        }
        if self.state.plugin_action_choices_requests_in_flight.len()
            >= MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT
        {
            let message = format!(
                "maximum concurrent plugin action choices providers reached ({MAX_PLUGIN_ACTION_CHOICES_PROVIDERS_IN_FLIGHT})"
            );
            self.push_plugin_command_log(PluginCommandLogInfo {
                log_id,
                plugin_id: plugin.plugin_id.clone(),
                action_id: Some(action_id),
                event: None,
                command,
                status: PluginCommandStatus::Failed,
                started_unix_ms,
                finished_unix_ms: Some(started_unix_ms),
                exit_code: None,
                stdout: Some(String::new()),
                stderr: Some(String::new()),
                error: Some(message.clone()),
            });
            return Err(("plugin_action_choices_provider_limit_reached", message));
        }

        let env = plugin_command_env(
            plugin,
            context,
            Some(&action_id),
            None,
            None,
            None,
            context_json,
        );
        let plugin_root = std::path::PathBuf::from(&plugin.plugin_root);
        let plugin_id = plugin.plugin_id.clone();
        let log = PluginCommandLogInfo {
            log_id: log_id.clone(),
            plugin_id: plugin_id.clone(),
            action_id: Some(action_id.clone()),
            event: None,
            command: command.clone(),
            status: PluginCommandStatus::Running,
            started_unix_ms,
            finished_unix_ms: None,
            exit_code: None,
            stdout: None,
            stderr: None,
            error: None,
        };
        self.push_plugin_command_log(log.clone());
        self.state.plugin_action_choices_providers_in_flight += 1;
        self.state
            .plugin_action_choices_requests_in_flight
            .insert(request_id.clone());
        let cancellation = crate::app::PluginChoiceProviderCancellation::new();
        self.plugin_choice_provider_cancellations
            .insert(request_id.clone(), cancellation.clone());

        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let mut command =
                crate::plugin_command::command_for_argv_in_dir(&program, &args, &plugin_root);
            configure_plugin_command(&mut command, &plugin_root, &env);
            command.stdin(Stdio::null());
            let ChoicesProviderCompletion {
                exit_code,
                stdout,
                stderr,
                result,
                deferred_reap,
            } = run_choices_provider(command, &cancellation);
            let cleanup_pending = deferred_reap.is_some();
            let event = crate::events::AppEvent::PluginActionChoicesFinished {
                request_id: request_id.clone(),
                plugin_id,
                action_id,
                log_id,
                finished_unix_ms: current_unix_ms(),
                exit_code,
                stdout,
                stderr,
                result,
                cleanup_pending,
            };
            let _ = event_tx.blocking_send(event);
            if let Some(child) = deferred_reap {
                child.reap();
                let _ = event_tx.blocking_send(
                    crate::events::AppEvent::PluginActionChoicesCleanupFinished { request_id },
                );
            }
        });

        Ok(log)
    }

    pub(crate) fn run_plugin_startup_hooks(&mut self) {
        let mut context = self.current_plugin_context("plugin.startup");
        context.invocation_source = Some("startup".to_string());
        let mut plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled && plugin_manifest_available(plugin) && !plugin.startup.is_empty()
            })
            .cloned()
            .collect::<Vec<_>>();
        plugins.sort_by(|left, right| left.plugin_id.cmp(&right.plugin_id));
        for plugin in plugins {
            for startup in plugin.startup.clone() {
                if ensure_platform_supported(
                    &effective_platforms(&startup.platforms, &plugin.platforms).clone(),
                    "startup",
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some("startup".to_string()),
                    startup.command,
                    &context,
                    None,
                );
            }
        }
    }

    pub(crate) fn run_plugin_event_hooks(&mut self, event: &crate::api::schema::EventEnvelope) {
        let event_name = event.event.dot_name();
        if !crate::api::schema::PLUGIN_HOOK_EVENT_KINDS.contains(&event.event) {
            return;
        }
        if let Err(err) = self.refresh_installed_plugins() {
            tracing::warn!(err = %err, "failed to refresh plugin registry before event hooks");
            return;
        }
        let plugins = self
            .state
            .installed_plugins
            .values()
            .filter(|plugin| {
                plugin.enabled
                    && plugin_manifest_available(plugin)
                    && plugin.events.iter().any(|hook| hook.on == event_name)
            })
            .cloned()
            .collect::<Vec<_>>();
        if plugins.is_empty() {
            return;
        }
        let event_json = serde_json::to_string(event).ok();
        let context = self.plugin_context_for_event(event, event_name);
        for plugin in plugins {
            for hook in plugin.events.clone() {
                if hook.on != event_name {
                    continue;
                }
                if ensure_platform_supported(
                    &effective_platforms(&hook.platforms, &plugin.platforms).clone(),
                    event_name,
                )
                .is_err()
                {
                    continue;
                }
                let _ = self.start_plugin_command(
                    &plugin,
                    None,
                    Some(event_name.to_string()),
                    hook.command.clone(),
                    &context,
                    event_json.clone(),
                );
            }
        }
    }

    fn push_plugin_command_log(&mut self, log: PluginCommandLogInfo) {
        self.state.plugin_command_logs.push(log);
        if self.state.plugin_command_logs.len() > PLUGIN_COMMAND_LOG_LIMIT {
            let extra = self.state.plugin_command_logs.len() - PLUGIN_COMMAND_LOG_LIMIT;
            self.state.plugin_command_logs.drain(0..extra);
        }
    }
}

fn plugin_command_env(
    plugin: &InstalledPluginInfo,
    context: &PluginInvocationContext,
    action_id: Option<&str>,
    event: Option<&str>,
    event_json: Option<&str>,
    choice_json: Option<&str>,
    context_json: String,
) -> Vec<(String, String)> {
    let mut env = super::env::plugin_path_env(plugin);
    env.extend([
        (
            crate::api::SOCKET_PATH_ENV_VAR.to_string(),
            crate::api::socket_path().display().to_string(),
        ),
        ("HERDR_ENV".to_string(), "1".to_string()),
        ("HERDR_PLUGIN_ID".to_string(), plugin.plugin_id.clone()),
        ("HERDR_PLUGIN_CONTEXT_JSON".to_string(), context_json),
    ]);
    if let Ok(current_exe) = std::env::current_exe() {
        env.push((
            "HERDR_BIN_PATH".to_string(),
            current_exe.display().to_string(),
        ));
    }
    for (key, value) in [
        ("HERDR_PLUGIN_ACTION_ID", action_id),
        ("HERDR_PLUGIN_EVENT", event),
        ("HERDR_PLUGIN_EVENT_JSON", event_json),
        ("HERDR_WORKSPACE_ID", context.workspace_id.as_deref()),
        ("HERDR_TAB_ID", context.tab_id.as_deref()),
        ("HERDR_PANE_ID", context.focused_pane_id.as_deref()),
        ("HERDR_PLUGIN_CLICKED_URL", context.clicked_url.as_deref()),
        (
            "HERDR_PLUGIN_LINK_HANDLER_ID",
            context.link_handler_id.as_deref(),
        ),
        (PLUGIN_ACTION_CHOICE_ENV, choice_json),
    ] {
        if let Some(value) = value {
            env.push((key.to_string(), value.to_string()));
        }
    }
    env
}

fn configure_plugin_command(
    command: &mut Command,
    plugin_root: &std::path::Path,
    env: &[(String, String)],
) {
    command
        .current_dir(plugin_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // This is environment hygiene for trusted plugins, not a sandbox or a
    // security boundary. Plugins can still deliberately alter their process
    // groups or launch arbitrary descendants.
    let explicit_plugin_keys = command
        .get_envs()
        .filter(|(key, _)| key.to_string_lossy().starts_with("HERDR_PLUGIN_"))
        .map(|(key, _)| key.to_os_string())
        .collect::<Vec<_>>();
    for key in explicit_plugin_keys {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("HERDR_PLUGIN_") {
            command.env_remove(key);
        }
    }
    for key in MANAGED_PLUGIN_ENV {
        command.env_remove(key);
    }
    command.envs(env.iter().map(|(key, value)| (key, value)));
}

struct ChoicesProviderCompletion {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    result: Result<crate::api::schema::PluginActionChoices, String>,
    deferred_reap: Option<crate::platform::IsolatedChild>,
}

fn run_choices_provider(
    mut command: Command,
    cancellation: &crate::app::PluginChoiceProviderCancellation,
) -> ChoicesProviderCompletion {
    let spawn_gate = cancellation.lock_spawn();
    if cancellation
        .signal
        .load(std::sync::atomic::Ordering::Acquire)
    {
        drop(spawn_gate);
        return ChoicesProviderCompletion {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            result: Err("choices provider cancelled".to_string()),
            deferred_reap: None,
        };
    }
    let mut child = match crate::platform::IsolatedChild::spawn(&mut command) {
        Ok(child) => child,
        Err(err) => {
            return ChoicesProviderCompletion {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
                result: Err(format!("failed to spawn choices provider: {err}")),
                deferred_reap: None,
            };
        }
    };
    drop(spawn_gate);
    let mut stdout = BoundedOutput::default();
    let mut stderr = BoundedOutput::default();
    let mut buffer = [0_u8; 8192];
    let deadline = Instant::now() + PLUGIN_CHOICES_TIMEOUT;
    let (status, execution_error) = loop {
        drain_provider_output_once(&mut child, &mut stdout, &mut stderr, &mut buffer);
        match child.try_wait() {
            Ok(Some(status)) => {
                // Descendants can retain or deliberately escape with inherited
                // pipes. Terminate the isolated tree, then drain only bytes that
                // are already available; pipe EOF is never a completion gate.
                let _ = child.terminate_tree();
                drain_provider_output_available(&mut child, &mut stdout, &mut stderr, &mut buffer);
                break (Some(status), None);
            }
            Ok(None)
                if !cancellation
                    .signal
                    .load(std::sync::atomic::Ordering::Acquire)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None)
                if cancellation
                    .signal
                    .load(std::sync::atomic::Ordering::Acquire) =>
            {
                let termination_error = child.terminate_tree().err();
                let status = reap_terminated_provider(&mut child);
                drain_provider_output_available(&mut child, &mut stdout, &mut stderr, &mut buffer);
                let message = termination_error.map_or_else(
                    || "choices provider cancelled".to_string(),
                    |err| format!("choices provider cancellation failed: {err}"),
                );
                break (status, Some(message));
            }
            Ok(None) => {
                let termination_error = child.terminate_tree().err();
                let status = reap_terminated_provider(&mut child);
                drain_provider_output_available(&mut child, &mut stdout, &mut stderr, &mut buffer);
                let message = termination_error.map_or_else(
                    || "choices provider timed out after 2 seconds".to_string(),
                    |err| format!(
                        "choices provider timed out after 2 seconds; process-tree termination failed: {err}"
                    ),
                );
                break (status, Some(message));
            }
            Err(err) => {
                let _ = child.terminate_tree();
                drain_provider_output_available(&mut child, &mut stdout, &mut stderr, &mut buffer);
                break (
                    None,
                    Some(format!("failed waiting for choices provider: {err}")),
                );
            }
        }
    };

    let stdout_log = stdout.log_string(PLUGIN_COMMAND_OUTPUT_MAX_BYTES);
    let stderr_log = stderr.log_string(PLUGIN_CHOICES_STDERR_MAX_BYTES);
    let exit_code = status.as_ref().and_then(std::process::ExitStatus::code);
    let result = if let Some(error) = execution_error {
        Err(error)
    } else if !status
        .as_ref()
        .is_some_and(std::process::ExitStatus::success)
    {
        Err(match exit_code {
            Some(code) => format!("choices provider exited with status {code}"),
            None => "choices provider terminated without an exit code".to_string(),
        })
    } else if stdout.truncated {
        Err(super::choices::PluginActionChoicesParseError::OutputTooLarge.to_string())
    } else {
        super::choices::parse_plugin_action_choices(&stdout.bytes).map_err(|err| err.to_string())
    };

    ChoicesProviderCompletion {
        exit_code,
        stdout: stdout_log,
        stderr: stderr_log,
        result,
        deferred_reap: status.is_none().then_some(child),
    }
}

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

impl BoundedOutput {
    fn append(&mut self, bytes: &[u8], cap: usize) {
        let remaining = cap.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        self.truncated |= bytes.len() > remaining;
    }

    fn log_string(&self, cap: usize) -> String {
        let mut output = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.truncated {
            output.push_str(&format!(
                "\n[herdr truncated plugin output after {cap} bytes]"
            ));
        }
        output
    }
}

fn drain_provider_output_once(
    child: &mut crate::platform::IsolatedChild,
    stdout: &mut BoundedOutput,
    stderr: &mut BoundedOutput,
    buffer: &mut [u8],
) -> bool {
    let mut read_bytes = false;
    if let Ok(crate::platform::AvailableOutput::Bytes(read)) = child.read_stdout_available(buffer) {
        stdout.append(&buffer[..read], PLUGIN_COMMAND_OUTPUT_MAX_BYTES);
        read_bytes = true;
    }
    if let Ok(crate::platform::AvailableOutput::Bytes(read)) = child.read_stderr_available(buffer) {
        stderr.append(&buffer[..read], PLUGIN_CHOICES_STDERR_MAX_BYTES);
        read_bytes = true;
    }
    read_bytes
}

fn drain_provider_output_available(
    child: &mut crate::platform::IsolatedChild,
    stdout: &mut BoundedOutput,
    stderr: &mut BoundedOutput,
    buffer: &mut [u8],
) {
    // More than enough to drain both bounded outputs, but finite even if an
    // escaped descendant continuously writes to an inherited pipe.
    for _ in 0..32 {
        if !drain_provider_output_once(child, stdout, stderr, buffer) {
            break;
        }
    }
}

fn reap_terminated_provider(
    child: &mut crate::platform::IsolatedChild,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(None) | Err(_) => return None,
        }
    }
}

pub(super) fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(super) fn read_capped_plugin_output(mut reader: impl Read, cap: usize) -> String {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = cap.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buf[..n.min(remaining)]);
                }
                if n > remaining {
                    truncated = true;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str(&format!(
            "\n[herdr truncated plugin output after {cap} bytes]"
        ));
    }
    output
}

#[cfg(all(test, unix))]
#[path = "runtime_tests.rs"]
mod tests;
