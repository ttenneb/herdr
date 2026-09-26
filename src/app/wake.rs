//! Herdr-owned sleep and wake for managed agent panes.
//!
//! `agent.sleep` records that Herdr put a pane's agent to sleep and sends a
//! guarded ctrl+d. [`App::wake_pane`] relaunches that agent in the same pane
//! and terminal from its persisted [`LaunchRecipe`], through the shared managed
//! launch path (`start_agent` → prepare/commit managed launch). The woken Pi
//! resumes its `--session` and its Gate drains the Herdr queue, so no prompt is
//! sent.
//!
//! Only panes Herdr itself put to sleep are woken. Never woken: a Pi the human
//! quit by hand (no sleep record), hand-typed `pi` (no recipe), Collection
//! helpers (their pane closes when Pi exits, so they cannot sleep), and panes
//! launched by a lifecycle role manager (`HERDR_LIFECYCLE_ROLE`).
//!
//! [`LaunchRecipe`]: crate::launch_recipe::LaunchRecipe

use std::time::{Duration, Instant};

use super::App;
use crate::launch_recipe::PaneSleep;
use crate::terminal::TerminalId;

/// How long a started wake may take to attach to Messages before it counts as
/// failed. One retry with a fresh wake ID follows.
pub(crate) const WAKE_ATTACH_TIMEOUT: Duration = Duration::from_secs(60);
/// After a failed or timed-out wake, further wakes for the pane wait this long.
pub(crate) const WAKE_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
/// A slept parent has no live generation or trusted session, so its children's
/// bound report routes would stop working; such a pane never sleeps.
pub(crate) const PARENT_OF_ACTIVE_ROUTES: &str = "parent of active delegation routes; not sleeping";

