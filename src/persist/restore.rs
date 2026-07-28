use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};
use tracing::{error, warn};

use crate::detect::AgentState;
use crate::events::AppEvent;
#[cfg(test)]
use crate::layout::Node;
use crate::layout::{CollectionId, LayoutLeaf, PaneCollection, PaneId, TileLayout, TypedNode};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalState};
use crate::workspace::Workspace;

use super::snapshot::{
    CollectionSnapshot, LayoutLeafSnapshot, PaneAgentSessionSnapshot, PaneHistorySnapshot,
    TabHistorySnapshot, WorkspaceHistorySnapshot,
};
use super::{
    DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot, SessionSnapshot, TabSnapshot,
    WorkspaceSnapshot,
};

struct AgentRestoreState<'a> {
    enabled: bool,
    resumed_sessions: &'a mut HashSet<String>,
}

struct PaneRestoreStartup<'a> {
    restore_plan: Option<crate::agent_resume::AgentResumePlan>,
    initial_history_ansi: Option<&'a str>,
    duplicate_agent_session: bool,
    reserved_agent_session: Option<String>,
}

struct RestoreRuntimeContext<'a> {
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'a>,
    resume_agents_on_restore: bool,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
}

type RestoredSession = (
    Vec<Workspace>,
    crate::delegation::Delegations,
    HashMap<PaneId, std::time::SystemTime>,
    HashMap<TerminalId, TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
);
type RestoredWorkspace = (
    Workspace,
    Vec<TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
    HashMap<u32, PaneId>,
);
type RestoredTab = (
    crate::workspace::Tab,
    Vec<TerminalState>,
    HashMap<TerminalId, TerminalRuntime>,
    HashMap<PaneId, u32>,
);
type RestoreFailures<T> = (T, usize);

/// Restore workspaces from a snapshot. Each pane gets a fresh shell in its saved cwd.
pub fn restore(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    shell_mode: crate::config::ShellModeConfig,
    resume_agents_on_restore: bool,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
) -> RestoredSession {
    let mut imported_panes = HashMap::new();
    restore_with_imports(
        snapshot,
        history,
        rows,
        cols,
        scrollback_limit_bytes,
        crate::pane::PaneShellConfig::new(default_shell, shell_mode),
        resume_agents_on_restore,
        &mut imported_panes,
        events,
        render_notify,
        render_dirty,
    )
}

#[cfg(unix)]
pub fn restore_handoff(
    snapshot: &SessionSnapshot,
    scrollback_limit_bytes: usize,
    default_shell: &str,
    shell_mode: crate::config::ShellModeConfig,
    imports: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
) -> std::io::Result<RestoredSession> {
    restore_with_imports_strict(
        snapshot,
        None,
        24,
        80,
        scrollback_limit_bytes,
        crate::pane::PaneShellConfig::new(default_shell, shell_mode),
        true,
        imports,
        events,
        render_notify,
        render_dirty,
    )
}

#[cfg(unix)]
pub fn handoff_pane_aliases(
    snapshot: &SessionSnapshot,
    workspaces: &[Workspace],
) -> HashMap<u32, PaneId> {
    let mut aliases = HashMap::new();
    for (ws_snap, workspace) in snapshot.workspaces.iter().zip(workspaces) {
        for (old_id, public_number) in &ws_snap.public_pane_numbers {
            if let Some(new_id) = workspace
                .public_pane_numbers
                .iter()
                .find_map(|(pane, number)| (number == public_number).then_some(*pane))
            {
                if *old_id != new_id.raw() {
                    aliases.insert(*old_id, new_id);
                }
            }
        }
        if ws_snap.public_pane_numbers.is_empty() {
            for (tab_snap, tab) in ws_snap.tabs.iter().zip(&workspace.tabs) {
                let mut old_ids: Vec<_> = tab_snap.panes.keys().copied().collect();
                old_ids.sort_unstable();
                let mut new_ids = tab.layout.pane_ids();
                new_ids.sort_by_key(|id| id.raw());
                for (old_id, new_id) in old_ids.into_iter().zip(new_ids) {
                    if old_id != new_id.raw() {
                        aliases.insert(old_id, new_id);
                    }
                }
            }
        }
    }
    aliases
}

fn migrated_public_pane_numbers_by_old_raw(
    snap: &WorkspaceSnapshot,
    next_public_pane_number: &mut usize,
) -> HashMap<u32, usize> {
    let mut public_numbers = snap.public_pane_numbers.clone();
    for tab in &snap.tabs {
        let mut pane_ids = Vec::new();
        collect_layout_snapshot_pane_ids(&tab.layout, &mut pane_ids);
        for old_raw in pane_ids {
            public_numbers.entry(old_raw).or_insert_with(|| {
                let number = *next_public_pane_number;
                *next_public_pane_number += 1;
                number
            });
        }
    }
    public_numbers
}

fn collect_layout_snapshot_pane_ids(node: &LayoutSnapshot, ids: &mut Vec<u32>) {
    match node {
        LayoutSnapshot::Pane(id) => ids.push(*id),
        LayoutSnapshot::Collection(_) => {}
        LayoutSnapshot::Split { first, second, .. } => {
            collect_layout_snapshot_pane_ids(first, ids);
            collect_layout_snapshot_pane_ids(second, ids);
        }
    }
}

#[cfg(unix)]
fn restore_with_imports_strict(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    resume_agents_on_restore: bool,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
) -> std::io::Result<RestoredSession> {
    let (restored, failed_imports) = restore_with_imports_and_failures(
        snapshot,
        history,
        rows,
        cols,
        scrollback_limit_bytes,
        shell_config,
        resume_agents_on_restore,
        imported_panes,
        events,
        render_notify,
        render_dirty,
    );
    if failed_imports > 0 {
        return Err(std::io::Error::other(format!(
            "handoff failed to restore {failed_imports} imported pane runtime(s)"
        )));
    }
    if !imported_panes.is_empty() {
        return Err(std::io::Error::other(format!(
            "handoff import did not consume {} pane runtime(s)",
            imported_panes.len()
        )));
    }
    Ok(restored)
}

fn restore_with_imports(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    resume_agents_on_restore: bool,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
) -> RestoredSession {
    restore_with_imports_and_failures(
        snapshot,
        history,
        rows,
        cols,
        scrollback_limit_bytes,
        shell_config,
        resume_agents_on_restore,
        imported_panes,
        events,
        render_notify,
        render_dirty,
    )
    .0
}

fn restore_with_imports_and_failures(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
    shell_config: crate::pane::PaneShellConfig<'_>,
    resume_agents_on_restore: bool,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    events: mpsc::Sender<AppEvent>,
    render_notify: Arc<Notify>,
    render_dirty: Arc<AtomicBool>,
) -> RestoreFailures<RestoredSession> {
    let mut workspaces = Vec::new();
    let mut terminals = HashMap::new();
    let mut terminal_runtimes = HashMap::new();
    let mut resumed_agent_sessions = HashSet::new();
    let mut claimed_collection_ids = HashSet::new();
    let mut pane_id_map = HashMap::new();
    let mut failed_imports = 0;
    for (idx, ws_snap) in snapshot.workspaces.iter().enumerate() {
        let is_repository_checkout = ws_snap.id.as_ref().is_some_and(|workspace_id| {
            snapshot
                .repositories
                .iter()
                .any(|repository| repository.checkout_workspace_ids.contains(workspace_id))
        });
        if is_repository_checkout && !ws_snap.identity_cwd.exists() {
            warn!(cwd = %ws_snap.identity_cwd.display(), "repository checkout is unavailable during restore; dropping checkout");
            continue;
        }
        let runtime_context = RestoreRuntimeContext {
            scrollback_limit_bytes,
            shell_config,
            resume_agents_on_restore,
            events: events.clone(),
            render_notify: render_notify.clone(),
            render_dirty: render_dirty.clone(),
        };
        let (restored, workspace_failed_imports) = restore_workspace(
            ws_snap,
            history.and_then(|history| history.workspaces.get(idx)),
            rows,
            cols,
            &runtime_context,
            &mut resumed_agent_sessions,
            &mut claimed_collection_ids,
            imported_panes,
        );
        failed_imports += workspace_failed_imports;
        if let Some((workspace, restored_terminals, restored_runtimes, workspace_id_map)) = restored
        {
            for terminal in restored_terminals {
                terminals.insert(terminal.id.clone(), terminal);
            }
            terminal_runtimes.extend(restored_runtimes);
            pane_id_map.extend(workspace_id_map);
            workspaces.push(workspace);
        }
    }
    crate::workspace::reserve_workspace_ids(&workspaces);
    let delegation_records = snapshot.delegations.iter().cloned().map(|mut record| {
        if let Some(old_pane) = record.pane_id {
            record.pane_id = pane_id_map.get(&old_pane.raw()).copied();
            if record.pane_id.is_none() {
                record.tombstone = true;
            }
        }
        record
    });
    let delegations = crate::delegation::Delegations::repair_records(delegation_records);
    let mut collection_archive_times = HashMap::new();
    for record in &snapshot.collection_archive_times {
        if record.subsec_nanos >= 1_000_000_000 {
            continue;
        }
        let Some(pane_id) = pane_id_map.get(&record.pane_id).copied() else {
            continue;
        };
        let archived = workspaces.iter().any(|workspace| {
            workspace.tabs.iter().any(|tab| {
                tab.pane_placement(pane_id)
                    .and_then(|placement| match placement {
                        crate::layout::PanePlacement::Collection(collection_id) => {
                            tab.collection(collection_id)
                        }
                        crate::layout::PanePlacement::Tiled => None,
                    })
                    .is_some_and(|collection| collection.is_archived(pane_id))
            })
        });
        if !archived {
            continue;
        }
        let duration = std::time::Duration::new(record.unix_seconds, record.subsec_nanos);
        if let Some(archived_at) = std::time::SystemTime::UNIX_EPOCH.checked_add(duration) {
            collection_archive_times
                .entry(pane_id)
                .or_insert(archived_at);
        }
    }
    (
        (
            workspaces,
            delegations,
            collection_archive_times,
            terminals,
            terminal_runtimes,
        ),
        failed_imports,
    )
}

