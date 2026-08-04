use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::layout::Direction;
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::layout::Node;
use crate::terminal::TerminalRuntimeRegistry;
use crate::workspace::Workspace;

/// Current snapshot format version.
pub(super) const SNAPSHOT_VERSION: u32 = 4;

/// Serializable snapshot of the entire herdr session.
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Format version — used to detect incompatible changes.
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnapshot>,
    #[serde(default)]
    pub repositories: Vec<crate::repository::Repository>,
    #[serde(default)]
    pub space_order: Vec<crate::repository::SpaceRef>,
    pub active: Option<usize>,
    pub selected: usize,
    #[serde(default)]
    pub sidebar_width: Option<u16>,
    #[serde(default)]
    pub sidebar_section_split: Option<f32>,
    /// Read-only compatibility with v3. Expansion is client-local in v4.
    #[serde(default, skip_serializing)]
    pub collapsed_space_keys: std::collections::HashSet<String>,
    /// Session-wide delegation provenance. Pane IDs use the pre-restore raw IDs.
    #[serde(default)]
    pub delegations: Vec<crate::delegation::DelegationRecord>,
    /// Archive ages for collection members. Pane IDs use the pre-restore raw IDs.
    #[serde(default)]
    pub collection_archive_times: Vec<CollectionArchiveTimeSnapshot>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CollectionArchiveTimeSnapshot {
    pub pane_id: u32,
    pub unix_seconds: u64,
    #[serde(default)]
    pub subsec_nanos: u32,
}

#[derive(Serialize, Deserialize)]
pub struct SessionHistorySnapshot {
    /// Format version follows the matching session snapshot version.
    #[serde(default)]
    pub version: u32,
    pub workspaces: Vec<WorkspaceHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceHistorySnapshot {
    pub tabs: Vec<TabHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct TabHistorySnapshot {
    pub panes: HashMap<u32, PaneHistorySnapshot>,
}

#[derive(Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub custom_name: Option<String>,
    pub identity_cwd: PathBuf,
    /// Immutable checkout identity, retained independently of a pane's live CWD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<crate::repository::CheckoutProvenance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_space: Option<crate::workspace::WorktreeSpaceMembership>,
    #[serde(default)]
    pub public_pane_numbers: HashMap<u32, usize>,
    #[serde(default)]
    pub next_public_pane_number: usize,
    #[serde(default)]
    pub public_tab_numbers: Vec<usize>,
    #[serde(default)]
    pub next_public_tab_number: usize,
    pub tabs: Vec<TabSnapshot>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Deserialize)]