/// Why the mailbox asks for a wake. Both are level-triggered: the caller asks
/// whenever a slept pane has unsettled heads; wake_pane refuses or coalesces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WakeCause {
    /// A head was appended for the pane's recipient.
    HeadAppended,
    /// After a server restart, the mailbox sweeps slept panes that still have
    /// unsettled heads.
    RestoreBacklog,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WakeTrigger {
    pub cause: WakeCause,
    pub recipient_id: String,
    pub head_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WakeOutcome {
    Started {
        wake_id: String,
        generation: u64,
    },
    /// Coalesced into the outstanding wake for the pane.
    Duplicate {
        wake_id: String,
    },
    Refused {
        wake_id: String,
        reason: WakeRefusal,
    },
    /// `start_agent` failed; `error` is its error code and message.
    Failed {
        wake_id: String,
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WakeRefusal {
    UnknownPane,
    NotSleeping,
    NoRecipe,
    LifecycleOwned,
    PiAttached,
    PaneNotAtIdleShell,
    CoolingDown { retry_after_ms: u64 },
    ParentOfActiveRoutes,
}

impl WakeRefusal {
    fn code(&self) -> &'static str {
        match self {
            Self::UnknownPane => "unknown_pane",
            Self::NotSleeping => "not_sleeping",
            Self::NoRecipe => "no_recipe",
            Self::LifecycleOwned => "lifecycle_owned",
            Self::PiAttached => "pi_attached",
            Self::PaneNotAtIdleShell => "pane_not_at_idle_shell",
            Self::CoolingDown { .. } => "cooling_down",
            Self::ParentOfActiveRoutes => "parent_of_active_routes",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OutstandingWake {
    pub wake_id: String,
    pub pane_key: String,
    pub generation: u64,
    pub trigger: WakeTrigger,
    pub deadline: Instant,
    pub retried: bool,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn new_wake_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    crate::platform::random_route_epoch().unwrap_or_else(|| {
        let counter = FALLBACK.fetch_add(1, Ordering::Relaxed);
        format!(
            "{:016x}{:016x}",
            now_ms() ^ u64::from(std::process::id()),
            counter
        )
    })
}

impl App {
    /// The restart-stable pane key is resolved here and only here. The key is
    /// the pane's durable Messages queue key (`TerminalState::queue_key`,
    /// also accepted as its `pane:<queueKey>` recipient ID), which survives
    /// Pi restarts, sleep and server restarts. A current public pane ID is
    /// still accepted for callers that address a pane directly.
    pub(crate) fn resolve_wake_pane_key(
        &self,
        pane_key: &str,
    ) -> Option<(usize, crate::layout::PaneId, TerminalId)> {
        let queue_key = pane_key.strip_prefix("pane:").unwrap_or(pane_key);
        if let Some(terminal) = self.state.terminals.values().find(|terminal| {
            crate::terminal::state::valid_queue_key(queue_key) && terminal.queue_key == queue_key
        }) {
            let (ws_idx, pane_id) = self.pane_of_terminal(&terminal.id)?;
            return Some((ws_idx, pane_id, terminal.id.clone()));
        }
        let (ws_idx, pane_id) = self.parse_current_public_pane_id(pane_key)?;
        let terminal_id = self.state.workspaces.get(ws_idx)?.terminal_id(pane_id)?;
        Some((ws_idx, pane_id, terminal_id.clone()))
    }

    fn pane_of_terminal(&self, terminal_id: &TerminalId) -> Option<(usize, crate::layout::PaneId)> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| &pane.attached_terminal_id == terminal_id)
                        .map(|(pane_id, _)| (ws_idx, *pane_id))
                })
            })
    }

    /// Whether the pane of this terminal is the parent of a ready delegation
    /// route, or of a live child delegation (a child pane with an agent,
    /// running or asleep).
    pub(crate) fn terminal_parents_active_routes(&self, terminal_id: &TerminalId) -> bool {
        let pane_of = |terminal: &TerminalId| {
            self.state.workspaces.iter().find_map(|workspace| {
                workspace.tabs.iter().find_map(|tab| {
                    tab.panes
                        .iter()
                        .find(|(_, pane)| &pane.attached_terminal_id == terminal)
                        .map(|(pane_id, _)| *pane_id)
                })
            })
        };
        let Some(pane) = pane_of(terminal_id) else {
            return false;
        };
        if self
            .ready_delegation_routes
            .values()
            .any(|route| route.parent_pane == pane)
        {
            return true;
        }
        let Some(parent) = self
            .state
            .delegations
            .delegation_for_pane(pane)
            .filter(|record| !record.tombstone)
            .map(|record| record.id)
        else {
            return false;
        };
        self.state.delegations.records().values().any(|child| {
            child.parent_id == Some(parent)
                && !child.tombstone
                && child.pane_id.is_some_and(|child_pane| {
                    self.find_pane(child_pane)
                        .and_then(|(ws_idx, _)| {
                            self.state.workspaces[ws_idx].terminal_id(child_pane)
                        })
                        .and_then(|terminal| self.state.terminals.get(terminal))
                        .is_some_and(|terminal| {
                            terminal.is_agent_terminal() || terminal.sleep.is_some()
                        })
                })
        })
    }

    /// Sleeping agents holding `name`, except in `except_terminal`.
    pub(crate) fn sleeping_name_conflicts(
        &self,
        name: &str,
        except_terminal: Option<&TerminalId>,
    ) -> Vec<crate::api::schema::AgentInfo> {
        self.state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, workspace)| {
                workspace.tabs.iter().flat_map(move |tab| {
                    tab.panes.iter().map(move |(pane_id, pane)| {
                        (ws_idx, *pane_id, pane.attached_terminal_id.clone())
                    })
                })
            })
            .filter(|(_, _, terminal_id)| Some(terminal_id) != except_terminal)
            .filter(|(_, _, terminal_id)| {
                self.state
                    .terminals
                    .get(terminal_id)
                    .is_some_and(|terminal| {
                        terminal.agent_name.is_none()
                            && terminal
                                .sleep
                                .as_ref()
                                .is_some_and(|sleep| sleep.agent_name == name)
                    })
            })
            .filter_map(|(ws_idx, pane_id, _)| self.agent_info_with_sleeping(ws_idx, pane_id, true))
            .collect()
    }

    /// Any live agent appearing in a slept pane other than the one a wake is
    /// starting (for example a hand-typed `pi`) ends the Herdr sleep, so
    /// prompts route normally again instead of queueing unread.
    pub(crate) fn end_sleep_on_live_agent(&mut self, pane_id: crate::layout::PaneId) {
        let Some(terminal_id) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.state.workspaces[ws_idx].terminal_id(pane_id).cloned())
        else {
            return;
        };
        if self.pane_wakes.contains_key(&terminal_id) {
            return;
        }
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return;
        };
        // While the slept agent itself is still exiting it keeps its name.
        if terminal.sleep.is_some() && terminal.agent_name.is_none() {
            terminal.sleep = None;
            self.state.mark_session_dirty();
            self.schedule_session_save();
        }
    }

    fn pane_wake_dir(&self) -> std::path::PathBuf {
        self.sender_authority_dir.join("pane-wakes")
    }

    /// One owner-only JSON record per wake ID. A wake ID with an existing
    /// record is never launched again.
    fn write_pane_wake_record(&self, wake_id: &str, record: &serde_json::Value) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = self.pane_wake_dir();
        if let Err(err) = std::fs::create_dir_all(&dir) {
            tracing::warn!(%err, "cannot create pane wake record directory");
            return;
        }
        let path = dir.join(format!("{wake_id}.json"));
        let tmp = dir.join(format!(".{wake_id}.tmp"));
        let result = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut file| {
                file.write_all(&serde_json::to_vec(record).unwrap_or_default())?;
                file.sync_all()
            })
            .and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(err) = result {
            tracing::warn!(%err, wake_id, "cannot write pane wake record");
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pane_wake_record_exists(&self, wake_id: &str) -> bool {
        self.pane_wake_dir()
            .join(format!("{wake_id}.json"))
            .exists()
    }

    #[allow(clippy::too_many_arguments)]
    fn record_pane_wake(
        &self,
        wake_id: &str,
        pane_key: &str,
        terminal_id: Option<&TerminalId>,
        trigger: &WakeTrigger,
        outcome: &str,
        reason: Option<String>,
        generation: Option<u64>,
        requested_at_ms: u64,
    ) {
        self.write_pane_wake_record(
            wake_id,
            &serde_json::json!({
                "version": 1,
                "wakeId": wake_id,
                "paneKey": pane_key,
                "terminalId": terminal_id.map(ToString::to_string),
                "trigger": trigger,
                "outcome": outcome,
                "reason": reason,
                "generation": generation,
                "requestedAtMs": requested_at_ms,
                "resolvedAtMs": now_ms(),
            }),
        );
    }

    /// Wake a pane that Herdr put to sleep. Non-blocking (it never waits for
    /// Pi readiness) and single-flight per pane: while one wake is
    /// outstanding, further calls coalesce into it. Every call writes one
    /// durable record.
    pub(crate) fn wake_pane(&mut self, pane_key: &str, trigger_head: WakeTrigger) -> WakeOutcome {
        self.wake_pane_attempt(pane_key, trigger_head, false)
    }

    fn wake_pane_attempt(
        &mut self,
        pane_key: &str,
        trigger: WakeTrigger,
        retry: bool,
    ) -> WakeOutcome {
        let wake_id = new_wake_id();
        let requested_at_ms = now_ms();
        let refuse = |app: &App, terminal: Option<&TerminalId>, reason: WakeRefusal| {
            app.record_pane_wake(
                &wake_id,
                pane_key,
                terminal,
                &trigger,
                "refused",
                Some(reason.code().to_string()),
                None,
                requested_at_ms,
            );
            WakeOutcome::Refused {
                wake_id: wake_id.clone(),
                reason,
            }
        };
        let Some((ws_idx, pane_id, terminal_id)) = self.resolve_wake_pane_key(pane_key) else {
            return refuse(self, None, WakeRefusal::UnknownPane);
        };
        let now = Instant::now();
        if let Some(outstanding) = self.pane_wakes.get(&terminal_id) {
            let coalesced = outstanding.wake_id.clone();
            self.record_pane_wake(
                &wake_id,
                pane_key,
                Some(&terminal_id),
                &trigger,
                "duplicate",
                Some(format!("coalesced into {coalesced}")),
                Some(outstanding.generation),
                requested_at_ms,
            );
            return WakeOutcome::Duplicate { wake_id: coalesced };
        }
        if let Some(until) = self.pane_wake_cooldowns.get(&terminal_id).copied() {
            if now < until {
                let retry_after_ms = until.duration_since(now).as_millis() as u64;
                return refuse(
                    self,
                    Some(&terminal_id),
                    WakeRefusal::CoolingDown { retry_after_ms },
                );
            }
            self.pane_wake_cooldowns.remove(&terminal_id);
        }
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return refuse(self, Some(&terminal_id), WakeRefusal::UnknownPane);
        };
        if terminal.sleep.is_none() {
            return refuse(self, Some(&terminal_id), WakeRefusal::NotSleeping);
        }
        let Some(recipe) = terminal.launch_recipe.clone() else {
            return refuse(self, Some(&terminal_id), WakeRefusal::NoRecipe);
        };
        if recipe.lifecycle_role.is_some() {
            return refuse(self, Some(&terminal_id), WakeRefusal::LifecycleOwned);
        }
        if retry && self.terminal_parents_active_routes(&terminal_id) {
            return refuse(self, Some(&terminal_id), WakeRefusal::ParentOfActiveRoutes);
        }
        let runtime = self.terminal_runtimes.get(&terminal_id);
        if runtime.is_some_and(super::agents::runtime_has_live_agent) {
            return refuse(self, Some(&terminal_id), WakeRefusal::PiAttached);
        }
        if !runtime.is_some_and(super::agents::runtime_at_idle_shell) {
            return refuse(self, Some(&terminal_id), WakeRefusal::PaneNotAtIdleShell);
        }
        let Some(public_pane_id) = self.public_pane_id(ws_idx, pane_id) else {
            return refuse(self, Some(&terminal_id), WakeRefusal::UnknownPane);
        };

        // Durable before any launch: this wake ID can never launch twice.
        self.record_pane_wake(
            &wake_id,
            pane_key,
            Some(&terminal_id),
            &trigger,
            "requested",
            None,
            None,
            requested_at_ms,
        );
        // Marks the start as a wake, so committing it keeps the sleep record
        // until the woken Pi attaches.
        self.pane_wakes.insert(
            terminal_id.clone(),
            OutstandingWake {
                wake_id: wake_id.clone(),
                pane_key: pane_key.to_string(),
                generation: 0,
                trigger: trigger.clone(),
                deadline: now + WAKE_ATTACH_TIMEOUT,
                retried: retry,
            },
        );
        // A previous generation's name may still be registered to this pane.
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.clear_agent_name();
        }
        self.recipe_relaunches.insert(terminal_id.clone());
        let started = self.start_agent(crate::api::schema::AgentStartParams {
            name: recipe.name.clone(),
            kind: recipe.kind.clone(),
            pane_id: public_pane_id,
            args: recipe.args.clone(),
            env: recipe.env_assignments(),
            timeout_ms: None,
        });
        self.recipe_relaunches.remove(&terminal_id);
        match started {
            Ok(_) => {
                let generation = self
                    .state
                    .terminals
                    .get(&terminal_id)
                    .and_then(crate::terminal::TerminalState::managed_agent_generation)
                    .unwrap_or(0);
                if let Some(outstanding) = self.pane_wakes.get_mut(&terminal_id) {
                    outstanding.generation = generation;
                }
                self.record_pane_wake(
                    &wake_id,
                    pane_key,
                    Some(&terminal_id),
                    &trigger,
                    "started",
                    None,
                    Some(generation),
                    requested_at_ms,
                );
                self.state.mark_session_dirty();
                self.schedule_session_save();
                WakeOutcome::Started {
                    wake_id,
                    generation,
                }
            }
            Err(err) => {
                self.pane_wakes.remove(&terminal_id);
                self.pane_wake_cooldowns
                    .insert(terminal_id.clone(), now + WAKE_FAILURE_COOLDOWN);
                let body = self.agent_start_error_body(err);
                let error = format!("{}: {}", body.code, body.message);
                self.record_pane_wake(
                    &wake_id,
                    pane_key,
                    Some(&terminal_id),
                    &trigger,
                    "failed",
                    Some(error.clone()),
                    None,
                    requested_at_ms,
                );
                WakeOutcome::Failed { wake_id, error }
            }
        }
    }

    /// A managed generation was observed Active (its Pi attached to Messages):
    /// the outstanding wake for the pane is resolved and its sleep ends.
    pub(crate) fn resolve_pane_wake_on_active(
        &mut self,
        terminal_id: &TerminalId,
        generation: u64,
    ) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            if terminal.sleep.take().is_some() {
                self.state.mark_session_dirty();
                self.schedule_session_save();
            }
        }
        if self
            .pane_wakes
            .get(terminal_id)
            .is_some_and(|outstanding| outstanding.generation == generation)
        {
            let outstanding = self.pane_wakes.remove(terminal_id).expect("present");
            self.record_pane_wake(
                &outstanding.wake_id,
                &outstanding.pane_key,
                Some(terminal_id),
                &outstanding.trigger,
                "started",
                Some("attached".into()),
                Some(generation),
                now_ms(),
            );
        }
    }

    pub(crate) fn next_pane_wake_deadline(&self) -> Option<Instant> {
        self.pane_wakes.values().map(|wake| wake.deadline).min()
    }

    /// Outstanding wakes whose Pi never attached count as failed. Each gets
    /// one retry with a fresh wake ID; the second failure starts the cooldown.
    pub(crate) fn handle_pane_wake_deadlines(&mut self, now: Instant) -> bool {
        let expired: Vec<TerminalId> = self
            .pane_wakes
            .iter()
            .filter(|(_, wake)| now >= wake.deadline)
            .map(|(terminal, _)| terminal.clone())
            .collect();
        for terminal_id in &expired {
            let Some(outstanding) = self.pane_wakes.remove(terminal_id) else {
                continue;
            };
            self.record_pane_wake(
                &outstanding.wake_id,
                &outstanding.pane_key,
                Some(terminal_id),
                &outstanding.trigger,
                "failed",
                Some("no Messages attach before the wake deadline".into()),
                Some(outstanding.generation),
                now_ms(),
            );
            if outstanding.retried {
                self.pane_wake_cooldowns
                    .insert(terminal_id.clone(), now + WAKE_FAILURE_COOLDOWN);
            } else {
                let _ = self.wake_pane_attempt(&outstanding.pane_key, outstanding.trigger, true);
            }
        }
        !expired.is_empty()
    }

    /// Why `agent.sleep` must refuse this terminal, or the agent name and
    /// generation it would record. Only an idle, non-lifecycle managed agent
    /// with a recipe, hosted by a shell pane, can sleep.
    pub(crate) fn pane_sleep_candidate(
        &self,
        terminal_id: &TerminalId,
    ) -> Result<(String, u64), &'static str> {
        let terminal = self
            .state
            .terminals
            .get(terminal_id)
            .ok_or("the pane has no managed agent")?;
        let recipe = terminal.launch_recipe.as_ref().ok_or(
            "no launch recipe: only agents started by agent.start or helper-launch can sleep",
        )?;
        if recipe.lifecycle_role.is_some() {
            return Err("a lifecycle role manager owns this agent");
        }
        if self.terminal_parents_active_routes(terminal_id) {
            return Err(PARENT_OF_ACTIVE_ROUTES);
        }
        if terminal.launch_argv.is_some() {
            return Err("Collection helpers cannot sleep: their pane closes when the agent exits");
        }
        if matches!(
            terminal.state,
            crate::detect::AgentState::Working | crate::detect::AgentState::Blocked
        ) {
            return Err("the agent is working or blocked; sleep only an idle agent");
        }
        let agent_name = terminal
            .agent_name
            .clone()
            .ok_or("the pane has no managed agent")?;
        Ok((agent_name, terminal.managed_agent_generation().unwrap_or(0)))
    }

    pub(crate) fn set_pane_sleep(&mut self, terminal_id: &TerminalId, sleep: Option<PaneSleep>) {
        if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
            terminal.sleep = sleep;
        }
        self.state.mark_session_dirty();
        self.schedule_session_save();
    }

    pub(crate) fn new_pane_sleep(agent_name: String, generation: u64) -> PaneSleep {
        PaneSleep {
            since_ms: now_ms(),
            agent_name,
            generation,
        }
    }
}