fn restore_workspace(
    snap: &WorkspaceSnapshot,
    history: Option<&WorkspaceHistorySnapshot>,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    resumed_agent_sessions: &mut HashSet<String>,
    claimed_collection_ids: &mut HashSet<CollectionId>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
) -> RestoreFailures<Option<RestoredWorkspace>> {
    let mut tabs = Vec::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    let mut workspace_id_map = HashMap::new();
    let workspace_id = snap
        .id
        .clone()
        .unwrap_or_else(crate::workspace::generate_workspace_id);
    let mut next_public_pane_number = snap
        .public_pane_numbers
        .values()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_pane_number);
    let public_pane_numbers_by_old_raw =
        migrated_public_pane_numbers_by_old_raw(snap, &mut next_public_pane_number);
    let public_pane_ids_by_old_raw: HashMap<u32, String> = public_pane_numbers_by_old_raw
        .iter()
        .map(|(old_raw, public_number)| {
            (
                *old_raw,
                format!(
                    "{}:p{}",
                    workspace_id,
                    crate::workspace::encode_public_number(*public_number)
                ),
            )
        })
        .collect();
    let mut public_pane_numbers = HashMap::new();
    let mut next_public_tab_number = snap
        .public_tab_numbers
        .iter()
        .copied()
        .max()
        .and_then(|max| max.checked_add(1))
        .unwrap_or(1)
        .max(snap.next_public_tab_number);
    let mut failed_imports = 0;

    for (idx, tab_snap) in snap.tabs.iter().enumerate() {
        let tab_number = snap.public_tab_numbers.get(idx).copied().unwrap_or(idx + 1);
        let (restored_tab, tab_failed_imports) = restore_tab(
            tab_snap,
            history.and_then(|history| history.tabs.get(idx)),
            tab_number,
            &workspace_id,
            rows,
            cols,
            runtime_context,
            resumed_agent_sessions,
            claimed_collection_ids,
            imported_panes,
            &public_pane_ids_by_old_raw,
        );
        failed_imports += tab_failed_imports;
        let Some((mut tab, restored_terminals, restored_runtimes, reverse_id_map)) = restored_tab
        else {
            continue;
        };
        if let Some(public_tab_number) = snap.public_tab_numbers.get(idx).copied() {
            tab.number = public_tab_number;
        }
        next_public_tab_number = next_public_tab_number.max(tab.number + 1);
        for (new_id, old_id) in &reverse_id_map {
            workspace_id_map.insert(*old_id, *new_id);
        }
        for pane_id in tab.layout.pane_ids() {
            let public_number = public_pane_numbers_by_old_raw
                .get(
                    &reverse_id_map
                        .get(&pane_id)
                        .copied()
                        .unwrap_or(pane_id.raw()),
                )
                .copied()
                .unwrap_or_else(|| {
                    let number = next_public_pane_number;
                    next_public_pane_number += 1;
                    number
                });
            public_pane_numbers.insert(pane_id, public_number);
            next_public_pane_number = next_public_pane_number.max(public_number + 1);
        }
        terminals.extend(restored_terminals);
        terminal_runtimes.extend(restored_runtimes);
        tabs.push(tab);
    }

    if tabs.is_empty() {
        return (None, failed_imports);
    }

    let worktree_space = restored_worktree_space_membership(snap.worktree_space.clone());
    let (cached_git_space, cached_auto_label, cached_git_status_key) =
        crate::workspace::discover_workspace_git_identity(&snap.identity_cwd);

    (
        Some(Workspace {
            id: workspace_id,
            custom_name: snap.custom_name.clone(),
            identity_cwd: snap.identity_cwd.clone(),
            cached_identity_cwd: snap.identity_cwd.clone(),
            cached_auto_label,
            cached_git_status_key,
            cached_git_branch: crate::workspace::git_branch(&snap.identity_cwd),
            cached_git_primary_upstream: None,
            cached_git_ahead_behind: None,
            cached_git_space,
            checkout: snap.checkout.clone(),
            worktree_space,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            resources: crate::workspace_resources::WorkspaceResourceRegistry::default(),
            public_pane_numbers,
            next_public_pane_number,
            next_public_tab_number,
            active_tab: snap.active_tab.min(tabs.len().saturating_sub(1)),
            tabs,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        })
        .map(|workspace| (workspace, terminals, terminal_runtimes, workspace_id_map)),
        failed_imports,
    )
}

fn restored_worktree_space_membership(
    space: Option<crate::workspace::WorktreeSpaceMembership>,
) -> Option<crate::workspace::WorktreeSpaceMembership> {
    space.filter(|space| {
        space.checkout_path.exists()
            && crate::workspace::git_space_metadata(&space.checkout_path)
                .is_some_and(|current| current.key == space.key)
    })
}

