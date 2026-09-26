use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use bytes::Bytes;

use super::{terminal_targets::TerminalTargetError, App};
use crate::api::schema::AgentStartParams;

const DEFAULT_AGENT_START_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const MAX_AGENT_START_TIMEOUT: Duration = Duration::from_secs(300);
pub(crate) const AGENT_START_SETTLE_DELAY: Duration = Duration::from_secs(3);
const INVALID_AGENT_TIMEOUT_MESSAGE: &str =
    "agent start timeout must be greater than 3000ms and at most 300000ms";
const INVALID_AGENT_NAME_MESSAGE: &str = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";
const MAX_AGENT_ENVIRONMENT_ITEMS: usize = 16;
const MAX_AGENT_ENVIRONMENT_BYTES: usize = 16 * 1024;

/// Not a pane launch command or persisted resume hint. Created only after a
/// committed managed Pi start, and bound to a new foreground process on Active.
#[derive(Debug, Clone)]
pub(crate) struct ManagedPiLaunch {
    pub(crate) generation: u64,
    pub(crate) session_path: String,
    pub(crate) earliest_birth_ticks: u64,
    pub(crate) process: Option<crate::platform::ProcessBirthIdentity>,
}

pub(super) fn explicit_pi_session_path(argv: &[String]) -> Option<String> {
    let mut path = None;
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--session" {
            Some(args.next()?.as_str())
        } else {
            arg.strip_prefix("--session=")
        };
        if let Some(value) = value {
            if path.replace(value).is_some() {
                return None;
            }
        }
    }
    let value = path?;
    crate::agent_resume::AgentSessionRef::path(value)?;
    (std::path::Path::new(value).extension()? == "jsonl").then(|| value.to_string())
}

/// Launch authority persisted by [`App::prepare_managed_launch`].
#[derive(Debug)]
pub(crate) struct PreparedManagedLaunch {
    pub(crate) generation: u64,
    pi_session: Option<(String, u64)>,
    recipe: Option<crate::launch_recipe::LaunchRecipe>,
}