struct LegacyWorkspaceSnapshot {
    #[serde(default)]
    custom_name: Option<String>,
    layout: LayoutSnapshot,
    panes: HashMap<u32, PaneSnapshot>,
    zoomed: bool,
    #[serde(default)]
    focused: Option<u32>,
    #[serde(default)]
    root_pane: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct TabSnapshot {
    #[serde(default)]
    pub custom_name: Option<String>,
    pub layout: LayoutSnapshot,
    #[serde(default)]
    pub collections: Vec<CollectionSnapshot>,
    pub panes: HashMap<u32, PaneSnapshot>,
    pub zoomed: bool,
    /// Legacy pane-only focus, retained for snapshots before version 4.
    #[serde(default)]
    pub focused: Option<u32>,
    #[serde(default)]
    pub focused_leaf: Option<LayoutLeafSnapshot>,
    #[serde(default)]
    pub root_pane: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSnapshot {
    pub id: crate::layout::CollectionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub members: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<u32>,
    #[serde(default)]
    pub archived: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum LayoutLeafSnapshot {
    Pane(u32),
    Collection(crate::layout::CollectionId),
}

fn default_pane_seen() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub cwd: PathBuf,
    #[serde(default = "default_pane_seen")]
    pub seen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_agent_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<PaneAgentSessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneAgentSessionSnapshot {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}

#[derive(Serialize, Deserialize)]
pub struct PaneHistorySnapshot {
    pub ansi: String,
    pub lines: usize,
}

/// Serializable BSP tree.
#[derive(Serialize, Deserialize)]
pub enum LayoutSnapshot {
    Pane(u32),
    Collection(crate::layout::CollectionId),
    Split {
        direction: DirectionSnapshot,
        ratio: f32,
        first: Box<LayoutSnapshot>,
        second: Box<LayoutSnapshot>,
    },
}

#[derive(Serialize, Deserialize)]
pub enum DirectionSnapshot {
    Horizontal,
    Vertical,
}

impl From<LegacyWorkspaceSnapshot> for WorkspaceSnapshot {
    fn from(snap: LegacyWorkspaceSnapshot) -> Self {
        let identity_cwd = legacy_identity_cwd(&snap);
        let tab = TabSnapshot {
            custom_name: None,
            layout: snap.layout,
            collections: Vec::new(),
            panes: snap.panes,
            zoomed: snap.zoomed,
            focused: snap.focused,
            focused_leaf: None,
            root_pane: snap.root_pane,
        };

        Self {
            id: None,
            custom_name: snap.custom_name,
            identity_cwd,
            checkout: None,
            worktree_space: None,
            public_pane_numbers: HashMap::new(),
            next_public_pane_number: 0,
            public_tab_numbers: Vec::new(),
            next_public_tab_number: 0,
            tabs: vec![tab],
            active_tab: 0,
        }
    }
}

#[derive(Deserialize)]
struct RawSessionSnapshot {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    workspaces: Vec<serde_json::Value>,
    #[serde(default)]
    repositories: Vec<crate::repository::Repository>,
    #[serde(default)]
    space_order: Vec<crate::repository::SpaceRef>,
    #[serde(default)]
    active: Option<usize>,
    #[serde(default)]
    selected: usize,
    #[serde(default)]
    sidebar_width: Option<u16>,
    #[serde(default)]
    sidebar_section_split: Option<f32>,
    #[serde(default)]
    collapsed_space_keys: std::collections::HashSet<String>,
    #[serde(default)]
    delegations: Vec<crate::delegation::DelegationRecord>,
    #[serde(default)]
    collection_archive_times: Vec<CollectionArchiveTimeSnapshot>,
}

fn migrate_snapshot(raw: RawSessionSnapshot) -> Result<SessionSnapshot, String> {
    let workspaces = raw
        .workspaces
        .into_iter()
        .map(migrate_workspace)
        .collect::<Result<Vec<_>, _>>()?;
    let (repositories, space_order) = if raw.repositories.is_empty() || raw.space_order.is_empty() {
        migrate_repository_state(&workspaces)
    } else {
        (raw.repositories, raw.space_order)
    };
    Ok(SessionSnapshot {
        version: raw.version,
        workspaces,
        repositories,
        space_order,
        active: raw.active,
        selected: raw.selected,
        sidebar_width: raw.sidebar_width,
        sidebar_section_split: raw.sidebar_section_split,
        collapsed_space_keys: raw.collapsed_space_keys,
        delegations: raw.delegations,
        collection_archive_times: raw.collection_archive_times,
    })
}

fn migrate_repository_state(
    workspaces: &[WorkspaceSnapshot],
) -> (
    Vec<crate::repository::Repository>,
    Vec<crate::repository::SpaceRef>,
) {
    let mut repositories = Vec::<crate::repository::Repository>::new();
    let mut order = Vec::new();
    for (idx, workspace) in workspaces.iter().enumerate() {
        let workspace_id = workspace
            .id
            .clone()
            .unwrap_or_else(|| format!("legacy-w{}", idx + 1));
        let identity = workspace.worktree_space.clone().or_else(|| {
            crate::workspace::git_space_metadata(&workspace.identity_cwd).map(|space| {
                crate::workspace::WorktreeSpaceMembership {
                    key: space.key,
                    label: space.repo_name,
                    repo_root: space.repo_root.clone(),
                    checkout_path: space.repo_root,
                    is_linked_worktree: space.is_linked_worktree,
                }
            })
        });
        let Some(identity) = identity else {
            order.push(crate::repository::SpaceRef::StandaloneWorkspace(
                workspace_id,
            ));
            continue;
        };
        let common_dir = PathBuf::from(&identity.key);
        let repository_id = crate::repository::stable_repository_id(&common_dir);
        if let Some(repository) = repositories
            .iter_mut()
            .find(|repository| repository.id == repository_id)
        {
            repository.checkout_workspace_ids.push(workspace_id);
        } else {
            repositories.push(crate::repository::Repository {
                id: repository_id.clone(),
                git_common_dir: common_dir,
                label: identity.label,
                custom_name: None,
                preferred_base: None,
                checkout_workspace_ids: vec![workspace_id.clone()],
                last_focused_workspace_id: Some(workspace_id),
            });
            order.push(crate::repository::SpaceRef::Repository(repository_id));
        }
    }
    (repositories, order)
}

fn migrate_workspace(raw: serde_json::Value) -> Result<WorkspaceSnapshot, String> {
    if raw.get("identity_cwd").is_some() {
        return serde_json::from_value(raw).map_err(|e| e.to_string());
    }

    if raw.get("layout").is_some() {
        let legacy =
            serde_json::from_value::<LegacyWorkspaceSnapshot>(raw).map_err(|e| e.to_string())?;
        return Ok(legacy.into());
    }

    Err("workspace snapshot is neither current nor legacy format".to_string())
}

fn legacy_identity_cwd(snap: &LegacyWorkspaceSnapshot) -> PathBuf {
    let root_pane = snap
        .root_pane
        .or_else(|| first_pane_id_in_layout(&snap.layout));

    root_pane
        .and_then(|pane_id| snap.panes.get(&pane_id))
        .map(|pane| pane.cwd.clone())
        .or_else(|| {
            first_pane_id_in_layout(&snap.layout)
                .and_then(|pane_id| snap.panes.get(&pane_id))
                .map(|pane| pane.cwd.clone())
        })
        .or_else(|| {
            snap.panes
                .keys()
                .min()
                .and_then(|pane_id| snap.panes.get(pane_id))
                .map(|pane| pane.cwd.clone())
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()))
}

fn first_pane_id_in_layout(layout: &LayoutSnapshot) -> Option<u32> {
    match layout {
        LayoutSnapshot::Pane(id) => Some(*id),
        LayoutSnapshot::Collection(_) => None,
        LayoutSnapshot::Split { first, second, .. } => {
            first_pane_id_in_layout(first).or_else(|| first_pane_id_in_layout(second))
        }
    }
}

/// Capture the current app state into a serializable snapshot.
#[allow(clippy::too_many_arguments)]
pub fn capture(
    workspaces: &[Workspace],
    repositories: &[crate::repository::Repository],
    space_order: &[crate::repository::SpaceRef],
    delegations: &crate::delegation::Delegations,
    collection_archive_times: &std::collections::HashMap<
        crate::layout::PaneId,
        std::time::SystemTime,
    >,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
    active: Option<usize>,
    selected: usize,
    sidebar_width: u16,
    sidebar_section_split: f32,
    collapsed_space_keys: std::collections::HashSet<String>,
) -> SessionSnapshot {
    SessionSnapshot {
        version: SNAPSHOT_VERSION,
        workspaces: workspaces
            .iter()
            .map(|workspace| capture_workspace(workspace, terminals, terminal_runtimes))
            .collect(),
        repositories: repositories.to_vec(),
        space_order: space_order.to_vec(),
        active,
        selected,
        sidebar_width: Some(sidebar_width),
        sidebar_section_split: Some(sidebar_section_split),
        collapsed_space_keys,
        delegations: {
            let mut records: Vec<_> = delegations.records().values().cloned().collect();
            records.sort_by_key(|record| record.id);
            records
        },
        collection_archive_times: {
            let mut records: Vec<_> = collection_archive_times
                .iter()
                .filter_map(|(pane_id, archived_at)| {
                    let elapsed = archived_at
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .ok()?;
                    Some(CollectionArchiveTimeSnapshot {
                        pane_id: pane_id.raw(),
                        unix_seconds: elapsed.as_secs(),
                        subsec_nanos: elapsed.subsec_nanos(),
                    })
                })
                .collect::<Vec<_>>();
            records.sort_by_key(|record| record.pane_id);
            records
        },
    }
}

fn capture_workspace(
    ws: &Workspace,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> WorkspaceSnapshot {
    WorkspaceSnapshot {
        id: Some(ws.id.clone()),
        custom_name: ws.custom_name.clone(),
        // A Checkout is bound to its canonical checkout path. Panes may cd
        // elsewhere, but that mutable CWD must not rewrite restore identity.
        identity_cwd: ws
            .checkout
            .as_ref()
            .map(|checkout| checkout.checkout_path.clone())
            .or_else(|| ws.resolved_identity_cwd_from(terminals, terminal_runtimes))
            .unwrap_or_else(|| ws.identity_cwd.clone()),
        checkout: ws.checkout.clone(),
        worktree_space: ws.worktree_space.clone(),
        public_pane_numbers: ws
            .public_pane_numbers
            .iter()
            .map(|(pane_id, number)| (pane_id.raw(), *number))
            .collect(),
        next_public_pane_number: ws.next_public_pane_number,
        public_tab_numbers: ws.tabs.iter().map(|tab| tab.number).collect(),
        next_public_tab_number: ws.next_public_tab_number,
        tabs: ws
            .tabs
            .iter()
            .map(|tab| capture_tab(tab, terminals, terminal_runtimes))
            .collect(),
        active_tab: ws.active_tab,
    }
}

fn capture_tab(
    tab: &crate::workspace::Tab,
    terminals: &std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> TabSnapshot {
    let mut panes = HashMap::new();
    for (id, pane) in &tab.panes {
        let cwd = tab
            .cwd_for_pane(*id, terminals, terminal_runtimes)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let terminal = terminals.get(&pane.attached_terminal_id);
        let label = terminal.and_then(|terminal| terminal.manual_label.clone());
        let (agent_name, managed_agent_kind) = terminal
            .filter(|terminal| !terminal.managed_agent_launch_pending())
            .map(|terminal| {
                (
                    terminal.agent_name.clone(),
                    terminal
                        .managed_agent_kind()
                        .map(|agent| crate::detect::agent_label(agent).to_string()),
                )
            })
            .unwrap_or_default();
        let launch_argv = terminal.and_then(|terminal| terminal.launch_argv.clone());
        let agent_session = terminal.and_then(|terminal| {
            if let Some(authority) = terminal.hook_authority.as_ref() {
                if let Some(session_ref) = authority.session_ref.as_ref() {
                    return Some(PaneAgentSessionSnapshot {
                        source: authority.source.clone(),
                        agent: authority.agent_label.clone(),
                        kind: session_ref.kind,
                        value: session_ref.value.clone(),
                    });
                }
            }
            terminal
                .persisted_agent_session
                .as_ref()
                .map(|session| PaneAgentSessionSnapshot {
                    source: session.source.clone(),
                    agent: session.agent.clone(),
                    kind: session.session_ref.kind,
                    value: session.session_ref.value.clone(),
                })
        });
        panes.insert(
            id.raw(),
            PaneSnapshot {
                cwd,
                seen: pane.seen,
                label,
                agent_name,
                managed_agent_kind,
                agent_session,
                launch_argv,
            },
        );
    }
    let mut collections: Vec<_> = tab
        .layout
        .collections()
        .map(|collection| CollectionSnapshot {
            id: collection.id,
            label: collection.label.clone(),
            members: collection.members().iter().map(|id| id.raw()).collect(),
            selected: collection.selected().map(|id| id.raw()),
            archived: collection.archived_members().map(|id| id.raw()).collect(),
        })
        .collect();
    collections.sort_by_key(|collection| collection.id.raw());
    TabSnapshot {
        custom_name: tab.custom_name.clone(),
        layout: capture_typed_node(tab.layout.typed_root()),
        collections,
        panes,
        zoomed: tab.zoomed,
        focused: None,
        focused_leaf: Some(capture_layout_leaf(tab.layout.focused_leaf())),
        root_pane: tab.root_pane.map(|pane| pane.raw()),
    }
}

/// Capture pane screen history separately from the structural session snapshot.
pub fn capture_history(
    workspaces: &[Workspace],
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> SessionHistorySnapshot {
    SessionHistorySnapshot {
        version: SNAPSHOT_VERSION,
        workspaces: workspaces
            .iter()
            .map(|workspace| WorkspaceHistorySnapshot {
                tabs: workspace
                    .tabs
                    .iter()
                    .map(|tab| TabHistorySnapshot {
                        panes: capture_tab_history(tab, terminal_runtimes),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn capture_tab_history(
    tab: &crate::workspace::Tab,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> HashMap<u32, PaneHistorySnapshot> {
    let mut panes = HashMap::new();
    for (id, pane) in &tab.panes {
        if let Some(history) = capture_pane_history(Some(pane), terminal_runtimes) {
            panes.insert(id.raw(), history);
        }
    }
    panes
}

fn capture_pane_history(
    pane: Option<&crate::pane::PaneState>,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> Option<PaneHistorySnapshot> {
    let ansi = terminal_runtimes
        .get(&pane?.attached_terminal_id)?
        .snapshot_history()?;
    let lines = ansi.lines().count();
    Some(PaneHistorySnapshot { ansi, lines })
}

fn capture_layout_leaf(leaf: crate::layout::LayoutLeaf) -> LayoutLeafSnapshot {
    match leaf {
        crate::layout::LayoutLeaf::Pane(id) => LayoutLeafSnapshot::Pane(id.raw()),
        crate::layout::LayoutLeaf::Collection(id) => LayoutLeafSnapshot::Collection(id),
    }
}

fn capture_typed_node(node: &crate::layout::TypedNode) -> LayoutSnapshot {
    match node {
        crate::layout::TypedNode::Leaf(leaf) => match leaf {
            crate::layout::LayoutLeaf::Pane(id) => LayoutSnapshot::Pane(id.raw()),
            crate::layout::LayoutLeaf::Collection(id) => LayoutSnapshot::Collection(*id),
        },
        crate::layout::TypedNode::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: match direction {
                Direction::Horizontal => DirectionSnapshot::Horizontal,
                Direction::Vertical => DirectionSnapshot::Vertical,
            },
            ratio: *ratio,
            first: Box::new(capture_typed_node(first)),
            second: Box::new(capture_typed_node(second)),
        },
    }
}

#[cfg(test)]
pub(super) fn capture_node(node: &Node) -> LayoutSnapshot {
    match node {
        Node::Pane(id) => LayoutSnapshot::Pane(id.raw()),
        Node::Split {
            direction,
            ratio,
            first,
            second,
        } => LayoutSnapshot::Split {
            direction: match direction {
                Direction::Horizontal => DirectionSnapshot::Horizontal,
                Direction::Vertical => DirectionSnapshot::Vertical,
            },
            ratio: *ratio,
            first: Box::new(capture_node(first)),
            second: Box::new(capture_node(second)),
        },
    }
}

pub(super) fn parse_snapshot(content: &str) -> Result<SessionSnapshot, String> {
    let raw = serde_json::from_str::<RawSessionSnapshot>(content).map_err(|e| e.to_string())?;
    if raw.version > SNAPSHOT_VERSION {
        return Err(format!(
            "snapshot version {} is newer than supported {}",
            raw.version, SNAPSHOT_VERSION
        ));
    }
    migrate_snapshot(raw)
}

pub(super) fn parse_history_snapshot(content: &str) -> Result<SessionHistorySnapshot, String> {
    let snapshot =
        serde_json::from_str::<SessionHistorySnapshot>(content).map_err(|e| e.to_string())?;
    if snapshot.version > SNAPSHOT_VERSION {
        return Err(format!(
            "history snapshot version {} is newer than supported {}",
            snapshot.version, SNAPSHOT_VERSION
        ));
    }
    Ok(snapshot)
}

pub(super) fn snapshot_file_version(content: &str) -> Option<u32> {
    serde_json::from_str::<RawSessionSnapshot>(content)
        .ok()
        .map(|raw| raw.version)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use ratatui::layout::{Direction, Rect};

    use super::*;
    use crate::app::{AppState, Mode};
    use crate::layout::NavDirection;
    use crate::workspace::Workspace;

    fn session_fixture(name: &str) -> &'static str {
        match name {
            "current-herdr" => {
                include_str!("../../tests/fixtures/session/current-herdr-session.json")
            }
            "current-herdr-dev" => {
                include_str!("../../tests/fixtures/session/current-herdr-dev-session.json")
            }
            "legacy-pre-tabs-v2" => {
                include_str!("../../tests/fixtures/session/legacy-pre-tabs-v2.json")
            }
            other => panic!("unknown session fixture: {other}"),
        }
    }

    fn test_session_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    fn state_with_workspaces(names: &[&str]) -> AppState {
        let mut state = AppState::test_new();
        state.workspaces = names.iter().map(|name| Workspace::test_new(name)).collect();
        state.ensure_test_terminals();
        if !state.workspaces.is_empty() {
            state.active = Some(0);
            state.selected = 0;
            state.mode = Mode::Terminal;
        }
        state
    }

    fn capture_from_state(state: &AppState) -> SessionSnapshot {
        let terminal_runtimes = TerminalRuntimeRegistry::new();
        capture_from_state_with_runtimes(state, &terminal_runtimes)
    }

    fn capture_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionSnapshot {
        capture(
            &state.workspaces,
            &state.repositories,
            &state.space_order,
            &state.delegations,
            &state.collection_archive_times,
            &state.terminals,
            terminal_runtimes,
            state.active,
            state.selected,
            state.sidebar_width,
            state.sidebar_section_split,
            state.collapsed_space_keys.clone(),
        )
    }

    fn capture_history_from_state_with_runtimes(
        state: &AppState,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> SessionHistorySnapshot {
        capture_history(&state.workspaces, terminal_runtimes)
    }

    fn root_split_ratio(tab: &TabSnapshot) -> Option<f32> {
        match &tab.layout {
            LayoutSnapshot::Split { ratio, .. } => Some(*ratio),
            LayoutSnapshot::Pane(_) | LayoutSnapshot::Collection(_) => None,
        }
    }

    #[test]
    fn managed_agent_snapshot_omits_pending_and_persists_active_ownership() {
        let mut state = state_with_workspaces(&["managed-snapshot"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let now = std::time::Instant::now();
        state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .begin_managed_agent(
                "reviewer".into(),
                crate::detect::Agent::Pi,
                now,
                std::time::Duration::ZERO,
                std::time::Duration::from_secs(1),
            );

        let pending = capture_from_state(&state);
        let pending_pane = &pending.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(pending_pane.agent_name, None);
        assert_eq!(pending_pane.managed_agent_kind, None);

        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        assert!(terminal.reconcile_managed_agent_at(now, false));
        let active = capture_from_state(&state);
        let active_pane = &active.workspaces[0].tabs[0].panes[&root.raw()];
        assert_eq!(active_pane.agent_name.as_deref(), Some("reviewer"));
        assert_eq!(active_pane.managed_agent_kind.as_deref(), Some("pi"));
    }

    #[test]
    fn round_trip_empty_session() {
        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![],
            active: None,
            selected: 0,
            sidebar_width: Some(26),
            sidebar_section_split: Some(0.5),
            collapsed_space_keys: std::collections::HashSet::new(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();
        assert!(restored.workspaces.is_empty());
        assert_eq!(restored.active, None);
        assert_eq!(restored.sidebar_width, Some(26));
        assert_eq!(restored.sidebar_section_split, Some(0.5));
    }

    #[test]
    fn round_trip_layout_snapshot() {
        let layout = LayoutSnapshot::Split {
            direction: DirectionSnapshot::Horizontal,
            ratio: 0.6,
            first: Box::new(LayoutSnapshot::Pane(0)),
            second: Box::new(LayoutSnapshot::Split {
                direction: DirectionSnapshot::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutSnapshot::Pane(1)),
                second: Box::new(LayoutSnapshot::Pane(2)),
            }),
        };
        let json = serde_json::to_string(&layout).unwrap();
        let restored: LayoutSnapshot = serde_json::from_str(&json).unwrap();

        match restored {
            LayoutSnapshot::Split { ratio, .. } => assert!((ratio - 0.6).abs() < 0.01),
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn typed_collection_snapshot_round_trips_membership_focus_and_archive_state() {
        let mut state = state_with_workspaces(&["typed"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let member = state.workspaces[0].test_split(Direction::Horizontal);
        let collection = state.workspaces[0]
            .create_collection_near(
                0,
                crate::layout::LayoutLeaf::Pane(root),
                Direction::Vertical,
                0.4,
                Some("helpers".into()),
            )
            .expect("create collection");
        state.workspaces[0]
            .collect_pane(member, collection)
            .expect("collect member");
        state.workspaces[0]
            .set_collection_member_archived(member, collection, true)
            .expect("archive member");
        let archived_at = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::new(1_700_000_000, 123_456_789);
        state.collection_archive_times.insert(member, archived_at);
        assert!(state.workspaces[0].tabs[0]
            .layout
            .focus_leaf(crate::layout::LayoutLeaf::Collection(collection)));

        let encoded = serde_json::to_string(&capture_from_state(&state)).unwrap();
        let restored = parse_snapshot(&encoded).unwrap();
        let tab = &restored.workspaces[0].tabs[0];

        assert!(matches!(
            tab.focused_leaf,
            Some(LayoutLeafSnapshot::Collection(id)) if id == collection
        ));
        assert!(matches!(
            &tab.layout,
            LayoutSnapshot::Split {
                second,
                ..
            } if matches!(**second, LayoutSnapshot::Collection(id) if id == collection)
        ));
        assert_eq!(tab.collections.len(), 1);
        assert_eq!(tab.collections[0].members, vec![member.raw()]);
        assert_eq!(tab.collections[0].selected, Some(member.raw()));
        assert_eq!(tab.collections[0].archived, vec![member.raw()]);
        assert_eq!(tab.collections[0].label.as_deref(), Some("helpers"));
        assert_eq!(restored.collection_archive_times.len(), 1);
        assert_eq!(restored.collection_archive_times[0].pane_id, member.raw());
        assert_eq!(
            restored.collection_archive_times[0].unix_seconds,
            1_700_000_000
        );
        assert_eq!(
            restored.collection_archive_times[0].subsec_nanos,
            123_456_789
        );
    }

    #[test]
    fn version_three_snapshot_preserves_legacy_pane_focus_for_migration() {
        let snap = parse_snapshot(session_fixture("current-herdr")).unwrap();
        let tab = &snap.workspaces[0].tabs[0];

        assert_eq!(snap.version, 3);
        assert!(tab.focused.is_some());
        assert_eq!(tab.focused_leaf, None);
        assert!(tab.collections.is_empty());
        assert!(snap.collection_archive_times.is_empty());
    }

    #[test]
    fn optional_root_pane_persists_for_empty_collection_tab() {
        let mut state = state_with_workspaces(&["empty-collection"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let collection = state.workspaces[0]
            .create_collection_near(
                0,
                crate::layout::LayoutLeaf::Pane(root),
                Direction::Horizontal,
                0.5,
                Some("helpers".into()),
            )
            .expect("create collection");
        let taken = state.workspaces[0]
            .take_pane_for_move(root)
            .expect("detach sole pane");
        state.workspaces[0].unregister_moved_pane(taken.moved.pane_id);
        state.workspaces[0].assert_invariants_for_test();

        let encoded = serde_json::to_string(&capture_from_state(&state)).unwrap();
        let restored = parse_snapshot(&encoded).unwrap();
        let tab = &restored.workspaces[0].tabs[0];

        assert_eq!(tab.root_pane, None);
        assert!(tab.panes.is_empty());
        assert!(matches!(
            &tab.layout,
            LayoutSnapshot::Collection(id) if *id == collection
        ));
        assert_eq!(tab.collections.len(), 1);
        assert!(tab.collections[0].members.is_empty());
    }

    #[test]
    fn round_trip_full_workspace_snapshot() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/herdr"),
                seen: true,
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: PathBuf::from("/home/can/Projects/website"),
                seen: false,
                label: Some("website".into()),
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );

        let snap = SessionSnapshot {
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("wproj".to_string()),
                custom_name: Some("pi-mono".to_string()),
                identity_cwd: PathBuf::from("/home/can/Projects/herdr"),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::from([(0, 1), (1, 2)]),
                next_public_pane_number: 3,
                public_tab_numbers: vec![1],
                next_public_tab_number: 2,
                tabs: vec![TabSnapshot {
                    custom_name: Some("api".to_string()),
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
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
            collapsed_space_keys: std::collections::HashSet::new(),
            version: SNAPSHOT_VERSION,

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };

        let json = serde_json::to_string_pretty(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();

        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(restored.workspaces[0].id.as_deref(), Some("wproj"));
        assert_eq!(
            restored.workspaces[0].custom_name.as_deref(),
            Some("pi-mono")
        );
        assert_eq!(restored.workspaces[0].tabs.len(), 1);
        assert_eq!(restored.workspaces[0].tabs[0].panes.len(), 2);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/home/can/Projects/herdr")
        );
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&1].label.as_deref(),
            Some("website")
        );
        assert!(!restored.workspaces[0].tabs[0].panes[&1].seen);
        assert_eq!(restored.sidebar_width, Some(26));
        assert_eq!(restored.sidebar_section_split, Some(0.5));
    }

    #[test]
    fn legacy_pane_snapshot_defaults_to_seen() {
        let pane: PaneSnapshot = serde_json::from_str(r#"{"cwd":"/tmp"}"#).unwrap();
        assert!(pane.seen);
    }

    #[test]
    fn current_session_fixture_parses() {
        let snap = parse_snapshot(session_fixture("current-herdr")).unwrap();

        assert_eq!(snap.version, 3);
        assert_eq!(snap.workspaces.len(), 2);
        assert_eq!(snap.active, Some(0));
        assert_eq!(snap.selected, 0);
        assert_eq!(snap.sidebar_width, None);
        assert_eq!(snap.sidebar_section_split, None);
        assert_eq!(snap.workspaces[0].tabs.len(), 2);
        assert_eq!(
            snap.workspaces[1].identity_cwd,
            PathBuf::from("/home/test/projects/project-b")
        );
    }

    #[test]
    fn current_dev_session_fixture_parses_additive_fields() {
        let snap = parse_snapshot(session_fixture("current-herdr-dev")).unwrap();

        assert_eq!(snap.version, 3);
        assert_eq!(snap.workspaces.len(), 2);
        assert_eq!(snap.sidebar_section_split, Some(0.4));
        assert_eq!(snap.workspaces[0].active_tab, 1);
        assert_eq!(snap.workspaces[1].tabs[0].panes.len(), 2);
    }

    #[test]
    fn old_snapshot_defaults_sidebar_fields() {
        let json = serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "workspaces": [],
            "active": null,
            "selected": 0
        })
        .to_string();

        let restored = parse_snapshot(&json).unwrap();

        assert_eq!(restored.sidebar_width, None);
        assert_eq!(restored.sidebar_section_split, None);
    }

    #[test]
    fn old_pane_snapshot_with_embedded_history_is_ignored() {
        let json = serde_json::json!({
            "version": SNAPSHOT_VERSION,
            "workspaces": [{
                "id": "wtest",
                "identity_cwd": "/tmp",
                "tabs": [{
                    "layout": { "Pane": 0 },
                    "panes": {
                        "0": {
                            "cwd": "/tmp",
                            "history": {
                                "ansi": "legacy-secret",
                                "lines": 1
                            }
                        }
                    },
                    "zoomed": false,
                    "focused": 0,
                    "root_pane": 0
                }],
                "active_tab": 0
            }],
            "active": 0,
            "selected": 0
        })
        .to_string();

        let restored = parse_snapshot(&json).unwrap();

        let encoded = serde_json::to_string(&restored).unwrap();
        assert!(!encoded.contains("legacy-secret"));
        assert!(!encoded.contains("\"history\""));
    }

    #[test]
    fn legacy_workspace_snapshot_migrates_to_single_tab() {
        let snap = parse_snapshot(session_fixture("legacy-pre-tabs-v2")).unwrap();
        let ws = &snap.workspaces[0];

        assert_eq!(snap.version, 2);
        assert_eq!(snap.workspaces.len(), 1);
        assert_eq!(ws.custom_name.as_deref(), Some("legacy"));
        assert_eq!(ws.identity_cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(ws.active_tab, 0);
        assert_eq!(ws.tabs.len(), 1);
        assert_eq!(ws.tabs[0].focused, Some(1));
        assert_eq!(ws.tabs[0].root_pane, Some(0));
        assert_eq!(ws.tabs[0].panes[&0].cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(ws.tabs[0].panes[&1].cwd, PathBuf::from("/tmp/herdr"));
    }

    #[test]
    fn capture_contract_tracks_workspace_order_active_and_selected() {
        let mut state = state_with_workspaces(&["a", "b", "c"]);
        state.active = Some(1);
        state.selected = 2;

        state.move_workspace(1, 0);

        let snapshot = capture_from_state(&state);
        let ids: Vec<_> = state.workspaces.iter().map(|ws| ws.id.clone()).collect();
        let captured_ids: Vec<_> = snapshot
            .workspaces
            .iter()
            .map(|ws| ws.id.clone().unwrap())
            .collect();
        assert_eq!(captured_ids, ids);
        assert_eq!(snapshot.active, state.active);
        assert_eq!(snapshot.selected, state.selected);
    }

    #[test]
    fn capture_contract_tracks_workspace_and_tab_names_and_active_tab() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].set_custom_name("renamed-workspace".into());
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.workspaces[0].switch_tab(second_tab);
        state.workspaces[0].tabs[0].set_custom_name("main".into());

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.custom_name.as_deref(), Some("renamed-workspace"));
        assert_eq!(workspace.active_tab, second_tab);
        assert_eq!(workspace.tabs[0].custom_name.as_deref(), Some("main"));
        assert_eq!(workspace.tabs[1].custom_name.as_deref(), Some("logs"));
    }

    #[test]
    fn capture_contract_tracks_workspace_closure() {
        let mut state = state_with_workspaces(&["one", "two"]);
        state.selected = 1;
        state.active = Some(1);

        state.close_selected_workspace();

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.workspaces.len(), 1);
        assert_eq!(snapshot.workspaces[0].custom_name.as_deref(), Some("one"));
        assert_eq!(snapshot.active, Some(0));
        assert_eq!(snapshot.selected, 0);
    }

    #[test]
    fn capture_contract_tracks_sidebar_state() {
        let mut state = state_with_workspaces(&["one"]);
        state.sidebar_width = 31;
        state.sidebar_section_split = 0.4;
        state.collapsed_space_keys.insert("repo-key".into());

        let snapshot = capture_from_state(&state);
        assert_eq!(snapshot.sidebar_width, Some(31));
        assert_eq!(snapshot.sidebar_section_split, Some(0.4));
        assert_eq!(snapshot.collapsed_space_keys, state.collapsed_space_keys);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("collapsed_space_keys"));
    }

    #[test]
    fn repository_state_round_trips_names_preferred_base_and_order() {
        let mut state = state_with_workspaces(&["main", "notes"]);
        state.workspaces[0].cached_git_space = Some(crate::workspace::GitSpaceMetadata {
            key: "/repo/herdr/.git".into(),
            checkout_key: "/repo/herdr".into(),
            repo_name: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        state.reconcile_repositories();
        state.repositories[0].custom_name = Some("Herdr source".into());
        state.repositories[0].preferred_base = Some("origin/main".into());
        state.space_order.reverse();

        let snapshot = capture_from_state(&state);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let restored = parse_snapshot(&encoded).unwrap();

        assert_eq!(restored.version, SNAPSHOT_VERSION);
        assert_eq!(
            restored.repositories[0].custom_name.as_deref(),
            Some("Herdr source")
        );
        assert_eq!(
            restored.repositories[0].preferred_base.as_deref(),
            Some("origin/main")
        );
        assert_eq!(restored.space_order, state.space_order);
    }

    #[test]
    fn v3_parent_child_membership_migrates_to_peer_checkouts() {
        let mut state = state_with_workspaces(&["main", "feature"]);
        for (idx, linked) in [false, true].into_iter().enumerate() {
            state.workspaces[idx].worktree_space =
                Some(crate::workspace::WorktreeSpaceMembership {
                    key: "/repo/herdr/.git".into(),
                    label: "herdr".into(),
                    repo_root: "/repo/herdr".into(),
                    checkout_path: if linked {
                        "/worktrees/feature".into()
                    } else {
                        "/repo/herdr".into()
                    },
                    is_linked_worktree: linked,
                });
        }
        let mut snapshot = capture_from_state(&state);
        snapshot.version = 3;
        snapshot.repositories.clear();
        snapshot.space_order.clear();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let migrated = parse_snapshot(&encoded).unwrap();

        assert_eq!(migrated.repositories.len(), 1);
        assert_eq!(migrated.repositories[0].checkout_workspace_ids.len(), 2);
        assert_eq!(migrated.space_order.len(), 1);
    }

    #[test]
    fn checkout_capture_uses_immutable_provenance_not_a_mutable_workspace_cwd() {
        let mut state = state_with_workspaces(&["checkout"]);
        state.workspaces[0].identity_cwd = PathBuf::from("/mutable/pane-cwd");
        state.workspaces[0].checkout = Some(crate::repository::CheckoutProvenance {
            repository_id: "repo".into(),
            checkout_path: PathBuf::from("/immutable/checkout"),
            kind: crate::repository::CheckoutKind::Linked,
        });

        let snapshot = capture_from_state(&state);

        assert_eq!(
            snapshot.workspaces[0].identity_cwd,
            PathBuf::from("/immutable/checkout")
        );
        assert_eq!(
            snapshot.workspaces[0].checkout,
            state.workspaces[0].checkout
        );
    }

    #[test]
    fn capture_contract_tracks_worktree_space_membership() {
        let mut state = state_with_workspaces(&["main"]);
        state.workspaces[0].worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: PathBuf::from("/repo/herdr"),
            checkout_path: PathBuf::from("/repo/herdr/worktree-a"),
            is_linked_worktree: true,
        });

        let snapshot = capture_from_state(&state);

        assert_eq!(
            snapshot.workspaces[0].worktree_space,
            state.workspaces[0].worktree_space
        );
    }

    #[test]
    fn capture_contract_tracks_layout_focus_zoom_and_root_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].tabs[0].layout.focus_pane(second);
        state.toggle_zoom();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert!(matches!(tab.layout, LayoutSnapshot::Split { .. }));
        assert_eq!(tab.focused, None);
        assert_eq!(
            tab.focused_leaf,
            Some(LayoutLeafSnapshot::Pane(second.raw()))
        );
        assert_eq!(tab.root_pane, Some(root.raw()));
        assert!(tab.zoomed);
        assert_eq!(tab.panes.len(), 2);
    }

    #[test]
    fn capture_contract_tracks_focus_navigation() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        crate::ui::compute_view(&mut state, Rect::new(0, 0, 106, 20));

        state.navigate_pane(NavDirection::Right);

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(tab.focused, None);
        assert_eq!(
            tab.focused_leaf,
            Some(LayoutLeafSnapshot::Pane(second.raw()))
        );
        assert_ne!(tab.focused_leaf, Some(LayoutLeafSnapshot::Pane(root.raw())));
    }

    #[test]
    fn capture_contract_tracks_resize_ratio_changes() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        state.workspaces[0].test_split(Direction::Horizontal);
        state.workspaces[0].layout.focus_pane(root);
        crate::ui::compute_view(&mut state, Rect::new(0, 0, 106, 20));
        let before = capture_from_state(&state);

        state.resize_pane(NavDirection::Right);

        let after = capture_from_state(&state);
        let before_ratio = root_split_ratio(&before.workspaces[0].tabs[0]).unwrap();
        let after_ratio = root_split_ratio(&after.workspaces[0].tabs[0]).unwrap();
        assert_ne!(before_ratio, after_ratio);
    }

    #[test]
    fn capture_contract_tracks_tab_closure() {
        let mut state = state_with_workspaces(&["one"]);
        let second_tab = state.workspaces[0].test_add_tab(Some("logs"));
        state.switch_tab(second_tab);

        state.close_tab();

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(workspace.tabs.len(), 1);
        assert_eq!(workspace.active_tab, 0);
        assert!(workspace.tabs[0].custom_name.is_none());
    }

    #[test]
    fn capture_contract_tracks_pane_closure() {
        let mut state = state_with_workspaces(&["one"]);
        state.workspaces[0].test_split(Direction::Horizontal);

        state.close_pane();

        let snapshot = capture_from_state(&state);
        let tab = &snapshot.workspaces[0].tabs[0];
        assert_eq!(tab.panes.len(), 1);
        assert!(matches!(tab.layout, LayoutSnapshot::Pane(_)));
        assert!(!tab.zoomed);
    }

    #[test]
    fn capture_contract_tracks_public_id_counters() {
        let mut state = state_with_workspaces(&["one"]);
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let third = state.workspaces[0].test_split(Direction::Vertical);
        let second_tab = state.workspaces[0].test_add_tab(None);

        state.workspaces[0].close_pane(second);

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        assert_eq!(
            workspace.public_pane_numbers,
            HashMap::from([
                (
                    state.workspaces[0].tabs[0]
                        .root_pane
                        .expect("test tab has root pane")
                        .raw(),
                    1
                ),
                (third.raw(), 3),
                (
                    state.workspaces[0].tabs[second_tab]
                        .root_pane
                        .expect("test tab has root pane")
                        .raw(),
                    4
                ),
            ])
        );
        assert_eq!(workspace.next_public_pane_number, 5);
        assert_eq!(workspace.public_tab_numbers, vec![1, 2]);
        assert_eq!(workspace.next_public_tab_number, 3);
    }

    #[test]
    fn capture_contract_tracks_workspace_identity_and_pane_cwds() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        state.workspaces[0].identity_cwd = PathBuf::from("/tmp/pion");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        state.ensure_test_terminals();
        let root_terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&root_terminal_id).unwrap().cwd = PathBuf::from("/tmp/pion");
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        state.terminals.get_mut(&second_terminal_id).unwrap().cwd = PathBuf::from("/tmp/herdr");

        let snapshot = capture_from_state(&state);
        let workspace = &snapshot.workspaces[0];
        let tab = &workspace.tabs[0];
        assert_eq!(workspace.identity_cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&root.raw()].cwd, PathBuf::from("/tmp/pion"));
        assert_eq!(tab.panes[&second.raw()].cwd, PathBuf::from("/tmp/herdr"));
    }

    #[tokio::test]
    async fn capture_contract_tracks_pane_history_from_runtime() {
        let state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"alpha\r\nbeta\r\ngamma\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("alpha"));
        assert!(!encoded.contains("\"history\""));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let history = &history_snapshot.workspaces[0].tabs[0].panes[&root.raw()];

        assert!(history.ansi.contains("alpha"));
        assert!(history.ansi.contains("gamma"));
        assert!(history.lines >= 3);
    }

    #[tokio::test]
    async fn capture_contract_tracks_history_for_each_pane() {
        let mut state = state_with_workspaces(&["one"]);
        let first = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        let second = state.workspaces[0].test_split(Direction::Horizontal);
        let first_terminal_id = state.workspaces[0].tabs[0].panes[&first]
            .attached_terminal_id
            .clone();
        let second_terminal_id = state.workspaces[0].tabs[0].panes[&second]
            .attached_terminal_id
            .clone();
        let mut terminal_runtimes = TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(
            first_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"first-pane-history\r\n",
            ),
        );
        terminal_runtimes.insert(
            second_terminal_id,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                20,
                3,
                4096,
                b"second-pane-history\r\n",
            ),
        );

        let snapshot = capture_from_state_with_runtimes(&state, &terminal_runtimes);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(!encoded.contains("first-pane-history"));
        assert!(!encoded.contains("second-pane-history"));

        let history_snapshot = capture_history_from_state_with_runtimes(&state, &terminal_runtimes);
        let tab = &history_snapshot.workspaces[0].tabs[0];
        let first_history = &tab.panes[&first.raw()];
        let second_history = &tab.panes[&second.raw()];

        assert!(first_history.ansi.contains("first-pane-history"));
        assert!(second_history.ansi.contains("second-pane-history"));
    }

    #[test]
    fn capture_contract_tracks_hook_authority_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let session_path = test_session_path("pi-session.jsonl");
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        let terminal = state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(
            Some(crate::detect::Agent::Pi),
            crate::detect::AgentState::Idle,
        );
        terminal.set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
            source: "herdr:pi".into(),
            agent: "pi".into(),
            session_ref: crate::agent_resume::AgentSessionRef::path(session_path.clone()).unwrap(),
        });
        terminal.set_hook_authority_with_session_ref(
            "herdr:pi".into(),
            "pi".into(),
            crate::detect::AgentState::Working,
            None,
            crate::agent_resume::AgentSessionRef::path(session_path.clone()),
            Some(20),
        );

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("agent session should be captured");

        assert_eq!(agent_session.source, "herdr:pi");
        assert_eq!(agent_session.agent, "pi");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Path
        );
        assert_eq!(agent_session.value, session_path);
    }

    #[test]
    fn capture_contract_preserves_restored_agent_session() {
        let mut state = state_with_workspaces(&["one"]);
        let root = state.workspaces[0].tabs[0]
            .root_pane
            .expect("test tab has root pane");
        state.ensure_test_terminals();
        let terminal_id = state.workspaces[0].tabs[0].panes[&root]
            .attached_terminal_id
            .clone();
        state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_persisted_agent_session(crate::agent_resume::PersistedAgentSession {
                source: "herdr:opencode".into(),
                agent: "opencode".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("opencode-session").unwrap(),
            });

        let snapshot = capture_from_state(&state);
        let agent_session = snapshot.workspaces[0].tabs[0].panes[&root.raw()]
            .agent_session
            .as_ref()
            .expect("persisted agent session should be captured");

        assert_eq!(agent_session.source, "herdr:opencode");
        assert_eq!(agent_session.agent, "opencode");
        assert_eq!(
            agent_session.kind,
            crate::agent_resume::AgentSessionRefKind::Id
        );
        assert_eq!(agent_session.value, "opencode-session");
    }

    #[test]
    fn old_unversioned_snapshot_loads_as_version_0() {
        let json = r#"{"workspaces":[],"active":null,"selected":0}"#;
        let snap = parse_snapshot(json).unwrap();
        assert_eq!(snap.version, 0);
    }

    #[test]
    fn future_version_is_rejected() {
        let json = r#"{"version":999,"workspaces":[],"active":null,"selected":0}"#;
        assert!(parse_snapshot(json).is_err());
    }

    #[test]
    fn active_tab_default_is_zero() {
        let json = r#"{"custom_name":"test","identity_cwd":"/tmp","tabs":[]}"#;
        let ws: WorkspaceSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(ws.active_tab, 0);
    }

    #[test]
    fn restore_falls_back_to_home_when_cwd_missing() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            PaneSnapshot {
                cwd: PathBuf::from("/tmp/this-directory-does-not-exist-for-herdr-test"),
                seen: true,
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );
        panes.insert(
            1,
            PaneSnapshot {
                cwd: std::env::var("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("/tmp")),
                seen: true,
                label: None,
                agent_name: None,
                managed_agent_kind: None,
                agent_session: None,
                launch_argv: None,
            },
        );

        let snap = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            repositories: Vec::new(),
            space_order: Vec::new(),
            workspaces: vec![WorkspaceSnapshot {
                id: Some("test-ws".to_string()),
                custom_name: Some("fallback test".to_string()),
                identity_cwd: PathBuf::from("/tmp"),
                checkout: None,
                worktree_space: None,
                public_pane_numbers: HashMap::new(),
                next_public_pane_number: 0,
                public_tab_numbers: Vec::new(),
                next_public_tab_number: 0,
                tabs: vec![TabSnapshot {
                    custom_name: None,
                    layout: LayoutSnapshot::Split {
                        direction: DirectionSnapshot::Horizontal,
                        ratio: 0.5,
                        first: Box::new(LayoutSnapshot::Pane(0)),
                        second: Box::new(LayoutSnapshot::Pane(1)),
                    },
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
            collapsed_space_keys: std::collections::HashSet::new(),

            delegations: Vec::new(),
            collection_archive_times: Vec::new(),
        };

        let json = serde_json::to_string(&snap).unwrap();
        let restored = parse_snapshot(&json).unwrap();
        assert_eq!(restored.workspaces.len(), 1);
        assert_eq!(
            restored.workspaces[0].tabs[0].panes[&0].cwd,
            PathBuf::from("/tmp/this-directory-does-not-exist-for-herdr-test")
        );
    }
}