fn restore_tab(
    snap: &TabSnapshot,
    history: Option<&TabHistorySnapshot>,
    number: usize,
    workspace_id: &str,
    rows: u16,
    cols: u16,
    runtime_context: &RestoreRuntimeContext<'_>,
    resumed_agent_sessions: &mut HashSet<String>,
    claimed_collection_ids: &mut HashSet<CollectionId>,
    imported_panes: &mut HashMap<u32, crate::handoff_runtime::ImportedHandoffRuntime>,
    public_pane_ids_by_old_raw: &HashMap<u32, String>,
) -> RestoreFailures<Option<RestoredTab>> {
    let (typed_node, id_map) = restore_typed_node_remapped(snap);
    let reverse_id_map: HashMap<PaneId, u32> = id_map
        .iter()
        .map(|(&old_id, &new_id)| (new_id, old_id))
        .collect();
    let mut pane_ids: Vec<_> = id_map.iter().map(|(old, new)| (*old, *new)).collect();
    pane_ids.sort_by_key(|(old, _)| *old);
    let pane_ids: Vec<_> = pane_ids.into_iter().map(|(_, new)| new).collect();

    let mut panes = HashMap::new();
    let mut terminals = Vec::new();
    let mut terminal_runtimes = HashMap::new();
    let mut failed_imports = 0;
    for id in &pane_ids {
        let old_id = reverse_id_map.get(id);
        let saved_pane = old_id.and_then(|old_id| snap.panes.get(old_id));
        let saved_cwd = saved_pane
            .map(|p| p.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));

        let cwd = if saved_cwd.exists() {
            saved_cwd
        } else {
            warn!(
                cwd = %saved_cwd.display(),
                "saved pane cwd does not exist, falling back to HOME"
            );
            let home = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/"));
            if home.exists() {
                home
            } else {
                PathBuf::from("/")
            }
        };

        let saved_label = saved_pane.and_then(|p| p.label.clone());
        let saved_agent_name = saved_pane.and_then(|p| p.agent_name.clone());
        let saved_managed_agent = saved_pane
            .and_then(|pane| pane.managed_agent_kind.as_deref())
            .and_then(crate::detect::parse_canonical_agent_label);
        let saved_launch_argv = saved_pane.and_then(|p| p.launch_argv.clone());
        let saved_agent_session = saved_pane.and_then(|p| p.agent_session.as_ref());
        let saved_history =
            old_id.and_then(|old_id| history.and_then(|history| history.panes.get(old_id)));
        let startup = {
            let mut agent_restore = AgentRestoreState {
                enabled: runtime_context.resume_agents_on_restore,
                resumed_sessions: resumed_agent_sessions,
            };
            pane_restore_startup(saved_agent_session, saved_history, &mut agent_restore)
        };
        let restored_agent_session =
            restored_terminal_agent_session(saved_agent_session, startup.duplicate_agent_session);
        let initial_restore_agent = startup
            .restore_plan
            .as_ref()
            .and_then(|plan| crate::detect::parse_agent_label(&plan.agent));

        let old_pane_id = reverse_id_map.get(id).copied();
        let public_pane_id = old_pane_id
            .and_then(|old_id| public_pane_ids_by_old_raw.get(&old_id))
            .map(String::as_str);
        let launch_env = public_pane_id
            .map(|pane_id| {
                PaneLaunchEnv::from_extra(Vec::new()).with_identity(
                    workspace_id.to_string(),
                    crate::workspace::public_tab_id_for_number(workspace_id, number),
                    pane_id.to_string(),
                )
            })
            .unwrap_or_default();
        let imported_runtime = old_pane_id.and_then(|old_id| imported_panes.remove(&old_id));
        let was_imported = imported_runtime.is_some();
        let pending_native_agent_restore = if was_imported {
            None
        } else {
            startup.restore_plan.clone()
        };
        if let Some(plan) = pending_native_agent_restore {
            let terminal_id = TerminalId::alloc();
            let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone())
                .with_pending_agent_resume_plan(plan);
            if let Some(label) = saved_label {
                terminal.set_manual_label(label);
            }
            if let Some(session) = restored_agent_session {
                terminal.set_persisted_agent_session(session);
            }
            match (saved_agent_name, saved_managed_agent) {
                (Some(agent_name), Some(agent)) => {
                    terminal.restore_managed_agent(agent_name, agent)
                }
                (Some(_), None) => {}
                (None, _) => {}
            }
            if let Some(agent) = initial_restore_agent {
                let _ = terminal.set_detected_state_with_screen_signals_at(
                    Some(agent),
                    AgentState::Idle,
                    false,
                    false,
                    false,
                    false,
                    std::time::Instant::now(),
                );
            }
            panes.insert(*id, PaneState::new(terminal_id));
            terminals.push(terminal);
            continue;
        }

        #[cfg(not(unix))]
        if imported_runtime.is_some() {
            failed_imports += 1;
            continue;
        }

        let runtime_result = {
            #[cfg(unix)]
            if let Some(imported) = imported_runtime {
                TerminalRuntime::from_handoff_fd(
                    crate::handoff_runtime::ImportedHandoffRuntime {
                        master_fd: imported.master_fd,
                        state: imported.state.with_pane_id(*id),
                    },
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            } else {
                TerminalRuntime::spawn_with_initial_history(
                    *id,
                    rows,
                    cols,
                    cwd.clone(),
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    runtime_context.shell_config,
                    &launch_env,
                    startup.initial_history_ansi,
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            }

            #[cfg(not(unix))]
            {
                TerminalRuntime::spawn_with_initial_history(
                    *id,
                    rows,
                    cols,
                    cwd.clone(),
                    runtime_context.scrollback_limit_bytes,
                    crate::terminal_theme::TerminalTheme::default(),
                    runtime_context.shell_config,
                    &launch_env,
                    startup.initial_history_ansi,
                    runtime_context.events.clone(),
                    runtime_context.render_notify.clone(),
                    runtime_context.render_dirty.clone(),
                )
            }
        };

        match runtime_result {
            Ok(runtime) => {
                let terminal_id = TerminalId::alloc();
                let mut terminal = TerminalState::new(terminal_id.clone(), cwd.clone());
                if was_imported {
                    if let Some(argv) = saved_launch_argv {
                        terminal = terminal.with_launch_argv(argv).with_respawn_shell_on_exit();
                    }
                }
                if let Some(label) = saved_label {
                    terminal.set_manual_label(label);
                }
                if let Some(session) = restored_agent_session {
                    terminal.set_persisted_agent_session(session);
                }
                match (saved_agent_name, saved_managed_agent) {
                    (Some(agent_name), Some(agent)) if was_imported => {
                        terminal.restore_managed_agent(agent_name, agent)
                    }
                    (Some(_), Some(_)) => {}
                    (Some(agent_name), None) if was_imported => terminal.set_agent_name(agent_name),
                    (Some(_), None) => {}
                    (None, _) => {}
                }
                if let Some(agent) = initial_restore_agent {
                    let _ = terminal.set_detected_state_with_screen_signals_at(
                        Some(agent),
                        AgentState::Idle,
                        false,
                        false,
                        false,
                        false,
                        std::time::Instant::now(),
                    );
                }
                panes.insert(*id, PaneState::new(terminal_id.clone()));
                terminal_runtimes.insert(terminal_id, runtime);
                terminals.push(terminal);
            }
            Err(e) => {
                if let Some(key) = startup.reserved_agent_session.as_deref() {
                    resumed_agent_sessions.remove(key);
                }
                if was_imported {
                    failed_imports += 1;
                    error!(
                        tab = ?snap.custom_name,
                        pane_id = id.raw(),
                        err = %e,
                        "failed to restore imported pane"
                    );
                }
                error!(
                    tab = ?snap.custom_name,
                    pane_id = id.raw(),
                    err = %e,
                    "failed to restore pane, skipping"
                );
            }
        }
    }

    let surviving: HashSet<PaneId> = panes.keys().copied().collect();
    let Some(layout) = recover_typed_layout_with_claims(
        snap,
        typed_node,
        &id_map,
        &surviving,
        claimed_collection_ids,
    ) else {
        warn!(
            tab = ?snap.custom_name,
            "restored tab lost all panes and collections while recovering typed layout"
        );
        return (None, failed_imports);
    };
    let pane_ids = layout.pane_ids();
    let root_pane = resolve_restored_pane(snap.root_pane, &id_map, &surviving, &pane_ids);
    if !panes.is_empty() && root_pane.is_none() {
        return (None, failed_imports);
    }

    (
        Some((
            crate::workspace::Tab {
                custom_name: snap.custom_name.clone(),
                number,
                root_pane,
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: snap.zoomed,
                events: runtime_context.events.clone(),
                render_notify: runtime_context.render_notify.clone(),
                render_dirty: runtime_context.render_dirty.clone(),
            },
            terminals,
            terminal_runtimes,
            reverse_id_map,
        )),
        failed_imports,
    )
}

fn pane_restore_startup<'a>(
    session: Option<&PaneAgentSessionSnapshot>,
    history: Option<&'a PaneHistorySnapshot>,
    agent_restore: &mut AgentRestoreState<'_>,
) -> PaneRestoreStartup<'a> {
    // Native agent resume owns the conversation history. If a pane has a
    // resumable agent session and resume is enabled, do not replay saved pane
    // presentation history into that terminal, even when this pane is a
    // duplicate suppressed by session de-duplication.
    let restore_plan =
        session.and_then(|session| restore_plan_for_snapshot(session, agent_restore.enabled));
    let has_native_agent_restore = restore_plan.is_some();
    // Reserve before spawning so later panes in the same restore pass cannot
    // launch the same native agent session. The caller rolls this reservation
    // back if runtime spawn fails before any agent process is started.
    let mut reserved_agent_session = None;
    let duplicate_agent_session = restore_plan.as_ref().is_some_and(|plan| {
        if agent_restore
            .resumed_sessions
            .insert(plan.dedupe_key.clone())
        {
            reserved_agent_session = Some(plan.dedupe_key.clone());
            false
        } else {
            true
        }
    });
    let restore_plan = if duplicate_agent_session {
        None
    } else {
        restore_plan
    };

    PaneRestoreStartup {
        restore_plan,
        initial_history_ansi: if has_native_agent_restore {
            None
        } else {
            history.map(|history| history.ansi.as_str())
        },
        duplicate_agent_session,
        reserved_agent_session,
    }
}