/// `NAME=value` from an `agent.start` environment list.
fn launch_env_value(values: &[String], name: &str) -> Option<String> {
    values.iter().find_map(|entry| {
        entry
            .split_once('=')
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
}

/// Pi's session directory as Pi resolves it for this launch:
/// `PI_CODING_AGENT_SESSION_DIR`, else `sessionDir` from
/// `<agent dir>/settings.json`, else `<agent dir>/sessions`, where the agent dir
/// is `PI_CODING_AGENT_DIR` or `~/.pi/agent`. Launch environment wins over the
/// server environment. A relative or unresolvable directory yields `None`.
fn pi_session_root(launch_env: &impl Fn(&str) -> Option<String>) -> Option<std::path::PathBuf> {
    let lookup = |name: &str| {
        launch_env(name)
            .or_else(|| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    };
    let home = lookup("HOME").map(std::path::PathBuf::from);
    let expand = |value: &str| -> Option<std::path::PathBuf> {
        let path = match value.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                home.as_ref()?.join(rest.trim_start_matches('/'))
            }
            Some(_) => return None,
            None => std::path::PathBuf::from(value),
        };
        path.is_absolute().then_some(path)
    };
    if let Some(dir) = lookup("PI_CODING_AGENT_SESSION_DIR") {
        return expand(&dir);
    }
    let agent_dir = match lookup("PI_CODING_AGENT_DIR") {
        Some(dir) => expand(&dir)?,
        None => home.as_ref()?.join(".pi/agent"),
    };
    let configured = std::fs::read(agent_dir.join("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|settings| settings.get("sessionDir")?.as_str().map(str::to_string));
    match configured {
        Some(dir) => expand(&dir),
        None => Some(agent_dir.join("sessions")),
    }
}

/// A `--session` path earns a managed launch record only if it is an existing,
/// canonical, owner-private (0600), single-link JSONL that is empty or has a
/// valid Pi header, and it lies inside Pi's session directory for this launch.
/// Anything else still launches, but its identity stays `reported`.
fn trusted_launch_session_path(path: &str, launch_env: &impl Fn(&str) -> Option<String>) -> bool {
    let path = std::path::Path::new(path);
    let Some(root) = pi_session_root(launch_env).and_then(|root| std::fs::canonicalize(root).ok())
    else {
        return false;
    };
    path.starts_with(&root) && path != root && crate::platform::launchable_pi_session_jsonl(path)
}

fn valid_agent_environment(values: &[String]) -> bool {
    if values.len() > MAX_AGENT_ENVIRONMENT_ITEMS
        || values.iter().map(String::len).sum::<usize>() > MAX_AGENT_ENVIRONMENT_BYTES
    {
        return false;
    }
    let mut names = HashSet::new();
    values.iter().all(|entry| {
        let Some((name, value)) = entry.split_once('=') else {
            return false;
        };
        let mut chars = name.chars();
        matches!(chars.next(), Some('A'..='Z' | 'a'..='z' | '_'))
            && name.len() <= 128
            && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
            && value.len() <= 4096
            && !value.chars().any(char::is_control)
            && names.insert(name.to_ascii_uppercase())
    })
}

fn valid_agent_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 32
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
}

impl App {
    pub(super) fn collect_agent_infos(&self) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs.iter().flat_map(move |tab| {
                    tab.layout
                        .pane_ids()
                        .into_iter()
                        .filter_map(move |pane_id| self.agent_info(ws_idx, pane_id))
                })
            })
            .collect()
    }

    pub(super) fn reconcile_managed_agent_target(&mut self, target: &str) {
        let Ok(resolved) = self.resolve_agent_target(target) else {
            return;
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return;
        };
        let changed = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .is_some_and(|terminal| terminal.reconcile_managed_agent_at(Instant::now(), false));
        if changed {
            self.state.mark_session_dirty();
            self.schedule_session_save();
            self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        }
    }

    pub(super) fn agent_info_for_target(
        &self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn focus_agent_target(
        &mut self,
        target: &str,
    ) -> Result<crate::api::schema::AgentInfo, TerminalTargetError> {
        let resolved = self.resolve_agent_target(target)?;
        self.state
            .focus_pane_in_workspace(resolved.ws_idx, resolved.pane_id);
        self.state.settle_terminal_mode_after_focus();
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| TerminalTargetError::NotFound {
                target: target.to_string(),
            })
    }

    pub(super) fn rename_agent_target(
        &mut self,
        target: &str,
        name: Option<String>,
    ) -> Result<crate::api::schema::AgentInfo, AgentRenameError> {
        let resolved = self
            .resolve_agent_target(target)
            .map_err(AgentRenameError::Target)?;
        let normalized_name = match name {
            Some(name) if valid_agent_name(&name) => Some(name),
            Some(_) => return Err(AgentRenameError::InvalidName),
            None => None,
        };

        if let Some(name) = normalized_name.as_deref() {
            let mut conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
            // A sleeping agent keeps its name (see start_agent).
            let own_terminal = self
                .state
                .workspaces
                .get(resolved.ws_idx)
                .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
                .cloned();
            conflicts.extend(self.sleeping_name_conflicts(name, own_terminal.as_ref()));
            if !conflicts.is_empty() {
                return Err(AgentRenameError::DuplicateName {
                    name: name.to_string(),
                    candidates: conflicts,
                });
            }
        }

        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == resolved.terminal_id)
        else {
            return Err(AgentRenameError::Target(TerminalTargetError::NotFound {
                target: target.to_string(),
            }));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(AgentRenameError::PendingLaunch);
        }
        if terminal.effective_agent_label().is_none() {
            return Err(AgentRenameError::NotAgent);
        }
        match normalized_name {
            Some(name) => terminal.set_agent_name(name),
            None => terminal.clear_agent_name(),
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
        self.emit_pane_updated(resolved.ws_idx, resolved.pane_id);
        self.agent_info(resolved.ws_idx, resolved.pane_id)
            .ok_or_else(|| {
                AgentRenameError::Target(TerminalTargetError::NotFound {
                    target: target.to_string(),
                })
            })
    }

    pub(super) fn prepare_agent_launch(
        &self,
        params: &AgentStartParams,
    ) -> Result<(crate::detect::Agent, Vec<String>), AgentStartError> {
        if !valid_agent_name(&params.name) {
            return Err(AgentStartError::InvalidName);
        }
        let Some(kind) = crate::detect::parse_agent_label(&params.kind) else {
            return Err(AgentStartError::UnsupportedKind(params.kind.clone()));
        };
        if params
            .args
            .iter()
            .any(|arg| arg.chars().any(char::is_control))
        {
            return Err(AgentStartError::InvalidArgument);
        }
        if !valid_agent_environment(&params.env) {
            return Err(AgentStartError::InvalidEnvironment);
        }
        let mut conflicts = self.agent_name_conflicts(&params.name, "");
        // A sleeping agent keeps its name; only a relaunch in its own pane
        // (a wake or a start in that pane) may use it.
        let target_terminal =
            self.parse_current_public_pane_id(&params.pane_id)
                .and_then(|(ws_idx, pane_id)| {
                    self.state
                        .workspaces
                        .get(ws_idx)?
                        .terminal_id(pane_id)
                        .cloned()
                });
        conflicts.extend(self.sleeping_name_conflicts(&params.name, target_terminal.as_ref()));
        if !conflicts.is_empty() {
            return Err(AgentStartError::DuplicateName {
                name: params.name.clone(),
                candidates: conflicts,
            });
        }
        let mut argv = vec![crate::detect::interactive_agent_executable(kind).to_string()];
        argv.extend(params.args.iter().cloned());
        Ok((kind, argv))
    }

    pub(super) fn agent_start_timeout(
        &self,
        params: &AgentStartParams,
    ) -> Result<Duration, AgentStartError> {
        let timeout = Duration::from_millis(
            params
                .timeout_ms
                .unwrap_or(DEFAULT_AGENT_START_TIMEOUT.as_millis() as u64),
        );
        if timeout <= AGENT_START_SETTLE_DELAY || timeout > MAX_AGENT_START_TIMEOUT {
            return Err(AgentStartError::InvalidTimeout);
        }
        Ok(timeout)
    }

    fn allocate_sender_authority_generation(
        &self,
        sender_key: String,
    ) -> Result<u64, AgentStartError> {
        let store = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &sender_key,
        )
        .map_err(|err| AgentStartError::AuthorityPersistence(err.to_string()))?;
        let previous = store
            .recover()
            .map_err(|err| AgentStartError::AuthorityPersistence(err.to_string()))?;
        let process_generation = previous
            .as_ref()
            .map(|record| record.process_generation)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| AgentStartError::AuthorityPersistence("generation exhausted".into()))?;
        let transition_revision = previous
            .as_ref()
            .map(|record| record.transition_revision)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| AgentStartError::AuthorityPersistence("revision exhausted".into()))?;
        store
            .cas(
                previous.as_ref().map(|record| record.transition_revision),
                crate::sender_authority::SenderAuthorityRecord {
                    sender_key,
                    process_generation,
                    phase: crate::sender_authority::SenderAuthorityPhase::Preparing,
                    transition_revision,
                },
            )
            .map_err(|err| AgentStartError::AuthorityPersistence(err.to_string()))?;
        Ok(process_generation)
    }

    /// The one managed-launch path shared by `agent.start` and
    /// `collection.helper_launch`. Call it immediately before the agent process
    /// is spawned or its command is submitted. It persists a fresh
    /// sender-authority generation for `terminal_id` (so nothing earlier can act
    /// as this launch) and drops any previous launch record for the terminal.
    /// For a Pi whose argv names exactly one `--session` file that passes the
    /// launch checks (see [`trusted_launch_session_path`]), it also samples the
    /// birth-tick cutoff and waits past it, so only a process born after this
    /// point can ever bind to the recorded session.
    ///
    /// Nothing in memory is bound until [`Self::commit_managed_launch`]; any
    /// failure after this returns must call [`Self::abandon_managed_launch`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_managed_launch(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        name: &str,
        kind: crate::detect::Agent,
        argv: &[String],
        explicit_env: &[(String, String)],
        launch_env: impl Fn(&str) -> Option<String>,
    ) -> Result<PreparedManagedLaunch, AgentStartError> {
        let generation = self.allocate_sender_authority_generation(terminal_id.to_string())?;
        self.managed_pi_launches.remove(terminal_id);
        // A process born in this coarse kernel tick is ambiguous. Sample the
        // strict cutoff, then wait for that tick before the process can start
        // so even a fast legitimate Pi is not penalized by the cutoff.
        let pi_session = (kind == crate::detect::Agent::Pi)
            .then(|| {
                explicit_pi_session_path(argv)
                    .filter(|path| trusted_launch_session_path(path, &launch_env))
                    .zip(crate::platform::first_post_launch_birth_tick())
            })
            .flatten();
        if let Some((_, cutoff)) = pi_session.as_ref() {
            if !crate::platform::wait_until_birth_tick(*cutoff) {
                self.abandon_managed_launch(terminal_id, generation);
                return Err(AgentStartError::AuthorityPersistence(
                    "process birth clock did not advance before launch".into(),
                ));
            }
        }
        let recipe = crate::launch_recipe::LaunchRecipe::capture(
            name,
            crate::detect::agent_label(kind),
            argv.get(1..).unwrap_or_default(),
            explicit_env,
        );
        Ok(PreparedManagedLaunch {
            generation,
            pi_session,
            recipe,
        })
    }

    /// Bind a prepared launch once its terminal and runtime exist and the
    /// terminal has begun its managed agent: the generation goes to both, and a
    /// trusted Pi session is recorded for Active-time process binding.
    /// Returns the launch's recipe, to be applied with
    /// [`Self::finalize_managed_launch`] only once the process has actually
    /// been started (input sent or member spawned).
    pub(super) fn commit_managed_launch(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        prepared: PreparedManagedLaunch,
    ) -> Option<crate::launch_recipe::LaunchRecipe> {
        if let Some(runtime) = self.terminal_runtimes.get(terminal_id) {
            runtime.set_managed_agent_generation(prepared.generation);
        }
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.set_managed_agent_generation(prepared.generation);
        }
        if let Some((session_path, earliest_birth_ticks)) = prepared.pi_session {
            self.managed_pi_launches.insert(
                terminal_id.clone(),
                ManagedPiLaunch {
                    generation: prepared.generation,
                    session_path,
                    earliest_birth_ticks,
                    process: None,
                },
            );
        }
        prepared.recipe
    }

    /// After a successful start: record the recipe, and end a Herdr sleep and
    /// drop a carried route for a hand start, or queue the route carry for a
    /// recipe relaunch. A failed start never gets here, so it loses nothing.
    pub(super) fn finalize_managed_launch(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        recipe: Option<crate::launch_recipe::LaunchRecipe>,
    ) {
        let relaunch = self.recipe_relaunches.contains(terminal_id)
            || self.pane_wakes.contains_key(terminal_id);
        let mut carry_route = false;
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            // The durable recipe is what a later wake or restart relaunches.
            terminal.launch_recipe = recipe;
            if relaunch {
                // A wake keeps the sleep until its Pi attaches; a recipe
                // relaunch may carry the pane's delegation route over.
                carry_route = terminal.route_carry.is_some();
            } else {
                // A hand start ends a Herdr sleep and never inherits a route.
                terminal.sleep = None;
                terminal.route_carry = None;
            }
        }
        if carry_route {
            self.pending_route_carries.insert(
                terminal_id.clone(),
                Instant::now() + super::api::ROUTE_CARRY_TIMEOUT,
            );
        } else {
            self.pending_route_carries.remove(terminal_id);
        }
    }

    /// Undo a prepared (or committed) launch whose process did not start: no
    /// launch record survives, the runtime stops tagging observations with the
    /// generation, and the durable generation is invalidated so it can never be
    /// promoted. The next launch allocates a newer generation.
    pub(super) fn abandon_managed_launch(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        generation: u64,
    ) {
        if self
            .managed_pi_launches
            .get(terminal_id)
            .is_some_and(|launch| launch.generation == generation)
        {
            self.managed_pi_launches.remove(terminal_id);
        }
        if let Some(runtime) = self.terminal_runtimes.get(terminal_id) {
            runtime.set_managed_agent_generation(0);
        }
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            if terminal.accepts_managed_agent_generation(generation) {
                terminal.set_managed_agent_generation(0);
            }
        }
        if let Ok(store) = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &terminal_id.to_string(),
        ) {
            // Only a non-authoritative (Preparing) record is invalidated here.
            let _ = store.recover();
        }
    }

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let (kind, argv) = self.prepare_agent_launch(&params)?;
        let name = params.name.clone();
        let Some((ws_idx, pane_id)) = self.parse_current_public_pane_id(&params.pane_id) else {
            return Err(AgentStartError::TargetNotFound(params.pane_id));
        };
        let terminal_id = self
            .state
            .workspaces
            .get(ws_idx)
            .and_then(|workspace| workspace.terminal_id(pane_id))
            .cloned()
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        let terminal = self
            .state
            .terminals
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetNotFound(params.pane_id.clone()))?;
        if terminal.is_agent_terminal() || terminal.managed_agent_kind().is_some() {
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let runtime = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        let shell_name = available_shell_name(runtime)
            .ok_or_else(|| AgentStartError::TargetBusy(params.pane_id.clone()))?;

        // Headless publishes the discovery address before any managed Pi launch.
        // It is a host-owned environment input only; the listener authenticates
        // the accepted stream and Active sender generation after connection.
        let launch_environment = if kind == crate::detect::Agent::Pi {
            self.pi_mailbox_bootstrap_launch_environment(&params.env)
        } else {
            params.env.clone()
        };
        let command =
            crate::platform::interactive_shell_command(&argv, &launch_environment, &shell_name)
                .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = self.agent_start_timeout(&params)?;
        // Persist the new generation, and for a Pi --session launch wait out the
        // birth-tick cutoff, before mutating launch state or sending bytes.
        let explicit_env: Vec<(String, String)> = params
            .env
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        let managed =
            self.prepare_managed_launch(&terminal_id, &name, kind, &argv, &explicit_env, |name| {
                launch_env_value(&params.env, name)
            })?;
        let generation = managed.generation;
        // The wait must not turn a previously shell-only pane into a launch
        // against a newly foregrounded Pi.
        let shell_unchanged = self
            .terminal_runtimes
            .get(&terminal_id)
            .and_then(available_shell_name)
            .as_deref()
            == Some(shell_name.as_str());
        if !shell_unchanged {
            self.abandon_managed_launch(&terminal_id, generation);
            return Err(AgentStartError::TargetBusy(params.pane_id));
        }
        let now = Instant::now();
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            self.abandon_managed_launch(&terminal_id, generation);
            return Err(AgentStartError::TargetUnavailable(params.pane_id));
        };
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        let recipe = self.commit_managed_launch(&terminal_id, managed);
        let sent = self
            .terminal_runtimes
            .get(&terminal_id)
            .ok_or_else(|| "terminal runtime disappeared".to_string())
            .and_then(|runtime| {
                runtime
                    .try_send_bytes(Bytes::from(bytes))
                    .map_err(|err| err.to_string())
            });
        if let Err(err) = sent {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.clear_agent_name();
            }
            self.abandon_managed_launch(&terminal_id, generation);
            return Err(AgentStartError::InputFailed(err));
        }
        self.finalize_managed_launch(&terminal_id, recipe);
        self.acknowledge_terminal_input(&terminal_id);
        self.state.mark_session_dirty();
        self.schedule_session_save();

        let agent = self
            .agent_info(ws_idx, pane_id)
            .ok_or(AgentStartError::TargetUnavailable(params.pane_id))?;
        Ok((agent, argv))
    }

    pub(super) fn agent_start_error_body(
        &self,
        err: AgentStartError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentStartError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentStartError::UnsupportedKind(kind) => crate::api::schema::ErrorBody {
                code: "unsupported_agent_kind".into(),
                message: format!("unsupported interactive agent kind {kind}"),
            },
            AgentStartError::InvalidArgument => crate::api::schema::ErrorBody {
                code: "invalid_agent_argument".into(),
                message: "agent arguments cannot be encoded safely for the target shell".into(),
            },
            AgentStartError::InvalidEnvironment => crate::api::schema::ErrorBody {
                code: "invalid_agent_environment".into(),
                message: "agent environment must contain at most 16 unique bounded NAME=VALUE assignments".into(),
            },
            AgentStartError::InvalidTimeout => crate::api::schema::ErrorBody {
                code: "invalid_agent_timeout".into(),
                message: INVALID_AGENT_TIMEOUT_MESSAGE.into(),
            },
            AgentStartError::TargetNotFound(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_not_found".into(),
                message: format!("agent target pane {target} not found"),
            },
            AgentStartError::TargetBusy(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_busy".into(),
                message: format!("agent target pane {target} is not an available shell"),
            },
            AgentStartError::TargetUnavailable(target) => crate::api::schema::ErrorBody {
                code: "agent_pane_unavailable".into(),
                message: format!("agent target pane {target} has no live terminal"),
            },
            AgentStartError::InputFailed(message) => crate::api::schema::ErrorBody {
                code: "agent_start_input_failed".into(),
                message,
            },
            AgentStartError::AuthorityPersistence(message) => crate::api::schema::ErrorBody {
                code: "agent_start_authority_persistence_failed".into(),
                message: format!("could not persist sender launch authority: {message}"),
            },
            AgentStartError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    pub(super) fn agent_target_error_body(
        &self,
        err: TerminalTargetError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            TerminalTargetError::NotFound { target } => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: format!("agent target {target} not found"),
            },
            TerminalTargetError::Ambiguous { target, candidates } => {
                crate::api::schema::ErrorBody {
                    code: "agent_target_ambiguous".into(),
                    message: format!(
                        "agent target {target} is ambiguous; candidates: {}",
                        candidates
                            .into_iter()
                            .map(|candidate| format!(
                                "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                                candidate.terminal_id,
                                candidate.pane_id,
                                candidate.workspace_id,
                                candidate.tab_id,
                                candidate.cwd.unwrap_or_else(|| "unknown".into()),
                                candidate.agent_status,
                            ))
                            .collect::<Vec<_>>()
                            .join("; ")
                    ),
                }
            }
        }
    }

    pub(super) fn agent_rename_error_body(
        &self,
        err: AgentRenameError,
    ) -> crate::api::schema::ErrorBody {
        match err {
            AgentRenameError::Target(err) => self.agent_target_error_body(err),
            AgentRenameError::InvalidName => crate::api::schema::ErrorBody {
                code: "invalid_agent_name".into(),
                message: INVALID_AGENT_NAME_MESSAGE.into(),
            },
            AgentRenameError::NotAgent => crate::api::schema::ErrorBody {
                code: "agent_not_found".into(),
                message: "agent target does not currently host an agent".into(),
            },
            AgentRenameError::PendingLaunch => crate::api::schema::ErrorBody {
                code: "agent_launch_pending".into(),
                message: "agent name cannot change while startup is pending".into(),
            },
            AgentRenameError::DuplicateName { name, candidates } => crate::api::schema::ErrorBody {
                code: "agent_name_taken".into(),
                message: format!(
                    "agent name {name} is already used; candidates: {}",
                    candidates
                        .into_iter()
                        .map(|candidate| format!(
                            "terminal_id={} pane_id={} workspace_id={} tab_id={} cwd={} status={:?}",
                            candidate.terminal_id,
                            candidate.pane_id,
                            candidate.workspace_id,
                            candidate.tab_id,
                            candidate.cwd.unwrap_or_else(|| "unknown".into()),
                            candidate.agent_status,
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            },
        }
    }

    /// Bind the successful server-owned start to the exact Pi process observed
    /// at its Active-generation transition. A pre-existing process, including
    /// one left in the foreground during a replacement, cannot satisfy the
    /// launch birth floor. No snapshot or client report can create this record.
    pub(crate) fn managed_pi_process_birth(
        &self,
        pid: u32,
    ) -> Option<crate::platform::ProcessBirthIdentity> {
        #[cfg(test)]
        if let Some(identity) = self.mailbox_bootstrap_test_process_births.get(&pid) {
            return Some(*identity);
        }
        crate::platform::process_birth_identity(pid)
    }

    pub(crate) fn bind_active_managed_pi_process(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        generation: u64,
    ) {
        let Some(launch) = self.managed_pi_launches.get(terminal_id) else {
            return;
        };
        if launch.generation != generation || launch.process.is_some() {
            return;
        }
        let floor = launch.earliest_birth_ticks;
        let Some(job) = self.mailbox_bootstrap_foreground_job(terminal_id) else {
            return;
        };
        let Some((crate::detect::Agent::Pi, process)) =
            crate::detect::identify_agent_process_in_job(&job)
        else {
            return;
        };
        let Some(birth) = self.managed_pi_process_birth(process.pid) else {
            return;
        };
        if birth.start_ticks < floor {
            return;
        }
        if let Some(launch) = self.managed_pi_launches.get_mut(terminal_id) {
            if launch.generation == generation && launch.process.is_none() {
                launch.process = Some(birth);
            }
        }
    }

    /// Derive Pi's identity on demand so no stopped or replaced process can
    /// leave a reusable cached path in agent get. The caller supplies only a
    /// target for lookup; neither the target nor a reported session is authority.
    pub(crate) fn trusted_managed_pi_session(
        &self,
        terminal: &crate::terminal::TerminalState,
    ) -> Option<crate::api::schema::AgentSessionInfo> {
        let key = terminal.id.to_string();
        let record = crate::sender_authority::SenderAuthorityStore::for_sender(
            &self.sender_authority_dir,
            &key,
        )
        .ok()?
        .load()
        .ok()??;
        if !record.authoritative()
            || record.sender_key != key
            || !terminal.accepts_managed_agent_generation(record.process_generation)
            || terminal.managed_agent_kind() != Some(crate::detect::Agent::Pi)
        {
            return None;
        }
        let job = self.mailbox_bootstrap_foreground_job(&terminal.id)?;
        let (agent, process) = crate::detect::identify_agent_process_in_job(&job)?;
        if agent != crate::detect::Agent::Pi || process.pid == 0 {
            return None;
        }
        let launch = self.managed_pi_launches.get(&terminal.id)?;
        if launch.generation != record.process_generation
            || launch.process? != self.managed_pi_process_birth(process.pid)?
        {
            return None;
        }
        let argv = process.argv.as_deref()?;
        // Pi's process.title can erase its argv, as observed in #90. If argv
        // still exposes a selector, it must agree exactly with the committed
        // launch; a malformed or conflicting selector always fails closed.
        if argv
            .iter()
            .any(|arg| arg == "--session" || arg.starts_with("--session="))
            && explicit_pi_session_path(argv).as_deref() != Some(&launch.session_path)
        {
            return None;
        }
        let value = &launch.session_path;
        let path = std::path::Path::new(value);
        if !crate::platform::verified_pi_session_jsonl(path) {
            return None;
        }
        Some(crate::api::schema::AgentSessionInfo {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: value.clone(),
        })
    }

    /// Identity shown by `agent.get` and matched by handoffs and assignments.
    ///
    /// A trusted managed Pi launch always wins and is marked `managed`. A live
    /// Pi without one shows its pane-reported session exactly as herdr 0.8.4
    /// did (herdr-agent-state's `pane.report_agent_session`), marked
    /// `reported`. A reported session never grants mailbox, bound-report or
    /// route authority: those read `trusted_managed_pi_session` directly. A
    /// pane that no longer runs Pi shows nothing for a former managed Pi.
    fn agent_session_for_info(
        &self,
        terminal: &crate::terminal::TerminalState,
        pi_label: bool,
        reported: Option<crate::api::schema::AgentSessionInfo>,
    ) -> (
        Option<crate::api::schema::AgentSessionInfo>,
        Option<crate::api::schema::AgentSessionTrust>,
    ) {
        use crate::api::schema::AgentSessionTrust;
        let managed_pi = terminal.managed_agent_kind() == Some(crate::detect::Agent::Pi);
        if managed_pi || pi_label {
            if let Some(trusted) = self.trusted_managed_pi_session(terminal) {
                return (Some(trusted), Some(AgentSessionTrust::Managed));
            }
            if !pi_label {
                return (None, None);
            }
        }
        let trust = reported.as_ref().map(|_| AgentSessionTrust::Reported);
        (reported, trust)
    }

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        self.agent_info_with_sleeping(ws_idx, pane_id, false)
    }

    /// As [`Self::agent_info`]; with `sleeping`, a pane Herdr put to sleep is
    /// described too, under its sleeping agent's name.
    pub(crate) fn agent_info_with_sleeping(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        sleeping: bool,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        let asleep = sleeping && terminal.sleep.is_some();
        if !terminal.is_agent_terminal() && !asleep {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        let pi_label = pane.agent.as_deref() == Some("pi");
        let (agent_session, agent_session_trust) =
            self.agent_session_for_info(terminal, pi_label, pane.agent_session.clone());
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone().or_else(|| {
                asleep
                    .then(|| {
                        terminal
                            .sleep
                            .as_ref()
                            .map(|sleep| sleep.agent_name.clone())
                    })
                    .flatten()
            }),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            agent_session,
            agent_session_trust,
            workspace_id: pane.workspace_id,
            tab_id: pane.tab_id,
            pane_id: pane.pane_id,
            focused: pane.focused,
            launch_pending: terminal.managed_agent_launch_pending(),
            interactive_ready: terminal.managed_agent_interactive_ready(),
            state_change_seq: terminal.last_agent_state_change_seq.unwrap_or(0),
            cwd: pane.cwd,
            foreground_cwd: pane.foreground_cwd,
            revision: pane.revision,
        })
    }

    fn agent_name_conflicts(
        &self,
        name: &str,
        except_terminal_id: &str,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.collect_agent_infos()
            .into_iter()
            .filter(|agent| {
                agent.name.as_deref() == Some(name) && agent.terminal_id != except_terminal_id
            })
            .collect()
    }
}