impl App {
    /// The terminal whose pane inbox covers this mailbox recipient ID (its
    /// `pane:<queueKey>` or its legacy terminal key).
    pub(crate) fn terminal_for_recipient(&self, recipient_id: &str) -> Option<String> {
        let queue_key = recipient_id.strip_prefix("pane:");
        self.state
            .terminals
            .values()
            .find(|terminal| match queue_key {
                Some(key) => terminal.queue_key == key,
                None => terminal.id.to_string() == recipient_id,
            })
            .map(|terminal| terminal.id.to_string())
    }

    /// The terminal of a handoff recipient that Herdr put to sleep, if the
    /// envelope names exactly that pane, terminal and the session file its
    /// recipe resumes (the woken Pi keeps that identity).
    pub(crate) fn sleeping_recipient_terminal(
        &self,
        identity: &crate::api::schema::CanonicalHerdrIdentity,
    ) -> Option<TerminalId> {
        let (ws_idx, pane_id) = self.parse_current_public_pane_id(&identity.pane_id)?;
        let pane = self.pane_info(ws_idx, pane_id)?;
        if pane.workspace_id != identity.workspace_id || pane.terminal_id != identity.terminal_id {
            return None;
        }
        let terminal_id = self.state.workspaces[ws_idx].terminal_id(pane_id)?.clone();
        let terminal = self.state.terminals.get(&terminal_id)?;
        terminal.sleep.as_ref()?;
        let recipe = terminal.launch_recipe.as_ref()?;
        let session = super::agents::explicit_pi_session_path(&recipe.args)?;
        (identity.agent_session.agent == "pi" && identity.agent_session.value == session)
            .then_some(terminal_id)
    }