fn restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
) -> Option<crate::agent_resume::AgentResumePlan> {
    if !resume_agents_on_restore {
        return None;
    }
    let persisted = persisted_agent_session_from_snapshot(session)?;
    crate::agent_resume::plan(&session.source, &session.agent, &persisted.session_ref)
}

fn persisted_agent_session_from_snapshot(
    session: &PaneAgentSessionSnapshot,
) -> Option<crate::agent_resume::PersistedAgentSession> {
    crate::agent_resume::session_ref_from_snapshot(
        &session.source,
        &session.agent,
        session.kind,
        &session.value,
    )
}

fn restored_terminal_agent_session(
    session: Option<&PaneAgentSessionSnapshot>,
    duplicate_agent_session: bool,
) -> Option<crate::agent_resume::PersistedAgentSession> {
    if duplicate_agent_session {
        return None;
    }
    session.and_then(persisted_agent_session_from_snapshot)
}

#[cfg(test)]
fn take_restore_plan_for_snapshot(
    session: &PaneAgentSessionSnapshot,
    resume_agents_on_restore: bool,
    resumed_agent_sessions: &mut HashSet<String>,
) -> Option<crate::agent_resume::AgentResumePlan> {
    restore_plan_for_snapshot(session, resume_agents_on_restore)
        .filter(|plan| resumed_agent_sessions.insert(plan.dedupe_key.clone()))
}

fn restore_typed_node_remapped(snap: &TabSnapshot) -> (TypedNode, HashMap<u32, PaneId>) {
    let mut old_ids: HashSet<u32> = snap.panes.keys().copied().collect();
    fn collect(node: &LayoutSnapshot, ids: &mut HashSet<u32>) {
        match node {
            LayoutSnapshot::Pane(id) => {
                ids.insert(*id);
            }
            LayoutSnapshot::Collection(_) => {}
            LayoutSnapshot::Split { first, second, .. } => {
                collect(first, ids);
                collect(second, ids);
            }
        }
    }
    collect(&snap.layout, &mut old_ids);
    let mut ordered: Vec<_> = old_ids.into_iter().collect();
    ordered.sort_unstable();
    let id_map: HashMap<_, _> = ordered
        .into_iter()
        .map(|old| (old, PaneId::alloc()))
        .collect();
    fn remap(node: &LayoutSnapshot, ids: &HashMap<u32, PaneId>) -> TypedNode {
        match node {
            LayoutSnapshot::Pane(old) => TypedNode::Leaf(LayoutLeaf::Pane(
                ids.get(old).copied().unwrap_or_else(PaneId::alloc),
            )),
            LayoutSnapshot::Collection(id) => TypedNode::Leaf(LayoutLeaf::Collection(*id)),
            LayoutSnapshot::Split {
                direction,
                ratio,
                first,
                second,
            } => TypedNode::Split {
                direction: match direction {
                    DirectionSnapshot::Horizontal => Direction::Horizontal,
                    DirectionSnapshot::Vertical => Direction::Vertical,
                },
                ratio: *ratio,
                first: Box::new(remap(first, ids)),
                second: Box::new(remap(second, ids)),
            },
        }
    }
    (remap(&snap.layout, &id_map), id_map)
}

#[cfg(test)]
fn recover_typed_layout(
    snap: &TabSnapshot,
    node: TypedNode,
    id_map: &HashMap<u32, PaneId>,
    surviving: &HashSet<PaneId>,
) -> Option<TileLayout> {
    recover_typed_layout_with_claims(snap, node, id_map, surviving, &mut HashSet::new())
}

fn recover_typed_layout_with_claims(
    snap: &TabSnapshot,
    node: TypedNode,
    id_map: &HashMap<u32, PaneId>,
    surviving: &HashSet<PaneId>,
    claimed_collection_ids: &mut HashSet<CollectionId>,
) -> Option<TileLayout> {
    let mut records: HashMap<CollectionId, &CollectionSnapshot> = HashMap::new();
    for record in &snap.collections {
        records.entry(record.id).or_insert(record);
    }
    let mut collection_leaves = HashSet::new();
    fn prune(
        node: TypedNode,
        surviving: &HashSet<PaneId>,
        records: &HashMap<CollectionId, &CollectionSnapshot>,
        collection_leaves: &mut HashSet<CollectionId>,
        claimed_collection_ids: &mut HashSet<CollectionId>,
    ) -> Option<TypedNode> {
        match node {
            TypedNode::Leaf(LayoutLeaf::Pane(id)) => surviving
                .contains(&id)
                .then_some(TypedNode::Leaf(LayoutLeaf::Pane(id))),
            TypedNode::Leaf(LayoutLeaf::Collection(id)) => (records.contains_key(&id)
                && !collection_leaves.contains(&id)
                && claimed_collection_ids.insert(id)
                && collection_leaves.insert(id))
            .then_some(TypedNode::Leaf(LayoutLeaf::Collection(id))),
            TypedNode::Split {
                direction,
                ratio,
                first,
                second,
            } => match (
                prune(
                    *first,
                    surviving,
                    records,
                    collection_leaves,
                    claimed_collection_ids,
                ),
                prune(
                    *second,
                    surviving,
                    records,
                    collection_leaves,
                    claimed_collection_ids,
                ),
            ) {
                (Some(first), Some(second)) => Some(TypedNode::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(node), None) | (None, Some(node)) => Some(node),
                (None, None) => None,
            },
        }
    }

    let mut root = prune(
        node,
        surviving,
        &records,
        &mut collection_leaves,
        claimed_collection_ids,
    );
    let mut placed = HashSet::new();
    fn collect_tiled(node: &TypedNode, placed: &mut HashSet<PaneId>) {
        match node {
            TypedNode::Leaf(LayoutLeaf::Pane(id)) => {
                placed.insert(*id);
            }
            TypedNode::Leaf(LayoutLeaf::Collection(_)) => {}
            TypedNode::Split { first, second, .. } => {
                collect_tiled(first, placed);
                collect_tiled(second, placed);
            }
        }
    }
    if let Some(node) = root.as_ref() {
        collect_tiled(node, &mut placed);
    }

    let mut collections = Vec::new();
    let mut collection_ids: Vec<_> = collection_leaves.into_iter().collect();
    collection_ids.sort_by_key(|id| id.raw());
    for id in collection_ids {
        let Some(saved) = records.get(&id) else {
            continue;
        };
        let mut members = Vec::new();
        for old in &saved.members {
            let Some(member) = id_map.get(old).copied() else {
                continue;
            };
            if surviving.contains(&member) && placed.insert(member) {
                members.push(member);
            }
        }
        let selected = saved
            .selected
            .and_then(|old| id_map.get(&old).copied())
            .filter(|pane| members.contains(pane))
            .or_else(|| members.first().copied());
        let archived: HashSet<_> = saved
            .archived
            .iter()
            .filter_map(|old| id_map.get(old).copied())
            .filter(|pane| members.contains(pane))
            .collect();
        if let Some(collection) =
            PaneCollection::from_saved(id, saved.label.clone(), members, selected, archived)
        {
            collections.push(collection);
        }
    }

    let mut unplaced: Vec<_> = surviving
        .iter()
        .copied()
        .filter(|pane| !placed.contains(pane))
        .collect();
    unplaced.sort_by_key(|pane| {
        id_map
            .iter()
            .find_map(|(old, new)| (*new == *pane).then_some(*old))
            .unwrap_or(u32::MAX)
    });
    for pane in unplaced {
        let leaf = TypedNode::Leaf(LayoutLeaf::Pane(pane));
        root = Some(match root {
            Some(existing) => TypedNode::Split {
                direction: Direction::Horizontal,
                ratio: 0.5,
                first: Box::new(existing),
                second: Box::new(leaf),
            },
            None => leaf,
        });
        placed.insert(pane);
    }
    let root = root?;

    let saved_focus = snap.focused_leaf.and_then(|leaf| match leaf {
        LayoutLeafSnapshot::Pane(old) => id_map.get(&old).copied().map(LayoutLeaf::Pane),
        LayoutLeafSnapshot::Collection(id) => Some(LayoutLeaf::Collection(id)),
    });
    let legacy_focus = snap
        .focused
        .and_then(|old| id_map.get(&old).copied())
        .map(|pane| {
            collections
                .iter()
                .find(|collection| collection.members().contains(&pane))
                .map(|collection| LayoutLeaf::Collection(collection.id))
                .unwrap_or(LayoutLeaf::Pane(pane))
        });
    let mut restored_leaves = Vec::new();
    fn collect_leaves(node: &TypedNode, output: &mut Vec<LayoutLeaf>) {
        match node {
            TypedNode::Leaf(leaf) => output.push(*leaf),
            TypedNode::Split { first, second, .. } => {
                collect_leaves(first, output);
                collect_leaves(second, output);
            }
        }
    }
    collect_leaves(&root, &mut restored_leaves);
    let focus = saved_focus
        .or(legacy_focus)
        .filter(|focus| restored_leaves.contains(focus))
        .or_else(|| restored_leaves.first().copied())?;
    TileLayout::from_typed_saved(root, focus, collections)
}