fn available_shell_name(runtime: &crate::terminal::TerminalRuntime) -> Option<String> {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return Some("sh".into());
    }
    crate::platform::available_pane_shell(runtime.child_pid()?)
}

/// A live agent (e.g. Pi) is the pane's foreground job.
pub(super) fn runtime_has_live_agent(runtime: &crate::terminal::TerminalRuntime) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return false;
    }
    live_runtime_agent(runtime).is_some()
}

/// The pane's shell is at an idle prompt, as agent.start requires.
pub(super) fn runtime_at_idle_shell(runtime: &crate::terminal::TerminalRuntime) -> bool {
    available_shell_name(runtime).is_some()
}

pub(super) fn runtime_hosts_agent(
    runtime: &crate::terminal::TerminalRuntime,
    expected: crate::detect::Agent,
) -> bool {
    #[cfg(test)]
    if runtime.child_pid().is_none() {
        return true;
    }
    live_runtime_agent(runtime) == Some(expected)
}

fn live_runtime_agent(runtime: &crate::terminal::TerminalRuntime) -> Option<crate::detect::Agent> {
    let job = crate::detect::foreground_job(runtime.child_pid()?)?;
    crate::detect::identify_agent_in_job(&job)
        .map(|(agent, _)| agent)
        .or_else(|| {
            job.processes
                .iter()
                .find_map(|process| crate::platform::process_agent_hint(process.pid))
        })
}

