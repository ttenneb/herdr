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

fn explicit_pi_session_path(argv: &[String]) -> Option<String> {
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
            let conflicts = self.agent_name_conflicts(name, &resolved.terminal_id);
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
        let conflicts = self.agent_name_conflicts(&params.name, "");
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

    pub(super) fn start_agent(
        &mut self,
        params: AgentStartParams,
    ) -> Result<(crate::api::schema::AgentInfo, Vec<String>), AgentStartError> {
        let (kind, mut argv) = self.prepare_agent_launch(&params)?;
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
        // #145: a covered child runs the pinned bwrap argv; the sandbox's
        // --clearenv discards the shell environment, so none is passed.
        let covered = params
            .covered
            .as_ref()
            .map(|covered| self.covered_launch_argv(kind, &argv, covered, &params.env))
            .transpose()?;
        let command = match &covered {
            Some((sandboxed, _)) => {
                crate::platform::interactive_shell_command(sandboxed, &[], &shell_name)
            }
            None => {
                crate::platform::interactive_shell_command(&argv, &launch_environment, &shell_name)
            }
        }
        .ok_or(AgentStartError::InvalidArgument)?;
        let bytes = crate::app::api_helpers::encode_api_submission(runtime, &command);
        let timeout = self.agent_start_timeout(&params)?;
        // Persist the new generation before mutating launch state or sending
        // bytes. A failed clock wait below aborts without submitting input.
        let process_generation =
            self.allocate_sender_authority_generation(terminal_id.to_string())?;

        // A process born in this coarse kernel tick is ambiguous. Sample the
        // strict cutoff, then wait for that tick before submitting the command
        // so even a fast legitimate Pi is not penalized by the cutoff.
        let managed_pi_launch = (kind == crate::detect::Agent::Pi)
            .then(|| {
                explicit_pi_session_path(&argv).zip(crate::platform::first_post_launch_birth_tick())
            })
            .flatten();
        self.managed_pi_launches.remove(&terminal_id);
        self.pending_covered_launches.remove(&terminal_id);
        self.covered_child_launches.remove(&terminal_id);
        if covered.is_some() && managed_pi_launch.is_none() {
            return Err(AgentStartError::CoveredLaunch(
                "an explicit --session and a process birth floor are required",
            ));
        }
        if let Some((_, cutoff)) = managed_pi_launch.as_ref() {
            if !crate::platform::wait_until_birth_tick(*cutoff) {
                return Err(AgentStartError::AuthorityPersistence(
                    "process birth clock did not advance before launch".into(),
                ));
            }
            // The wait must not turn a previously shell-only pane into a
            // launch against a newly foregrounded Pi.
            if available_shell_name(runtime).as_deref() != Some(shell_name.as_str()) {
                return Err(AgentStartError::TargetBusy(params.pane_id));
            }
        }
        let now = Instant::now();
        let terminal = self
            .state
            .terminals
            .get_mut(&terminal_id)
            .ok_or_else(|| AgentStartError::TargetUnavailable(params.pane_id.clone()))?;
        runtime.set_managed_agent_generation(process_generation);
        terminal.begin_managed_agent(name.clone(), kind, now, AGENT_START_SETTLE_DELAY, timeout);
        terminal.set_managed_agent_generation(process_generation);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            terminal.clear_agent_name();
            runtime.set_managed_agent_generation(0);
            return Err(AgentStartError::InputFailed(err.to_string()));
        }
        if let Some((sandboxed, pending)) = covered {
            self.pending_covered_launches.insert(
                terminal_id.clone(),
                crate::covered_launch::PendingCoveredLaunch {
                    generation: process_generation,
                    ..pending
                },
            );
            argv = sandboxed;
        }
        if let Some((session_path, earliest_birth_ticks)) = managed_pi_launch {
            self.managed_pi_launches.insert(
                terminal_id.clone(),
                ManagedPiLaunch {
                    generation: process_generation,
                    session_path,
                    earliest_birth_ticks,
                    process: None,
                },
            );
        }
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
            AgentStartError::CoveredLaunch(reason) => crate::api::schema::ErrorBody {
                code: "covered_launch_unavailable".into(),
                message: format!("covered child launch refused: {reason}"),
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
        // #145 seam: the receipt names this exact managed-launch birth and is
        // registered before any route for the child can become ready.
        if let Some(pending) = self
            .pending_covered_launches
            .remove(terminal_id)
            .filter(|pending| pending.generation == generation)
        {
            let receipt = crate::covered_launch::CoveredLaunchReceipt {
                kind: "herdr.covered-launch-receipt",
                version: 1,
                policy_id: crate::covered_launch::POLICY_ID,
                argv_sha256: pending.argv_sha256,
                env_keys: pending.env_keys,
                launch_birth: birth.into(),
                launch_floor_ticks: floor,
            };
            if !self.register_covered_child_launch(terminal_id.clone(), receipt.policy()) {
                tracing::warn!("covered child launch could not be registered; it stays uncovered");
            }
        }
    }

    /// Build the pinned sandbox argv for a covered Pi launch.
    fn covered_launch_argv(
        &self,
        kind: crate::detect::Agent,
        argv: &[String],
        covered: &crate::covered_launch::CoveredLaunchParams,
        requested_env: &[String],
    ) -> Result<(Vec<String>, crate::covered_launch::PendingCoveredLaunch), AgentStartError> {
        if kind != crate::detect::Agent::Pi {
            return Err(AgentStartError::CoveredLaunch(
                "only a managed Pi can be covered",
            ));
        }
        if !self.child_report_signals_enabled || !cfg!(target_os = "linux") {
            return Err(AgentStartError::CoveredLaunch(
                "child report signals are disabled or unsupported on this platform",
            ));
        }
        if !requested_env.is_empty() {
            return Err(AgentStartError::CoveredLaunch(
                "caller environment cannot enter the sandbox",
            ));
        }
        let address = self
            .mailbox_bootstrap_discovery_address
            .clone()
            .filter(|address| !address.is_empty())
            .ok_or(AgentStartError::CoveredLaunch(
                "mailbox bootstrap is not listening",
            ))?;
        let env = crate::covered_launch::child_env(
            &covered.root,
            [
                (
                    crate::app::mailbox::PI_MAILBOX_BOOTSTRAP_ADDRESS_ENV.to_string(),
                    address.clone(),
                ),
                ("HERDR_AGENT".to_string(), "pi".to_string()),
            ],
        )
        .map_err(|error| AgentStartError::CoveredLaunch(error.0))?;
        #[cfg(unix)]
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        #[cfg(not(unix))]
        let (uid, gid) = (0, 0);
        let sandboxed = crate::covered_launch::bwrap_argv(
            covered,
            &[address],
            &env,
            argv.get(1..).unwrap_or_default(),
            uid,
            gid,
        )
        .map_err(|error| AgentStartError::CoveredLaunch(error.0))?;
        let pending = crate::covered_launch::PendingCoveredLaunch {
            generation: 0,
            argv_sha256: crate::covered_launch::policy_hash(&sandboxed),
            env_keys: env.keys().cloned().collect(),
        };
        Ok((sandboxed, pending))
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

    pub(super) fn agent_info(
        &self,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
    ) -> Option<crate::api::schema::AgentInfo> {
        let ws = self.state.workspaces.get(ws_idx)?;
        let pane_state = ws.pane_state(pane_id)?;
        let terminal = self.state.terminals.get(&pane_state.attached_terminal_id)?;
        if !terminal.is_agent_terminal() {
            return None;
        }
        let pane = self.pane_info(ws_idx, pane_id)?;
        let pi_label = pane.agent.as_deref() == Some("pi");
        Some(crate::api::schema::AgentInfo {
            terminal_id: pane.terminal_id,
            name: terminal.agent_name.clone(),
            agent: pane.agent,
            title: pane.title,
            terminal_title: pane.terminal_title,
            terminal_title_stripped: pane.terminal_title_stripped,
            display_agent: pane.display_agent,
            agent_status: pane.agent_status,
            screen_detection_skipped: terminal.full_lifecycle_hook_authority_active(),
            state_labels: pane.state_labels,
            tokens: pane.tokens,
            // Managed Pi identity is derived from this execution, not from a
            // pane-scoped report that any local API client could have supplied.
            agent_session: if terminal.managed_agent_kind() == Some(crate::detect::Agent::Pi)
                || pi_label
            {
                // A stopped managed Pi must not fall back to a stale pane
                // report merely because its managed name was cleared.
                self.trusted_managed_pi_session(terminal)
            } else {
                pane.agent_session
            },
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
    /// #145: a covered launch was requested but cannot be established.
    CoveredLaunch(&'static str),
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