#[cfg(test)]
pub(super) fn prune_restored_node(node: Node, surviving: &HashSet<PaneId>) -> Option<Node> {
    match node {
        Node::Pane(id) => surviving.contains(&id).then_some(Node::Pane(id)),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first = prune_restored_node(*first, surviving);
            let second = prune_restored_node(*second, surviving);
            match (first, second) {
                (Some(first), Some(second)) => Some(Node::Split {
                    direction,
                    ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (Some(remaining), None) | (None, Some(remaining)) => Some(remaining),
                (None, None) => None,
            }
        }
    }
}

pub(super) fn resolve_restored_pane(
    saved_old_id: Option<u32>,
    id_map: &HashMap<u32, PaneId>,
    surviving: &HashSet<PaneId>,
    pane_ids: &[PaneId],
) -> Option<PaneId> {
    saved_old_id
        .and_then(|old_id| id_map.get(&old_id).copied())
        .filter(|pane_id| surviving.contains(pane_id))
        .or_else(|| pane_ids.first().copied())
}

/// Restore a layout tree, remapping every pane ID to a fresh globally unique one.
/// Returns the new tree and a map of old_raw_id → new PaneId.
#[cfg(test)]
pub(super) fn restore_node_remapped(snap: &LayoutSnapshot) -> (Node, HashMap<u32, PaneId>) {
    let mut id_map = HashMap::new();
    let node = remap_inner(snap, &mut id_map);
    (node, id_map)
}

#[cfg(test)]
fn remap_inner(snap: &LayoutSnapshot, id_map: &mut HashMap<u32, PaneId>) -> Node {
    match snap {
        LayoutSnapshot::Pane(old_id) => {
            let new_id = PaneId::alloc();
            id_map.insert(*old_id, new_id);
            Node::Pane(new_id)
        }
        LayoutSnapshot::Collection(_) => Node::Pane(PaneId::alloc()),
        LayoutSnapshot::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let first_node = remap_inner(first, id_map);
            let second_node = remap_inner(second, id_map);
            let dir = match direction {
                DirectionSnapshot::Horizontal => Direction::Horizontal,
                DirectionSnapshot::Vertical => Direction::Vertical,
            };
            Node::Split {
                direction: dir,
                ratio: *ratio,
                first: Box::new(first_node),
                second: Box::new(second_node),
            }
        }
    }
}

#[cfg(test)]
pub(super) fn collect_pane_ids(node: &Node) -> Vec<PaneId> {
    let mut ids = Vec::new();
    collect_ids_inner(node, &mut ids);
    ids
}