    /// The first unsettled head in the pane's inbox (its queue key and the
    /// legacy terminal key).
    #[cfg(test)]
    fn first_unsettled_head(&self, terminal_id: &TerminalId) -> Option<String> {
        let store = crate::mailbox::MailboxStore::open(&self.sender_authority_dir).ok()?;
        let recovered = store.load().ok()?;
        self.inbox_recipients(&terminal_id.to_string())
            .iter()
            .filter_map(|recipient| crate::mailbox_v1::snapshot(&recovered, recipient).ok())
            .flat_map(|snapshot| snapshot.head_states)
            .find(|state| state.lifecycle != crate::mailbox_v1::HeadLifecycle::Settled)
            .map(|state| state.stable_id)
    }

    /// When the server loop must next run the sleeping-pane backlog sweep
    /// (`maybe_sweep_sleeping_backlog`): only while some pane is asleep.
    pub(crate) fn next_wake_sweep_deadline(&self, _now: Instant) -> Option<Instant> {
        const FIRST_SWEEP_AFTER: Duration = Duration::from_secs(3);
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.sleep.is_some())
            .then(|| {
                self.next_backlog_sweep
                    .unwrap_or(self.server_started_at + FIRST_SWEEP_AFTER)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentStartParams, AgentTarget};

    fn app_with_shell_pane() -> (App, crate::layout::PaneId, TerminalId, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            true,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![crate::workspace::Workspace::test_new("wake")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        let pane = app.state.workspaces[0].tabs[0].root_pane.unwrap();
        let terminal = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
        let public = app.public_pane_id(0, pane).unwrap();
        (app, pane, terminal, public)
    }

    fn start(
        app: &mut App,
        public: &str,
        env: Vec<String>,
    ) -> tokio::sync::mpsc::Receiver<bytes::Bytes> {
        let terminal = app.resolve_wake_pane_key(public).unwrap().2;
        let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal, runtime);
        app.start_agent(AgentStartParams {
            name: "owner".into(),
            kind: "pi".into(),
            pane_id: public.to_string(),
            args: vec!["--thinking".into(), "low".into()],
            env,
            timeout_ms: None,
        })
        .ok()
        .expect("managed start");
        input
    }

    fn pi_exits(app: &mut App, pane: crate::layout::PaneId) {
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id: pane,
            agent: None,
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at: Instant::now(),
        });
    }

    fn trigger() -> WakeTrigger {
        WakeTrigger {
            cause: WakeCause::HeadAppended,
            recipient_id: "r".into(),
            head_id: "h".into(),
        }
    }

    #[tokio::test]
    async fn start_records_explicit_recipe_and_a_human_quit_is_never_woken() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let mut input = start(
            &mut app,
            &public,
            vec![
                "PI_CODING_AGENT_DIR=/agent".into(),
                "OPENAI_API_KEY=sk".into(),
            ],
        );
        let recipe = app.state.terminals[&terminal]
            .launch_recipe
            .clone()
            .unwrap();
        assert_eq!(recipe.name, "owner");
        assert_eq!(recipe.args, ["--thinking", "low"]);
        assert_eq!(
            recipe.env,
            [("PI_CODING_AGENT_DIR".to_string(), "/agent".to_string())],
            "only explicit, non-credential --env keys are persisted"
        );
        while input.try_recv().is_ok() {}
        pi_exits(&mut app, pane);
        let outcome = app.wake_pane(&public, trigger());
        assert!(
            matches!(
                outcome,
                WakeOutcome::Refused {
                    reason: WakeRefusal::NotSleeping,
                    ref wake_id
                } if app.pane_wake_record_exists(wake_id)
            ),
            "{outcome:?}"
        );
        assert!(input.try_recv().is_err(), "nothing was launched");
    }

    #[tokio::test]
    async fn lifecycle_owned_panes_neither_sleep_nor_wake() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let _input = start(
            &mut app,
            &public,
            vec![format!(
                "{}=owner-1",
                crate::launch_recipe::LIFECYCLE_ROLE_ENV
            )],
        );
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "sleep".into(),
            method: crate::api::schema::Method::AgentSleep(AgentTarget {
                target: public.clone(),
            }),
        });
        assert!(response.contains("agent_sleep_unavailable"), "{response}");
        assert!(app.state.terminals[&terminal].sleep.is_none());
        // Even with a sleep record, a lifecycle-owned pane is never woken.
        pi_exits(&mut app, pane);
        app.state.terminals.get_mut(&terminal).unwrap().sleep =
            Some(App::new_pane_sleep("owner".into(), 1));
        assert!(matches!(
            app.wake_pane(&public, trigger()),
            WakeOutcome::Refused {
                reason: WakeRefusal::LifecycleOwned,
                ..
            }
        ));
    }

    /// A wake whose start fails after commit keeps the pane's recipe, sleep
    /// and carried route, so a later wake can still carry the route.
    #[tokio::test]
    async fn a_failed_wake_loses_neither_recipe_nor_sleep_nor_route_carry() {
        let (mut app, _pane, terminal, public) = app_with_shell_pane();
        let recipe = crate::launch_recipe::LaunchRecipe::capture(
            "owner",
            "pi",
            &["--session".into(), "/sessions/owner.jsonl".into()],
            &[],
        );
        let carry = crate::launch_recipe::RouteCarry {
            child_delegation: "d2".into(),
            parent_delegation: "d1".into(),
            session_path: "/sessions/owner.jsonl".into(),
            generation: 1,
            parent_terminal: "term_parent".into(),
            parent_session: "/sessions/parent.jsonl".into(),
        };
        {
            let state = app.state.terminals.get_mut(&terminal).unwrap();
            state.launch_recipe = recipe.clone();
            state.sleep = Some(App::new_pane_sleep("owner".into(), 1));
            state.route_carry = Some(carry.clone());
        }
        // The pane's input is closed: the start is committed, then the send fails.
        let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        let outcome = app.wake_pane(&public, trigger());
        assert!(matches!(outcome, WakeOutcome::Failed { .. }), "{outcome:?}");
        let state = &app.state.terminals[&terminal];
        assert_eq!(state.launch_recipe, recipe);
        assert!(state.sleep.is_some());
        assert_eq!(state.route_carry, Some(carry));
        assert!(app.pending_route_carries.is_empty());
        assert!(!app.managed_pi_launches.contains_key(&terminal));
        // A hand start that fails the same way does not clear them either.
        app.pane_wake_cooldowns.clear();
        let (runtime, input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        drop(input);
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        let started = app.start_agent(AgentStartParams {
            name: "other".into(),
            kind: "pi".into(),
            pane_id: public.clone(),
            args: vec!["--thinking".into(), "low".into()],
            env: Vec::new(),
            timeout_ms: None,
        });
        assert!(started.is_err());
        let state = &app.state.terminals[&terminal];
        assert_eq!(state.launch_recipe, recipe);
        assert!(state.sleep.is_some());
        assert!(state.route_carry.is_some());
    }

    #[tokio::test]
    async fn unattached_wake_expires_retries_once_then_cools_down() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let _input = start(&mut app, &public, Vec::new());
        app.state.terminals.get_mut(&terminal).unwrap().sleep =
            Some(App::new_pane_sleep("owner".into(), 1));
        pi_exits(&mut app, pane);
        let WakeOutcome::Started { wake_id: first, .. } = app.wake_pane(&public, trigger()) else {
            panic!("wake starts")
        };
        // Its Pi never attaches: at the deadline the wake fails and one retry
        // runs with a fresh wake ID.
        pi_exits(&mut app, pane);
        let past = Instant::now() + WAKE_ATTACH_TIMEOUT + Duration::from_secs(1);
        assert!(app.handle_pane_wake_deadlines(past));
        let retry = app.pane_wakes.get(&terminal).expect("one retry").clone();
        assert_ne!(retry.wake_id, first);
        assert!(retry.retried);
        // The retry also expires: no third attempt, the pane cools down.
        pi_exits(&mut app, pane);
        assert!(app.handle_pane_wake_deadlines(past + WAKE_ATTACH_TIMEOUT + Duration::from_secs(1)));
        assert!(app.pane_wakes.is_empty());
        assert!(app.pane_wake_cooldowns.contains_key(&terminal));
        assert!(
            app.state.terminals[&terminal].sleep.is_some(),
            "still asleep"
        );
    }

    /// After a restart a slept pane is restored asleep (not resumed); the
    /// mailbox's backlog sweep then wakes it with RestoreBacklog.
    #[tokio::test]
    async fn restore_backlog_sweep_wakes_a_pane_restored_asleep() {
        let (mut app, _pane, terminal, public) = app_with_shell_pane();
        let (runtime, mut input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes.insert(terminal.clone(), runtime);
        {
            // What restore leaves for a slept pane: recipe and sleep, no
            // resume plan and no agent.
            let state = app.state.terminals.get_mut(&terminal).unwrap();
            state.launch_recipe = crate::launch_recipe::LaunchRecipe::capture(
                "owner",
                "pi",
                &["--thinking".into(), "low".into()],
                &[],
            );
            state.sleep = Some(App::new_pane_sleep("owner".into(), 1));
            assert!(state.pending_agent_resume_plan.is_none());
        }
        let sweep = WakeTrigger {
            cause: WakeCause::RestoreBacklog,
            recipient_id: terminal.to_string(),
            head_id: "backlog-1".into(),
        };
        let WakeOutcome::Started {
            wake_id,
            generation,
        } = app.wake_pane(&public, sweep.clone())
        else {
            panic!("the backlog sweep wakes the slept pane")
        };
        assert_eq!(generation, 1);
        assert!(input.try_recv().is_ok(), "one launch");
        let record: serde_json::Value = serde_json::from_slice(
            &std::fs::read(app.pane_wake_dir().join(format!("{wake_id}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(record["trigger"]["cause"], "restore_backlog");
        assert_eq!(record["outcome"], "started");
        // A second sweep before the Pi attaches coalesces.
        assert!(matches!(
            app.wake_pane(&public, sweep),
            WakeOutcome::Duplicate { .. }
        ));
        assert!(input.try_recv().is_err());
    }

    fn request(app: &mut App, method: crate::api::schema::Method) -> serde_json::Value {
        serde_json::from_str(&app.handle_api_request(crate::api::schema::Request {
            id: "t".into(),
            method,
        }))
        .unwrap()
    }

    fn prompt(app: &mut App, target: &str, text: &str) -> serde_json::Value {
        request(
            app,
            crate::api::schema::Method::AgentPrompt(crate::api::schema::AgentPromptParams {
                target: target.into(),
                text: text.into(),
                wait: None,
                send: Default::default(),
            }),
        )
    }

    /// A Pi is detected in the pane; it counts as having attached Messages
    /// before (so its pane keeps a Messages queue while it sleeps).
    fn pi_attaches(app: &mut App, pane: crate::layout::PaneId, generation: u64) {
        if let Some(terminal) = app
            .state
            .workspaces
            .iter()
            .find_map(|workspace| workspace.terminal_id(pane))
            .cloned()
        {
            if let Some(state) = app.state.terminals.get_mut(&terminal) {
                state.messages_capable = true;
            }
        }
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: crate::detect::Agent::Pi,
            process_generation: generation,
            observed_at: Instant::now(),
        });
    }

    /// The woken generation claims and settles its pane inbox's next head
    /// (queue key and legacy terminal key), bound to its execution.
    fn claim(app: &mut App, terminal: &TerminalId, generation: u64) -> Option<String> {
        let store = crate::mailbox::MailboxStore::open(&app.sender_authority_dir).unwrap();
        let claim = store
            .claim_next_for_execution(
                &app.inbox_recipients(&terminal.to_string()),
                &format!("managed:{terminal}:{generation}"),
            )
            .unwrap()?;
        store
            .resolve_claim(
                &claim.claim_id,
                crate::mailbox::ClaimResolutionOutcome::Settled,
            )
            .unwrap();
        Some(claim.stable_id)
    }

    /// The wake hook end to end: sleep, then `agent prompt` to the sleeping
    /// agent queues a Messages head and wakes the pane once; a second prompt
    /// coalesces; the woken generation runs each head exactly once.
    #[tokio::test]
    async fn prompt_to_a_sleeping_agent_queues_wakes_once_and_each_head_runs_once() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let mut input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        let slept = request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        assert!(slept.get("error").is_none(), "{slept}");
        pi_exits(&mut app, pane);
        while input.try_recv().is_ok() {}

        let first = prompt(&mut app, "owner", "first task");
        assert_eq!(first["result"]["delivery"]["path"], "mailbox", "{first}");
        assert_eq!(first["result"]["agent"]["name"], "owner");
        let outstanding = app
            .pane_wakes
            .get(&terminal)
            .expect("the head woke the pane")
            .clone();
        assert_eq!(outstanding.trigger.cause, WakeCause::HeadAppended);
        // Exactly one wake call (one durable record) for the appended head.
        let wake_records = |app: &App| {
            std::fs::read_dir(app.sender_authority_dir.join("pane-wakes"))
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
                        .count()
                })
                .unwrap_or(0)
        };
        assert_eq!(wake_records(&app), 1, "one wake call per append");
        // The wake is keyed by the pane's restart-stable queue key.
        let queue_key = app.state.terminals[&terminal].queue_key.clone();
        assert_eq!(outstanding.pane_key, format!("pane:{queue_key}"));
        assert_eq!(
            outstanding.trigger.recipient_id,
            format!("pane:{queue_key}")
        );
        for key in [
            queue_key.clone(),
            format!("pane:{queue_key}"),
            public.clone(),
        ] {
            assert_eq!(
                app.resolve_wake_pane_key(&key).map(|(_, _, t)| t),
                Some(terminal.clone())
            );
        }
        let second = prompt(&mut app, "owner", "second task");
        assert_eq!(second["result"]["delivery"]["path"], "mailbox", "{second}");
        assert_eq!(wake_records(&app), 2, "the second append coalesces once");
        assert_eq!(
            app.pane_wakes[&terminal].wake_id, outstanding.wake_id,
            "coalesced"
        );
        let launch = input.try_recv().expect("one launch");
        assert!(String::from_utf8_lossy(&launch).contains("pi"));
        assert!(
            input.try_recv().is_err(),
            "exactly one launch, and no prompt typed"
        );

        pi_attaches(&mut app, pane, outstanding.generation);
        assert!(app.state.terminals[&terminal].sleep.is_none());
        let mut ran = Vec::new();
        while let Some(head) = claim(&mut app, &terminal, outstanding.generation) {
            ran.push(head);
            assert!(ran.len() <= 2, "a head ran twice: {ran:?}");
        }
        assert_eq!(ran.len(), 2, "each queued head runs exactly once");
        let delivered = |response: &serde_json::Value| {
            let delivery = &response["result"]["delivery"];
            delivery["stable_id"]
                .as_str()
                .or_else(|| delivery["stableId"].as_str())
                .expect("delivery stable id")
                .to_string()
        };
        let mut expected = vec![delivered(&first), delivered(&second)];
        expected.sort();
        ran.sort();
        assert_eq!(ran, expected);
    }

    /// A handoff to an agent Herdr put to sleep is queued in its Messages and
    /// wakes the pane; it is not refused because the target sleeps.
    #[tokio::test]
    async fn handoff_to_a_sleeping_agent_is_queued_and_wakes_it() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        // Sender: a second workspace with a reported Pi.
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("sender"));
        app.state.ensure_test_terminals();
        let sender_pane = app.state.workspaces[1].tabs[0].root_pane.unwrap();
        let sender_terminal = app.state.workspaces[1]
            .terminal_id(sender_pane)
            .unwrap()
            .clone();
        app.state
            .terminals
            .get_mut(&sender_terminal)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        let (sender_runtime, _sender_input) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes
            .insert(sender_terminal.clone(), sender_runtime);
        let sender_public = app.public_pane_id(1, sender_pane).unwrap();
        request(
            &mut app,
            crate::api::schema::Method::PaneReportAgentSession(
                crate::api::schema::PaneReportAgentSessionParams {
                    pane_id: sender_public,
                    source: "herdr:pi".into(),
                    agent: "pi".into(),
                    seq: Some(1),
                    agent_session_id: None,
                    agent_session_path: Some("/sessions/sender.jsonl".into()),
                    session_start_source: Some("startup".into()),
                },
            ),
        );
        // Recipient: a managed agent whose recipe resumes /sessions/owner.jsonl,
        // put to sleep.
        let mut input = start(&mut app, &public, Vec::new());
        app.state
            .terminals
            .get_mut(&terminal)
            .unwrap()
            .launch_recipe = crate::launch_recipe::LaunchRecipe::capture(
            "owner",
            "pi",
            &["--session".into(), "/sessions/owner.jsonl".into()],
            &[],
        );
        pi_attaches(&mut app, pane, 1);
        let slept = request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        assert!(slept.get("error").is_none(), "{slept}");
        pi_exits(&mut app, pane);
        while input.try_recv().is_ok() {}

        let sender_info = app.agent_info(1, sender_pane).unwrap();
        let recipient_pane = app.pane_info(0, pane).unwrap();
        let identity = |workspace_id: String, pane_id: String, terminal_id: String, value: &str| {
            crate::api::schema::CanonicalHerdrIdentity {
                workspace_id,
                pane_id,
                terminal_id,
                agent_session: crate::api::schema::AgentSessionInfo {
                    source: "herdr:pi".into(),
                    agent: "pi".into(),
                    kind: crate::agent_resume::AgentSessionRefKind::Path,
                    value: value.into(),
                },
            }
        };
        let envelope = crate::api::schema::HerdrHandoff {
            version: 1,
            message_id: "handoff-sleep-1".into(),
            created_at: "unix:1".into(),
            sender: identity(
                sender_info.workspace_id,
                sender_info.pane_id,
                sender_info.terminal_id,
                "/sessions/sender.jsonl",
            ),
            recipient: identity(
                recipient_pane.workspace_id,
                recipient_pane.pane_id,
                recipient_pane.terminal_id,
                "/sessions/owner.jsonl",
            ),
            kind: crate::api::schema::HandoffKind::Assignment,
            correlation_id: None,
            reply_to_id: None,
            task: None,
            summary: "work while I sleep".into(),
            artifact_refs: vec![],
        };
        let send = |app: &mut App, transport| {
            request(
                app,
                crate::api::schema::Method::HandoffSend(crate::api::schema::HandoffSendParams {
                    envelope: envelope.clone(),
                    send: crate::api::schema::MessageSendOptions {
                        transport,
                        ..Default::default()
                    },
                }),
            )
        };
        let pty = send(&mut app, Some(crate::api::schema::MessageTransport::Pty));
        assert_eq!(
            pty["result"]["receipt"]["outcome"], "recipient_not_ready",
            "{pty}"
        );
        assert!(app.pane_wakes.is_empty() && input.try_recv().is_err());
        let queued = send(&mut app, None);
        assert_eq!(
            queued["result"]["receipt"]["outcome"], "mailbox_admitted",
            "{queued}"
        );
        assert_eq!(queued["result"]["receipt"]["delivery"]["path"], "mailbox");
        assert_eq!(
            app.pane_wakes.get(&terminal).map(|wake| wake.trigger.cause),
            Some(WakeCause::HeadAppended)
        );
        assert!(input.try_recv().is_ok(), "one launch");
        assert!(input.try_recv().is_err(), "nothing typed");
        assert!(
            app.first_unsettled_head(&terminal).is_some(),
            "the handoff is queued"
        );
    }

    fn second_pane(app: &mut App) -> (crate::layout::PaneId, TerminalId, String) {
        app.state
            .workspaces
            .push(crate::workspace::Workspace::test_new("second"));
        app.state.ensure_test_terminals();
        let ws_idx = app.state.workspaces.len() - 1;
        let pane = app.state.workspaces[ws_idx].tabs[0].root_pane.unwrap();
        let terminal = app.state.workspaces[ws_idx]
            .terminal_id(pane)
            .unwrap()
            .clone();
        let public = app.public_pane_id(ws_idx, pane).unwrap();
        (pane, terminal, public)
    }

    /// QA: a slept parent would break its children's bound report routes, so a
    /// parent of live delegations neither sleeps nor is re-woken by a retry.
    #[tokio::test]
    async fn a_parent_of_active_delegation_routes_does_not_sleep() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let _input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        let (child_pane, child_terminal, _) = second_pane(&mut app);
        let parent = app
            .state
            .delegations
            .create(Some(pane), None, None)
            .unwrap();
        let _child = app
            .state
            .delegations
            .create(Some(child_pane), Some(parent), None)
            .unwrap();
        app.state
            .terminals
            .get_mut(&child_terminal)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        let refused = request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        assert_eq!(
            refused["error"]["code"], "agent_sleep_unavailable",
            "{refused}"
        );
        assert_eq!(refused["error"]["message"], PARENT_OF_ACTIVE_ROUTES);
        assert!(app.state.terminals[&terminal].sleep.is_none());
        // A retry never wakes such a pane either.
        pi_exits(&mut app, pane);
        app.state.terminals.get_mut(&terminal).unwrap().sleep =
            Some(App::new_pane_sleep("owner".into(), 1));
        assert!(matches!(
            app.wake_pane_attempt(&public, trigger(), true),
            WakeOutcome::Refused {
                reason: WakeRefusal::ParentOfActiveRoutes,
                ..
            }
        ));
        // Once the child is gone, the pane may sleep again.
        app.state
            .terminals
            .get_mut(&child_terminal)
            .unwrap()
            .set_detected_state(None, crate::detect::AgentState::Unknown);
        assert!(!app.terminal_parents_active_routes(&terminal));
    }

    /// QA: a hand-typed pi in a slept pane ends the sleep, so prompts are no
    /// longer queued unread.
    #[tokio::test]
    async fn a_live_agent_appearing_in_a_slept_pane_ends_the_sleep() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let mut input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        pi_exits(&mut app, pane);
        assert!(app.state.terminals[&terminal].sleep.is_some());
        while input.try_recv().is_ok() {}
        // The human types `pi` into the pane: an unmanaged Pi is detected.
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id: pane,
            agent: crate::detect::Agent::Pi,
            process_generation: 0,
            observed_at: Instant::now(),
        });
        assert!(app.state.terminals[&terminal].sleep.is_none());
        assert!(app.pane_wakes.is_empty());
        let queued = prompt(&mut app, "owner", "hello?");
        assert!(
            queued.get("error").is_some(),
            "no longer addressed as a sleeping agent: {queued}"
        );
        assert!(
            app.first_unsettled_head(&terminal).is_none(),
            "nothing queued unread"
        );
    }

    /// QA: a sleeping agent's name stays taken, except by a relaunch in its own pane.
    #[tokio::test]
    async fn a_sleeping_agents_name_stays_taken() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let _input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        pi_exits(&mut app, pane);
        let (_other_pane, other_terminal, other_public) = second_pane(&mut app);
        let (runtime, _other_input) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.terminal_runtimes
            .insert(other_terminal.clone(), runtime);
        let taken = app.start_agent(AgentStartParams {
            name: "owner".into(),
            kind: "pi".into(),
            pane_id: other_public.clone(),
            args: Vec::new(),
            env: Vec::new(),
            timeout_ms: None,
        });
        assert!(matches!(
            taken,
            Err(crate::app::agents::AgentStartError::DuplicateName { .. })
        ));
        // Nor can another pane's agent be renamed to it.
        app.state
            .terminals
            .get_mut(&other_terminal)
            .unwrap()
            .set_detected_state(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
            );
        let renamed = app.rename_agent_target(&other_public, Some("owner".into()));
        assert!(
            matches!(
                renamed,
                Err(crate::app::agents::AgentRenameError::DuplicateName { ref name, ref candidates })
                    if name == "owner" && candidates.iter().any(|agent| agent.terminal_id == terminal.to_string())
            ),
            "rename to a sleeping agent's name must be refused"
        );
        assert!(!matches!(
            app.rename_agent_target(&other_public, Some("free-name".into())),
            Err(crate::app::agents::AgentRenameError::DuplicateName { .. })
        ));
        // The wake itself relaunches the same name in the same pane.
        assert!(matches!(
            app.wake_pane(&public, trigger()),
            WakeOutcome::Started { .. }
        ));
        assert_eq!(
            app.state.terminals[&terminal].agent_name.as_deref(),
            Some("owner")
        );
    }

    #[tokio::test]
    async fn prompt_after_a_manual_quit_neither_queues_nor_wakes() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let mut input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        pi_exits(&mut app, pane);
        while input.try_recv().is_ok() {}
        let response = prompt(&mut app, "owner", "hello?");
        assert!(response.get("error").is_some(), "{response}");
        assert!(app.pane_wakes.is_empty());
        assert!(input.try_recv().is_err(), "nothing launched or typed");
        assert_eq!(app.first_unsettled_head(&terminal), None, "nothing queued");
    }

    #[tokio::test]
    async fn startup_backlog_sweep_wakes_slept_panes_with_unsettled_heads_only() {
        let (mut app, pane, terminal, public) = app_with_shell_pane();
        let mut input = start(&mut app, &public, Vec::new());
        pi_attaches(&mut app, pane, 1);
        request(
            &mut app,
            crate::api::schema::Method::AgentSleep(AgentTarget {
                target: "owner".into(),
            }),
        );
        pi_exits(&mut app, pane);
        // Nothing queued yet: the sweep leaves the pane asleep.
        app.sweep_sleeping_backlog(true);
        assert!(app.pane_wakes.is_empty());
        // A head queued before the "restart", with the hook not yet run.
        app.pane_wake_cooldowns.clear();
        let recipient = crate::app::messages::pane_recipient(
            &app.pane_queue_key(&terminal.to_string()).unwrap(),
        );
        let sender = crate::app::messages::SenderAttribution {
            terminal: None,
            label: "external".into(),
            session: None,
        };
        let routed = app.route_ordinary_send(
            &terminal.to_string(),
            &sender,
            crate::app::messages::OutgoingMessage {
                origin: "agent_prompt",
                subject: "backlog".into(),
                body: "queued before restart".into(),
                priority: "normal".into(),
                kind: "advisory".into(),
                message_id: None,
                correlation: None,
                replace_pending: false,
            },
            &Default::default(),
        );
        assert!(matches!(
            routed,
            Ok(crate::app::messages::SendRoute::Mailbox(_))
        ));
        assert!(recipient.recipient_id.starts_with("pane:"));
        while input.try_recv().is_ok() {}
        app.sweep_sleeping_backlog(true);
        let outstanding = app
            .pane_wakes
            .get(&terminal)
            .expect("backlog woke the pane");
        assert_eq!(outstanding.trigger.cause, WakeCause::RestoreBacklog);
        assert!(input.try_recv().is_ok(), "one launch");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_resume_with_a_recipe_comes_back_managed() {
        let _env = crate::test_env::shared();
        let (mut app, _pane, terminal, _public) = app_with_shell_pane();
        app.terminal_runtimes.remove(&terminal);
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        // An inert "sh" that reads and discards its input: the resume command
        // typed into it can never start a real agent.
        let fake_shell_dir = std::env::temp_dir().join(format!(
            "herdr-inert-shell-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&fake_shell_dir).unwrap();
        let fake_shell = fake_shell_dir.join("sh");
        crate::test_env::write_executable(
            &fake_shell,
            "#!/bin/sh\nwhile read -r line; do :; done\n",
        );
        app.state.default_shell = fake_shell.display().to_string();
        {
            let state = app.state.terminals.get_mut(&terminal).unwrap();
            // The restored snapshot: a recipe, a resume plan and a stale agent
            // name. PATH makes the launched command inert in the test shell.
            state.launch_recipe = crate::launch_recipe::LaunchRecipe::capture(
                "owner",
                "pi",
                &["--thinking".into(), "low".into()],
                &[],
            );
            state.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "pi".into(),
                argv: vec!["pi".into(), "--session".into(), "/tmp/s.jsonl".into()],
                dedupe_key: "pi:/tmp/s.jsonl".into(),
            });
            state.restore_managed_agent("owner".into(), crate::detect::Agent::Pi);
            // As restore does: the old agent shows as a detected, idle Pi.
            let _ = state.set_detected_state_with_screen_signals_at(
                Some(crate::detect::Agent::Pi),
                crate::detect::AgentState::Idle,
                false,
                false,
                false,
                false,
                Instant::now(),
            );
        }
        assert!(app.start_pending_agent_resume_for_terminal(&terminal, 24, 80, true));
        // The new shell reaches its prompt on a later scheduled tick.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !app.pending_managed_resumes.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            app.retry_pending_managed_resumes(Instant::now());
        }
        let state = &app.state.terminals[&terminal];
        assert_eq!(state.agent_name.as_deref(), Some("owner"));
        assert_eq!(state.managed_agent_kind(), Some(crate::detect::Agent::Pi));
        assert_eq!(
            state.managed_agent_generation(),
            Some(1),
            "resumed through the managed path, with a sender generation"
        );
        assert!(state.pending_agent_resume_plan.is_none());
        if let Some(runtime) = app.terminal_runtimes.remove(&terminal) {
            runtime.shutdown();
        }
        let _ = std::fs::remove_dir_all(fake_shell_dir);
    }
}