pub(super) enum AgentStartError {
    InvalidName,
    UnsupportedKind(String),
    InvalidArgument,
    InvalidEnvironment,
    InvalidTimeout,
    TargetNotFound(String),
    TargetBusy(String),
    TargetUnavailable(String),
    InputFailed(String),
    AuthorityPersistence(String),
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

pub(super) enum AgentRenameError {
    Target(TerminalTargetError),
    InvalidName,
    NotAgent,
    PendingLaunch,
    DuplicateName {
        name: String,
        candidates: Vec<crate::api::schema::AgentInfo>,
    },
}

#[cfg(test)]
mod tests {
    use super::{valid_agent_environment, valid_agent_name};

    #[test]
    fn agent_names_use_a_small_cli_safe_grammar() {
        for name in ["a", "reviewer-one", "reviewer_2", &"a".repeat(32)] {
            assert!(valid_agent_name(name), "expected {name:?} to be valid");
        }
        for name in [
            "",
            " reviewer",
            "reviewer ",
            "reviewer one",
            "Reviewer",
            "1reviewer",
            "reviewer.one",
            &"a".repeat(33),
        ] {
            assert!(!valid_agent_name(name), "expected {name:?} to be invalid");
        }
    }

    #[test]
    fn agent_environment_is_bounded_unique_and_shell_safe() {
        assert!(valid_agent_environment(&[
            "PI_TASKING_HERDR_ADAPTER_CONFIG=/home/user/adapter.json".into()
        ]));
        for invalid in [
            vec!["NO_EQUALS".into()],
            vec!["1BAD=value".into()],
            vec!["BAD-NAME=value".into()],
            vec!["DUP=one".into(), "DUP=two".into()],
            vec!["DUP=one".into(), "dup=two".into()],
            vec!["CONTROL=bad\nvalue".into()],
            vec![format!("TOO_LONG={}", "x".repeat(4097))],
            (0..17).map(|index| format!("KEY_{index}=value")).collect(),
        ] {
            assert!(
                !valid_agent_environment(&invalid),
                "expected {invalid:?} to be invalid"
            );
        }
    }
}