#[cfg(test)]
fn collect_ids_inner(node: &Node, ids: &mut Vec<PaneId>) {
    match node {
        Node::Pane(id) => ids.push(*id),
        Node::Split { first, second, .. } => {
            collect_ids_inner(first, ids);
            collect_ids_inner(second, ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    fn pane_snapshot(cwd: &std::path::Path) -> super::super::snapshot::PaneSnapshot {
        super::super::snapshot::PaneSnapshot {
            cwd: cwd.to_path_buf(),
            label: None,
            agent_name: None,
            managed_agent_kind: None,
            agent_session: None,
            launch_argv: None,
        }
    }

    fn tab_snapshot(
        layout: LayoutSnapshot,
        collections: Vec<CollectionSnapshot>,
        panes: HashMap<u32, super::super::snapshot::PaneSnapshot>,
    ) -> TabSnapshot {
        TabSnapshot {
            custom_name: None,
            layout,
            collections,
            panes,
            zoomed: false,
            focused: None,
            focused_leaf: None,
            root_pane: None,
        }
    }

    fn session_snapshot_with_tab(
        tab: TabSnapshot,
        delegations: Vec<crate::delegation::DelegationRecord>,
    ) -> SessionSnapshot {
        SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("restore-test".into()),
                custom_name: None,
                identity_cwd: std::env::current_dir().unwrap(),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: vec![1],
                next_public_tab_number: 2,
                tabs: vec![tab],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
            delegations,
            collection_archive_times: Vec::new(),
        }
    }

    #[cfg(windows)]
    fn test_restore_shell() -> &'static str {
        "C:\\Windows\\System32\\whoami.exe"
    }

    #[cfg(not(windows))]
    fn test_restore_shell() -> &'static str {
        "/bin/sh"
    }

    #[test]
    fn capture_and_restore_node_round_trip() {
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Pane(PaneId::from_raw(0))),
            second: Box::new(Node::Split {
                direction: Direction::Vertical,
                ratio: 0.3,
                first: Box::new(Node::Pane(PaneId::from_raw(1))),
                second: Box::new(Node::Pane(PaneId::from_raw(2))),
            }),
        };

        let snap = super::super::snapshot::capture_node(&node);
        let (restored, id_map) = restore_node_remapped(&snap);

        assert_eq!(id_map.len(), 3);
        let ids = collect_pane_ids(&restored);
        assert_eq!(ids.len(), 3);
        let unique: std::collections::HashSet<u32> = ids.iter().map(|id| id.raw()).collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn malformed_collection_state_promotes_every_surviving_pane() {
        let cwd = std::env::current_dir().unwrap();
        let collection = CollectionId::alloc().expect("collection id");
        let snap = tab_snapshot(
            LayoutSnapshot::Collection(collection),
            Vec::new(),
            HashMap::from([(10, pane_snapshot(&cwd)), (20, pane_snapshot(&cwd))]),
        );
        let (node, id_map) = restore_typed_node_remapped(&snap);
        let surviving: HashSet<_> = id_map.values().copied().collect();

        let layout = recover_typed_layout(&snap, node, &id_map, &surviving)
            .expect("surviving panes should be promoted");

        assert_eq!(layout.pane_ids().len(), 2);
        assert_eq!(layout.tiled_pane_ids().len(), 2);
        assert_eq!(layout.collections().count(), 0);
    }

    #[tokio::test]
    async fn duplicate_collection_ids_across_tabs_and_workspaces_promote_conflicting_members() {
        let cwd = std::env::current_dir().unwrap();
        let collection = CollectionId::alloc().expect("collection id");
        let collection_tab = |old_pane| {
            tab_snapshot(
                LayoutSnapshot::Collection(collection),
                vec![CollectionSnapshot {
                    id: collection,
                    label: Some(format!("helpers-{old_pane}")),
                    members: vec![old_pane],
                    selected: Some(old_pane),
                    archived: Vec::new(),
                }],
                HashMap::from([(old_pane, pane_snapshot(&cwd))]),
            )
        };
        let workspace = |id: &str, tabs: Vec<TabSnapshot>| WorkspaceSnapshot {
            id: Some(id.into()),
            custom_name: None,
            identity_cwd: cwd.clone(),
            checkout: None,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 1,
            public_tab_numbers: (1..=tabs.len()).collect(),
            next_public_tab_number: tabs.len() + 1,
            tabs,
            active_tab: 0,
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![
                workspace(
                    "duplicate-tabs",
                    vec![collection_tab(10), collection_tab(20)],
                ),
                workspace("duplicate-workspace", vec![collection_tab(30)]),
            ],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _delegations, _archive_times, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(workspaces.len(), 2);
        assert_eq!(workspaces[0].tabs.len(), 2);
        assert!(workspaces[0].tabs[0].collection(collection).is_some());
        assert_eq!(workspaces[0].tabs[0].layout.tiled_pane_ids().len(), 0);
        for tab in [&workspaces[0].tabs[1], &workspaces[1].tabs[0]] {
            assert!(tab.collection(collection).is_none());
            assert_eq!(tab.panes.len(), 1);
            assert_eq!(tab.layout.tiled_pane_ids().len(), 1);
        }
        assert_eq!(terminals.len(), 3);
        assert_eq!(runtimes.len(), 3);
        let collection_count = workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .flat_map(|tab| tab.layout.collection_ids())
            .count();
        assert_eq!(collection_count, 1);

        let mut state = crate::app::AppState::test_new();
        state.workspaces = workspaces;
        state.active = Some(0);
        state.selected = 0;
        state.terminals = terminals;
        state.assert_invariants_for_test();
    }

    #[test]
    fn failed_collection_member_recovery_keeps_survivor_and_repairs_selection() {
        let cwd = std::env::current_dir().unwrap();
        let collection = CollectionId::alloc().expect("collection id");
        let snap = tab_snapshot(
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(10)),
                second: Box::new(LayoutSnapshot::Collection(collection)),
            },
            vec![CollectionSnapshot {
                id: collection,
                label: Some("helpers".into()),
                members: vec![20, 30],
                selected: Some(30),
                archived: vec![20, 30],
            }],
            HashMap::from([
                (10, pane_snapshot(&cwd)),
                (20, pane_snapshot(&cwd)),
                (30, pane_snapshot(&cwd)),
            ]),
        );
        let (node, id_map) = restore_typed_node_remapped(&snap);
        let surviving = HashSet::from([id_map[&10], id_map[&20]]);

        let layout = recover_typed_layout(&snap, node, &id_map, &surviving)
            .expect("remaining member should preserve collection");
        let restored = layout.collection(collection).expect("collection survives");

        assert_eq!(restored.members(), &[id_map[&20]]);
        assert_eq!(restored.selected(), Some(id_map[&20]));
        assert!(restored.is_archived(id_map[&20]));
        assert_eq!(layout.pane_ids().len(), 2);
    }

    #[test]
    fn legacy_version_three_focus_migrates_to_typed_pane_focus() {
        let snapshot = super::super::snapshot::parse_snapshot(include_str!(
            "../../tests/fixtures/session/current-herdr-session.json"
        ))
        .expect("v3 fixture parses");
        let snap = &snapshot.workspaces[0].tabs[0];
        let (node, id_map) = restore_typed_node_remapped(snap);
        let surviving: HashSet<_> = id_map.values().copied().collect();

        let layout = recover_typed_layout(snap, node, &id_map, &surviving)
            .expect("legacy pane layout recovers");
        let old_focus = snap.focused.expect("fixture has pane focus");

        assert_eq!(layout.focused_leaf(), LayoutLeaf::Pane(id_map[&old_focus]));
    }

    #[test]
    fn prune_restored_node_collapses_missing_branch() {
        let keep = PaneId::from_raw(11);
        let missing = PaneId::from_raw(12);
        let node = Node::Split {
            direction: Direction::Horizontal,
            ratio: 0.5,
            first: Box::new(Node::Pane(keep)),
            second: Box::new(Node::Pane(missing)),
        };
        let surviving = std::collections::HashSet::from([keep]);

        let pruned = prune_restored_node(node, &surviving).expect("remaining pane should survive");

        assert!(matches!(pruned, Node::Pane(id) if id == keep));
    }

    #[test]
    fn resolve_restored_pane_prefers_surviving_saved_id_and_falls_back_to_first_remaining() {
        let first = PaneId::from_raw(21);
        let second = PaneId::from_raw(22);
        let id_map = HashMap::from([(0_u32, first), (1_u32, second)]);
        let surviving = std::collections::HashSet::from([first]);
        let pane_ids = vec![first];

        assert_eq!(
            resolve_restored_pane(Some(0), &id_map, &surviving, &pane_ids),
            Some(first)
        );
        assert_eq!(
            resolve_restored_pane(Some(1), &id_map, &surviving, &pane_ids),
            Some(first)
        );
    }

    #[test]
    fn restored_worktree_space_membership_drops_missing_checkout() {
        let missing =
            std::env::temp_dir().join(format!("herdr-missing-worktree-{}", std::process::id()));
        let membership = crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: missing.join("repo"),
            checkout_path: missing.join("checkout"),
            is_linked_worktree: true,
        };

        assert_eq!(restored_worktree_space_membership(Some(membership)), None);
    }

    #[test]
    fn restore_plan_respects_opt_in_and_allowlist() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: pi_session_path.clone(),
        };

        assert!(restore_plan_for_snapshot(&session, false).is_none());
        assert_eq!(
            restore_plan_for_snapshot(&session, true).unwrap().argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );

        let unsupported_path = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:claude".into(),
            agent: "claude".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("claude-session"),
        };
        assert!(restore_plan_for_snapshot(&unsupported_path, true).is_none());
    }

    #[test]
    fn restore_plan_selection_suppresses_duplicates() {
        let pi_session_path = test_session_path("pi-session.jsonl");
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: pi_session_path.clone(),
        };
        let mut resumed = HashSet::new();

        assert!(take_restore_plan_for_snapshot(&session, false, &mut resumed).is_none());
        assert!(resumed.is_empty());

        let first = take_restore_plan_for_snapshot(&session, true, &mut resumed)
            .expect("first restore should get a plan");
        assert_eq!(
            first.argv,
            vec!["pi", "--session", pi_session_path.as_str()]
        );
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_some());
        assert!(startup.initial_history_ansi.is_none());
        assert!(!startup.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_suppresses_history_for_duplicate_native_agent_session() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: true,
            resumed_sessions: &mut resumed,
        };

        let first = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);
        let duplicate = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(first.restore_plan.is_some());
        assert!(first.initial_history_ansi.is_none());
        assert!(duplicate.restore_plan.is_none());
        assert!(duplicate.initial_history_ansi.is_none());
        assert!(duplicate.duplicate_agent_session);
    }

    #[test]
    fn pane_restore_startup_keeps_history_without_native_agent_resume() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let history = super::super::snapshot::PaneHistorySnapshot {
            ansi: "RESTORED_HISTORY\r\n".into(),
            lines: 1,
        };
        let mut resumed = HashSet::new();
        let mut agent_restore = AgentRestoreState {
            enabled: false,
            resumed_sessions: &mut resumed,
        };

        let startup = pane_restore_startup(Some(&session), Some(&history), &mut agent_restore);

        assert!(startup.restore_plan.is_none());
        assert_eq!(startup.initial_history_ansi, Some("RESTORED_HISTORY\r\n"));
        assert!(!startup.duplicate_agent_session);
        assert!(resumed.is_empty());
    }

    #[test]
    fn restore_rehydrates_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:hermes".into(),
            agent: "hermes".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Id,
            value: "hermes-session".into(),
        };

        let preserved = restored_terminal_agent_session(Some(&session), false)
            .expect("restore should preserve metadata");
        assert_eq!(preserved.source, "herdr:hermes");
        assert_eq!(preserved.agent, "hermes");
        assert_eq!(preserved.session_ref.value, "hermes-session");
    }

    #[test]
    fn restore_does_not_rehydrate_duplicate_agent_session_metadata() {
        let session = super::super::snapshot::PaneAgentSessionSnapshot {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            kind: crate::agent_resume::AgentSessionRefKind::Path,
            value: test_session_path("pi-session.jsonl"),
        };
        let mut resumed = HashSet::new();
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_some());
        assert!(take_restore_plan_for_snapshot(&session, true, &mut resumed).is_none());

        assert!(restored_terminal_agent_session(Some(&session), true).is_none());
    }

    #[tokio::test]
    async fn restore_carries_persisted_agent_session_metadata() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: Some("reviewer".into()),
                            agent_name: Some("reviewer".into()),
                            managed_agent_kind: Some("opencode".into()),
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "herdr:opencode".into(),
                                agent: "opencode".into(),
                                kind: crate::agent_resume::AgentSessionRefKind::Id,
                                value: "opencode-session".into(),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),

                    collections: Vec::new(),
                    focused_leaf: None,
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (_workspaces, _delegations, _archive_times, terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("restored terminal should exist");
        assert!(
            !terminal.respawn_shell_on_exit,
            "agent sessions should not use native restore lifecycle when resume_agents_on_restore is disabled"
        );
        assert_eq!(terminal.agent_name, None);
        assert_eq!(terminal.manual_label.as_deref(), Some("reviewer"));
        let session = terminal
            .persisted_agent_session
            .as_ref()
            .expect("persisted agent session should survive restore");
        assert_eq!(session.source, "herdr:opencode");
        assert_eq!(session.agent, "opencode");
        assert_eq!(session.session_ref.value, "opencode-session");
    }

    #[tokio::test]
    async fn restore_preserves_public_id_mapping_after_pane_id_remap() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::from([(10, 1), (20, 3)]),
                next_public_pane_number: 4,
                public_tab_numbers: vec![5],
                next_public_tab_number: 6,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Split {
                        direction: super::super::snapshot::DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(10)),
                        second: Box::new(LayoutSnapshot::Pane(20)),
                    },
                    panes: HashMap::from([
                        (
                            10,
                            super::super::snapshot::PaneSnapshot {
                                cwd: cwd.clone(),
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                        (
                            20,
                            super::super::snapshot::PaneSnapshot {
                                cwd: cwd.clone(),
                                label: None,
                                agent_name: None,
                                managed_agent_kind: None,
                                agent_session: None,
                                launch_argv: None,
                            },
                        ),
                    ]),
                    zoomed: false,
                    focused: Some(10),
                    root_pane: Some(10),

                    collections: Vec::new(),
                    focused_leaf: None,
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _delegations, _archive_times, _terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        let mut public_numbers: Vec<_> = workspace.public_pane_numbers.values().copied().collect();
        public_numbers.sort_unstable();
        assert_eq!(public_numbers, vec![1, 3]);
        assert_eq!(workspace.next_public_pane_number, 4);
        assert_eq!(workspace.tabs[0].number, 5);
        assert_eq!(workspace.next_public_tab_number, 6);
    }

    #[tokio::test]
    async fn archive_times_remap_and_discard_malformed_or_unarchived_entries() {
        let cwd = std::env::current_dir().unwrap();
        let root_old = 4_100_010;
        let archived_old = 4_100_020;
        let active_old = 4_100_030;
        let missing_old = 4_100_040;
        let collection = CollectionId::alloc().expect("collection id");
        let mut snapshot = session_snapshot_with_tab(
            tab_snapshot(
                LayoutSnapshot::Split {
                    direction: DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(root_old)),
                    second: Box::new(LayoutSnapshot::Collection(collection)),
                },
                vec![CollectionSnapshot {
                    id: collection,
                    label: None,
                    members: vec![archived_old, active_old],
                    selected: Some(archived_old),
                    archived: vec![archived_old],
                }],
                HashMap::from([
                    (root_old, pane_snapshot(&cwd)),
                    (archived_old, pane_snapshot(&cwd)),
                    (active_old, pane_snapshot(&cwd)),
                ]),
            ),
            Vec::new(),
        );
        snapshot.collection_archive_times = vec![
            super::super::snapshot::CollectionArchiveTimeSnapshot {
                pane_id: archived_old,
                unix_seconds: 1,
                subsec_nanos: 1_000_000_000,
            },
            super::super::snapshot::CollectionArchiveTimeSnapshot {
                pane_id: archived_old,
                unix_seconds: 1_700_000_000,
                subsec_nanos: 123,
            },
            super::super::snapshot::CollectionArchiveTimeSnapshot {
                pane_id: active_old,
                unix_seconds: 1_600_000_000,
                subsec_nanos: 0,
            },
            super::super::snapshot::CollectionArchiveTimeSnapshot {
                pane_id: missing_old,
                unix_seconds: 1_500_000_000,
                subsec_nanos: 0,
            },
        ];
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _delegations, archive_times, _terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let restored_collection = workspaces[0].tabs[0]
            .collection(collection)
            .expect("collection restored");
        let archived = restored_collection
            .archived_members()
            .next()
            .expect("archived member restored");
        assert_ne!(archived.raw(), archived_old);
        assert_eq!(archive_times.len(), 1);
        let expected =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123);
        assert_eq!(archive_times.get(&archived), Some(&expected));

        #[cfg(unix)]
        {
            let mut imports = HashMap::new();
            let (handoff_workspaces, _delegations, handoff_times, _terminals, _runtimes) =
                restore_handoff(
                    &snapshot,
                    0,
                    test_restore_shell(),
                    crate::config::ShellModeConfig::NonLogin,
                    &mut imports,
                    mpsc::channel(4).0,
                    Arc::new(Notify::new()),
                    Arc::new(AtomicBool::new(false)),
                )
                .expect("live handoff restore should preserve archive age");
            let handoff_archived = handoff_workspaces[0].tabs[0]
                .collection(collection)
                .and_then(|collection| collection.archived_members().next())
                .expect("handoff archived member restored");
            assert_eq!(handoff_times.len(), 1);
            assert_eq!(handoff_times.get(&handoff_archived), Some(&expected));
        }
    }

    #[tokio::test]
    async fn delegation_pane_ids_remap_while_relationship_ids_remain_stable() {
        let cwd = std::env::current_dir().unwrap();
        let parent_old = 4_000_010;
        let child_old = 4_000_020;
        let parent_id = crate::delegation::DelegationId::alloc().expect("delegation id");
        let child_id = crate::delegation::DelegationId::alloc().expect("delegation id");
        let mut tab = tab_snapshot(
            LayoutSnapshot::Split {
                direction: DirectionSnapshot::Horizontal,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(parent_old)),
                second: Box::new(LayoutSnapshot::Pane(child_old)),
            },
            Vec::new(),
            HashMap::from([
                (parent_old, pane_snapshot(&cwd)),
                (child_old, pane_snapshot(&cwd)),
            ]),
        );
        tab.root_pane = Some(parent_old);
        tab.focused = Some(child_old);
        let snapshot = session_snapshot_with_tab(
            tab,
            vec![
                crate::delegation::DelegationRecord {
                    id: parent_id,
                    pane_id: Some(PaneId::from_raw(parent_old)),
                    parent_id: None,
                    purpose: Some("parent".into()),
                    sibling_rank: 0,
                    tombstone: false,
                },
                crate::delegation::DelegationRecord {
                    id: child_id,
                    pane_id: Some(PaneId::from_raw(child_old)),
                    parent_id: Some(parent_id),
                    purpose: Some("child".into()),
                    sibling_rank: 0,
                    tombstone: false,
                },
            ],
        );
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, delegations, _archive_times, _terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let restored_panes: HashSet<_> = workspaces[0].tabs[0].panes.keys().copied().collect();
        let parent = delegations.get(parent_id).expect("parent delegation");
        let child = delegations.get(child_id).expect("child delegation");
        assert!(parent
            .pane_id
            .is_some_and(|pane| restored_panes.contains(&pane)));
        assert!(child
            .pane_id
            .is_some_and(|pane| restored_panes.contains(&pane)));
        assert_eq!(child.parent_id, Some(parent_id));
        assert!(!parent.tombstone);
        assert!(!child.tombstone);
    }

    #[tokio::test]
    async fn empty_collection_tab_round_trips_with_optional_root_pane() {
        let collection = CollectionId::alloc().expect("collection id");
        let snapshot = session_snapshot_with_tab(
            tab_snapshot(
                LayoutSnapshot::Collection(collection),
                vec![CollectionSnapshot {
                    id: collection,
                    label: Some("helpers".into()),
                    members: Vec::new(),
                    selected: None,
                    archived: Vec::new(),
                }],
                HashMap::new(),
            ),
            Vec::new(),
        );
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let snapshot = super::super::snapshot::parse_snapshot(&encoded).unwrap();
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _delegations, _archive_times, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].tabs.len(), 1);
        assert_eq!(workspaces[0].tabs[0].root_pane, None);
        assert!(workspaces[0].tabs[0].panes.is_empty());
        assert!(workspaces[0].tabs[0].collection(collection).is_some());
        assert!(terminals.is_empty());
        assert!(runtimes.is_empty());
        workspaces[0].assert_invariants_for_test();
    }

    #[tokio::test]
    async fn cold_restore_with_gapped_public_tab_numbers_drops_unmanaged_agent_name() {
        let cwd = std::env::current_dir().unwrap();
        let pane_snap = |id: &str| {
            (
                id.parse::<u32>().unwrap(),
                super::super::snapshot::PaneSnapshot {
                    cwd: cwd.clone(),
                    label: None,
                    agent_name: None,
                    managed_agent_kind: None,
                    agent_session: None,
                    launch_argv: None,
                },
            )
        };
        let final_pane = super::super::snapshot::PaneSnapshot {
            cwd: cwd.clone(),
            label: Some("planner".into()),
            agent_name: Some("planner".into()),
            managed_agent_kind: None,
            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                kind: crate::agent_resume::AgentSessionRefKind::Id,
                value: "codex-session".into(),
            }),
            launch_argv: None,
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("w1".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::from([(10, 1), (11, 2), (12, 3), (13, 4)]),
                next_public_pane_number: 5,
                public_tab_numbers: vec![1, 3, 4, 5],
                next_public_tab_number: 6,
                tabs: vec![
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(10),
                        panes: HashMap::from([pane_snap("10")]),
                        zoomed: false,
                        focused: Some(10),
                        root_pane: Some(10),

                        collections: Vec::new(),
                        focused_leaf: None,
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(11),
                        panes: HashMap::from([pane_snap("11")]),
                        zoomed: false,
                        focused: Some(11),
                        root_pane: Some(11),

                        collections: Vec::new(),
                        focused_leaf: None,
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(12),
                        panes: HashMap::from([pane_snap("12")]),
                        zoomed: false,
                        focused: Some(12),
                        root_pane: Some(12),

                        collections: Vec::new(),
                        focused_leaf: None,
                    },
                    TabSnapshot {
                        custom_name: None,
                        layout: LayoutSnapshot::Pane(13),
                        panes: HashMap::from([(13, final_pane)]),
                        zoomed: false,
                        focused: Some(13),
                        root_pane: Some(13),

                        collections: Vec::new(),
                        focused_leaf: None,
                    },
                ],
                active_tab: 3,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (workspaces, _delegations, _archive_times, terminals, _runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let workspace = workspaces.first().expect("workspace should restore");
        assert_eq!(workspace.active_tab, 3);
        assert_eq!(workspace.tabs[3].number, 5);
        let agent_pane = workspace.tabs[3].root_pane.expect("test tab has root pane");
        let terminal_id = &workspace.tabs[3].panes[&agent_pane].attached_terminal_id;
        assert!(terminals[terminal_id].agent_name.is_none());
        assert_eq!(terminals[terminal_id].managed_agent_kind(), None);
        assert!(workspace
            .pane_details(&terminals)
            .into_iter()
            .all(|detail| detail.pane_id != agent_pane));
    }

    #[test]
    fn legacy_restore_precomputes_missing_public_pane_numbers() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = WorkspaceSnapshot {
            id: Some("w1".into()),
            custom_name: None,
            identity_cwd: cwd,
            checkout: None,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![TabSnapshot {
                custom_name: None,
                layout: LayoutSnapshot::Split {
                    direction: super::super::snapshot::DirectionSnapshot::Horizontal,
                    ratio: 0.5,
                    first: Box::new(LayoutSnapshot::Pane(10)),
                    second: Box::new(LayoutSnapshot::Pane(20)),
                },
                panes: HashMap::new(),
                zoomed: false,
                focused: Some(10),
                root_pane: Some(10),

                collections: Vec::new(),
                focused_leaf: None,
            }],
            active_tab: 0,
        };
        let mut next_public_pane_number = 1;

        let public_numbers =
            migrated_public_pane_numbers_by_old_raw(&snapshot, &mut next_public_pane_number);

        assert_eq!(public_numbers, HashMap::from([(10, 1), (20, 2)]));
        assert_eq!(next_public_pane_number, 3);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn native_agent_restore_defers_runtime_launch() {
        let cwd = std::env::current_dir().unwrap();
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd.clone(),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneSnapshot {
                            cwd,
                            label: None,
                            agent_name: None,
                            managed_agent_kind: None,
                            agent_session: Some(super::super::snapshot::PaneAgentSessionSnapshot {
                                source: "herdr:codex".into(),
                                agent: "codex".into(),
                                kind: crate::agent_resume::AgentSessionRefKind::Id,
                                value: "codex-session".into(),
                            }),
                            launch_argv: None,
                        },
                    )]),
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),

                    collections: Vec::new(),
                    focused_leaf: None,
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let (events, _event_rx) = mpsc::channel(4);

        let (_workspaces, _delegations, _archive_times, terminals, runtimes) = restore(
            &snapshot,
            None,
            24,
            80,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            true,
            events,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        );

        let terminal = terminals
            .values()
            .next()
            .expect("native agent restore should create terminal state");
        assert!(
            terminal.pending_agent_resume_plan.is_some(),
            "restored native agent panes should defer resume until client terminal context is known"
        );
        assert!(
            !terminal.respawn_shell_on_exit,
            "deferred agent resume should not use native restore lifecycle before launch"
        );
        assert!(
            runtimes.is_empty(),
            "native agent restore should not spawn a fallback-size runtime during snapshot restore"
        );
        let mut imports = HashMap::new();
        let (
            _handoff_workspaces,
            _handoff_delegations,
            _archive_times,
            handoff_terminals,
            handoff_runtimes,
        ) = restore_handoff(
            &snapshot,
            0,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            &mut imports,
            mpsc::channel(4).0,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("handoff restore should preserve pending native agent resume");
        let handoff_terminal = handoff_terminals
            .values()
            .next()
            .expect("handoff restore should create terminal state");
        assert!(
            handoff_terminal.pending_agent_resume_plan.is_some(),
            "handoff restore should preserve pending native agent resume intent"
        );
        assert!(
            handoff_runtimes.is_empty(),
            "handoff restore should not replace pending native agent resume with a shell runtime"
        );
    }

    #[tokio::test]
    async fn restore_seeds_saved_pane_history_into_runtime() {
        let (snapshot, history) = snapshot_with_saved_pane_history();
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));

        let (_workspaces, _delegations, _archive_times, _terminals, runtimes) = restore(
            &snapshot,
            Some(&history),
            5,
            40,
            4096,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            render_notify,
            render_dirty,
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        let restored_text = runtime.recent_unwrapped_text(10);
        assert!(
            restored_text.contains("RESTORED_HISTORY 👨‍👩‍👧 LINK"),
            "styled Unicode and hyperlink text should survive history replay"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    #[tokio::test]
    async fn restore_without_history_snapshot_keeps_pane_contents_empty() {
        let (snapshot, _history) = snapshot_with_saved_pane_history();
        let (events, _events_rx) = mpsc::channel(8);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));

        let (_workspaces, _delegations, _archive_times, _terminals, runtimes) = restore(
            &snapshot,
            None,
            5,
            40,
            4096,
            test_restore_shell(),
            crate::config::ShellModeConfig::NonLogin,
            false,
            events,
            render_notify,
            render_dirty,
        );
        let runtime = runtimes
            .values()
            .next()
            .expect("restored runtime should exist");

        assert!(
            !runtime
                .recent_unwrapped_text(10)
                .contains("RESTORED_HISTORY"),
            "pane history should not restore unless a history snapshot is supplied"
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while runtime.cwd().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = runtime.try_send_bytes(bytes::Bytes::from_static(b"exit\n"));
    }

    fn snapshot_with_saved_pane_history() -> (SessionSnapshot, SessionHistorySnapshot) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let mut panes = HashMap::new();
        panes.insert(
            0,
            super::super::snapshot::PaneSnapshot {
                cwd: cwd.clone(),
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        let history = SessionHistorySnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![super::super::snapshot::TabHistorySnapshot {
                    panes: HashMap::from([(
                        0,
                        super::super::snapshot::PaneHistorySnapshot {
                            ansi: concat!(
                                "\x1b[31mRESTORED_HISTORY 👨‍👩‍👧\x1b[0m ",
                                "\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\\r\n"
                            )
                            .to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        };
        let snapshot = SessionSnapshot {
            version: super::super::snapshot::SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("workspace".into()),
                custom_name: None,
                identity_cwd: cwd,
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Pane(0),
                    panes,
                    zoomed: false,
                    focused: Some(0),
                    root_pane: Some(0),

                    collections: Vec::new(),
                    focused_leaf: None,
                }],
                active_tab: 0,
            }],
            active: Some(0),
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: Default::default(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        (snapshot, history)
    }
}
