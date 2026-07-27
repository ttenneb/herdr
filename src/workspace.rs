use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
#[cfg(test)]
use crate::layout::PanePlacement;
use crate::layout::{CollectionId, LayoutLeaf, PaneId, TileLayout};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

mod aggregate;
mod git;
mod tab;

#[cfg(test)]
use self::git::git_ahead_behind;
use self::git::git_status_cache_key_for_space;
#[cfg(test)]
pub(crate) use self::tab::fail_next_collection_mutation_for_test;
pub(crate) use self::{
    git::git_status_snapshot_for_cwd_with_demand,
    tab::{take_collection_mutation_failure_for_test, MovedPane},
};
pub use self::{
    git::{
        derive_label_from_cwd, fallback_label_from_cwd, git_branch, git_space_metadata,
        git_status_cache_key, GitSpaceMetadata, GitStatusCacheEntry, GitStatusRefreshDemand,
    },
    tab::{CollectionMutationError, NewPane, Tab},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorktreeSpaceMembership {
    pub key: String,
    pub label: String,
    pub repo_root: PathBuf,
    pub checkout_path: PathBuf,
    pub is_linked_worktree: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatus {
    pub workspace_id: String,
    pub resolved_identity_cwd: PathBuf,
    pub status_cache_key: PathBuf,
    pub demand: GitStatusRefreshDemand,
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceGitStatusSnapshot {
    pub auto_label: String,
    pub branch: Option<String>,
    pub ahead_behind: Option<(usize, usize)>,
    pub space: Option<GitSpaceMetadata>,
}

pub(crate) fn discover_workspace_git_identity(
    cwd: &std::path::Path,
) -> (Option<GitSpaceMetadata>, String, PathBuf) {
    let space = git_space_metadata(cwd);
    let auto_label = space
        .as_ref()
        .map(|space| space.label.clone())
        .unwrap_or_else(|| fallback_label_from_cwd(cwd));
    let status_cache_key = space
        .as_ref()
        .map(git_status_cache_key_for_space)
        .unwrap_or_else(|| cwd.to_path_buf());
    (space, auto_label, status_cache_key)
}

impl WorkspaceGitStatusSnapshot {
    pub fn into_workspace_status(
        self,
        workspace_id: String,
        resolved_identity_cwd: PathBuf,
        status_cache_key: PathBuf,
        demand: GitStatusRefreshDemand,
    ) -> WorkspaceGitStatus {
        WorkspaceGitStatus {
            workspace_id,
            resolved_identity_cwd,
            status_cache_key,
            demand,
            auto_label: self.auto_label,
            branch: self.branch,
            ahead_behind: self.ahead_behind,
            space: self.space,
        }
    }
}

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);
const PUBLIC_ID_ALPHABET: &[u8; 32] = b"123456789ABCDEFGHJKMNPQRSTVWXYZ0";

pub(crate) fn generate_workspace_id() -> String {
    let counter = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    format!("w{}", encode_public_number(counter as usize))
}

pub(crate) fn encode_public_number(mut value: usize) -> String {
    if value == 0 {
        return "0".to_string();
    }

    let mut encoded = Vec::new();
    while value > 0 {
        let digit = (value - 1) % PUBLIC_ID_ALPHABET.len();
        encoded.push(PUBLIC_ID_ALPHABET[digit] as char);
        value = (value - 1) / PUBLIC_ID_ALPHABET.len();
    }
    encoded.iter().rev().collect()
}

pub(crate) fn decode_public_number(value: &str) -> Option<usize> {
    let mut decoded = 0usize;
    for ch in value.chars() {
        let digit = PUBLIC_ID_ALPHABET
            .iter()
            .position(|candidate| *candidate as char == ch)?;
        decoded = decoded
            .checked_mul(PUBLIC_ID_ALPHABET.len())?
            .checked_add(digit + 1)?;
    }
    Some(decoded)
}

pub(crate) fn public_workspace_number(id: &str) -> Option<usize> {
    id.strip_prefix('w').and_then(decode_public_number)
}

pub(crate) fn public_pane_id_for_number(workspace_id: &str, pane_number: usize) -> String {
    format!("{workspace_id}:p{}", encode_public_number(pane_number))
}

pub(crate) fn public_tab_id_for_number(workspace_id: &str, tab_number: usize) -> String {
    format!("{workspace_id}:t{}", encode_public_number(tab_number))
}

pub(crate) fn reserve_workspace_ids(workspaces: &[Workspace]) {
    let Some(next) = workspaces
        .iter()
        .filter_map(|workspace| public_workspace_number(&workspace.id))
        .max()
        .and_then(|max| u64::try_from(max.checked_add(1)?).ok())
    else {
        return;
    };

    let mut current = NEXT_WORKSPACE_ID.load(Ordering::Relaxed);
    while current < next {
        match NEXT_WORKSPACE_ID.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

/// A named workspace containing tabs.
pub struct Workspace {
    /// Stable public workspace identity, independent of display order.
    pub id: String,
    /// User-provided override. If set, auto-derived identity stops updating.
    pub custom_name: Option<String>,
    /// Fallback workspace identity source for tests, old snapshots, or missing runtimes.
    pub identity_cwd: PathBuf,
    /// CWD from which the cached automatic label and Git metadata were derived.
    pub(crate) cached_identity_cwd: PathBuf,
    /// Automatic workspace label cached outside the render path.
    pub(crate) cached_auto_label: String,
    /// Cache key for periodic Git status associated with `cached_identity_cwd`.
    pub(crate) cached_git_status_key: PathBuf,
    /// Cached current git branch for the workspace repo.
    pub(crate) cached_git_branch: Option<String>,
    /// Cached ahead/behind counts for the workspace repo's current branch upstream.
    pub(crate) cached_git_ahead_behind: Option<(usize, usize)>,
    /// Cached derived Git repo metadata for worktree actions and status display.
    pub(crate) cached_git_space: Option<GitSpaceMetadata>,
    /// Explicit Herdr-managed worktree grouping provenance.
    pub worktree_space: Option<WorktreeSpaceMembership>,
    pub(crate) metadata_tokens: crate::metadata_tokens::MetadataTokens,
    pub(crate) metadata_token_sequences: HashMap<String, u64>,
    /// Public pane numbers within this workspace. Closed pane numbers are not reused.
    pub public_pane_numbers: HashMap<PaneId, usize>,
    pub(crate) next_public_pane_number: usize,
    pub(crate) next_public_tab_number: usize,
    pub tabs: Vec<Tab>,
    pub active_tab: usize,
    #[cfg(test)]
    pub(crate) test_runtimes: HashMap<PaneId, TerminalRuntime>,
}

impl Deref for Workspace {
    type Target = Tab;

    fn deref(&self) -> &Self::Target {
        self.active_tab()
            .expect("workspace must always have at least one active tab")
    }
}

impl DerefMut for Workspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.active_tab_mut()
            .expect("workspace must always have at least one active tab")
    }
}

impl Workspace {
    fn adjust_active_tab_after_removal(&mut self, removed_idx: usize) {
        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if removed_idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
    }

    pub(crate) fn from_existing_pane(
        label: Option<String>,
        tab_label: Option<String>,
        identity_cwd: PathBuf,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> Self {
        let id = generate_workspace_id();
        let root_pane = moved.pane_id;
        let tab = Tab::from_existing_pane(1, tab_label, moved, events, render_notify, render_dirty);
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(root_pane, 1);
        let (cached_git_space, cached_auto_label, cached_git_status_key) =
            discover_workspace_git_identity(&identity_cwd);
        Self {
            id,
            custom_name: label,
            identity_cwd: identity_cwd.clone(),
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label,
            cached_git_status_key,
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            cached_git_space,
            worktree_space: None,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            #[cfg(test)]
            test_runtimes: HashMap::new(),
        }
    }

    // Test modules construct workspaces through the default constructor; production paths
    // use the env-aware variant so pane identity env is always explicit.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_extra_env(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            events,
            render_notify,
            render_dirty,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_extra_env(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            events,
            render_notify,
            render_dirty,
            None,
            extra_env,
        )
    }

    // Kept for tests that do not need launch-env customization.
    #[allow(dead_code)]
    pub fn new_argv_command(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_argv_command_with_extra_env(
            initial_cwd,
            rows,
            cols,
            argv,
            scrollback_limit_bytes,
            host_terminal_theme,
            events,
            render_notify,
            render_dirty,
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_argv_command_with_extra_env(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_tab(
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            events,
            render_notify,
            render_dirty,
            Some(argv),
            extra_env,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_tab(
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let id = generate_workspace_id();
        let launch_env = PaneLaunchEnv::from_extra(extra_env).with_identity(
            id.clone(),
            public_tab_id_for_number(&id, 1),
            public_pane_id_for_number(&id, 1),
        );
        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                1,
                initial_cwd.clone(),
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane.expect("new tab must have a root pane"), 1);
        let (cached_git_space, cached_auto_label, cached_git_status_key) =
            discover_workspace_git_identity(&initial_cwd);
        Ok((
            Self {
                id,
                custom_name: None,
                identity_cwd: initial_cwd.clone(),
                cached_identity_cwd: initial_cwd.clone(),
                cached_auto_label,
                cached_git_status_key,
                cached_git_branch: git_branch(&initial_cwd),
                cached_git_ahead_behind: None,
                cached_git_space,
                worktree_space: None,
                metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
                metadata_token_sequences: HashMap::new(),
                public_pane_numbers,
                next_public_pane_number: 2,
                next_public_tab_number: 2,
                tabs: vec![tab],
                active_tab: 0,
                #[cfg(test)]
                test_runtimes: HashMap::new(),
            },
            terminal,
            runtime,
        ))
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub fn active_tab_display_name(&self) -> Option<String> {
        self.tab_display_name(self.active_tab)
    }

    pub fn tab_display_name(&self, tab_idx: usize) -> Option<String> {
        let tab = self.tabs.get(tab_idx)?;
        Some(
            tab.custom_name
                .clone()
                .unwrap_or_else(|| (tab_idx + 1).to_string()),
        )
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.active_tab = idx;
            if let Some(tab) = self.tabs.get_mut(idx) {
                let visible_tiled: std::collections::HashSet<_> =
                    tab.layout.tiled_pane_ids().into_iter().collect();
                for (pane_id, pane) in &mut tab.panes {
                    // Grouped children require explicit foreground terminal entry.
                    if visible_tiled.contains(pane_id) {
                        pane.seen = true;
                    }
                }
            }
        }
    }

    pub fn create_tab(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            None,
            extra_env,
        )
    }

    pub fn create_tab_argv_command(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        self.create_tab_with_runtime(
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            Some(argv),
            extra_env,
        )
    }

    fn create_tab_with_runtime(
        &mut self,
        rows: u16,
        cols: u16,
        cwd: PathBuf,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        argv: Option<&[String]>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<(usize, TerminalState, TerminalRuntime)> {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_number = self.next_public_pane_number;
        let launch_env = self.launch_env_for_new_pane(number, pane_number, extra_env);
        let events = self
            .active_tab()
            .map(|tab| tab.events.clone())
            .expect("workspace must always have at least one tab");
        let render_notify = self
            .active_tab()
            .map(|tab| tab.render_notify.clone())
            .expect("workspace must always have at least one tab");
        let render_dirty = self
            .active_tab()
            .map(|tab| tab.render_dirty.clone())
            .expect("workspace must always have at least one tab");

        let (tab, terminal, runtime) = if let Some(argv) = argv {
            Tab::new_argv_command(
                number,
                cwd,
                rows,
                cols,
                argv,
                scrollback_limit_bytes,
                host_terminal_theme,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        } else {
            Tab::new(
                number,
                cwd,
                rows,
                cols,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                &launch_env,
                events,
                render_notify,
                render_dirty,
            )?
        };
        self.register_new_pane_with_number(
            tab.root_pane.expect("new tab must have a root pane"),
            pane_number,
        );
        self.tabs.push(tab);
        Ok((self.tabs.len() - 1, terminal, runtime))
    }

    pub fn close_tab(&mut self, idx: usize) -> bool {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return false;
        }
        let tab = self.tabs.remove(idx);
        for pane_id in tab.panes.keys() {
            self.unregister_pane(*pane_id);
        }
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if idx <= self.active_tab && self.active_tab > 0 {
            self.active_tab -= 1;
        }
        true
    }

    pub fn move_tab(&mut self, source_idx: usize, insert_idx: usize) -> bool {
        if source_idx >= self.tabs.len() || insert_idx > self.tabs.len() {
            return false;
        }

        let target_idx = if source_idx < insert_idx {
            insert_idx.saturating_sub(1)
        } else {
            insert_idx
        }
        .min(self.tabs.len().saturating_sub(1));

        if source_idx == target_idx {
            return false;
        }

        let active_tab_number = self.tabs.get(self.active_tab).map(|tab| tab.number);
        let tab = self.tabs.remove(source_idx);
        self.tabs.insert(target_idx, tab);
        self.active_tab = active_tab_number
            .and_then(|number| self.tabs.iter().position(|tab| tab.number == number))
            .unwrap_or(target_idx);
        true
    }

    #[cfg(test)]
    pub fn close_active_tab(&mut self) -> bool {
        self.close_tab(self.active_tab)
    }

    #[cfg(test)]
    pub fn split_focused(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
    ) -> std::io::Result<crate::workspace::tab::NewPane> {
        let pane_number = self.next_public_pane_number;
        let tab_number = self
            .active_tab()
            .map(|tab| tab.number)
            .expect("workspace must always have at least one tab");
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let new_pane = self
            .active_tab_mut()
            .expect("workspace must always have at least one tab")
            .split_focused(
                direction,
                rows,
                cols,
                cwd,
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                &launch_env,
            )?;
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Ok(new_pane)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_focused_command(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        command: &str,
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<crate::workspace::tab::NewPane> {
        let pane_number = self.next_public_pane_number;
        let tab_number = self
            .active_tab()
            .map(|tab| tab.number)
            .expect("workspace must always have at least one tab");
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let new_pane = self
            .active_tab_mut()
            .expect("workspace must always have at least one tab")
            .split_focused_command(
                direction,
                rows,
                cols,
                cwd,
                command,
                &launch_env,
                scrollback_limit_bytes,
                host_terminal_theme,
            )?;
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Ok(new_pane)
    }

    pub fn split_pane(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_with_ratio(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            extra_env,
            focus_new_pane,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            extra_env,
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn split_pane_argv_command_with_ratio(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        extra_env: Vec<(String, String)>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        focus_new_pane: bool,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        self.split_pane_with_runtime(
            pane_id,
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            extra_env,
            focus_new_pane,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_collection_member(
        &mut self,
        tab_idx: usize,
        collection_id: CollectionId,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
    ) -> Result<crate::workspace::tab::NewPane, crate::workspace::tab::CollectionCreateMemberError>
    {
        let pane_number = self.next_public_pane_number;
        let tab_number = self.tabs[tab_idx].number;
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let new_pane = self.tabs[tab_idx].create_collection_member(
            collection_id,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            &launch_env,
        )?;
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Ok(new_pane)
    }

    #[allow(clippy::too_many_arguments)]
    fn split_pane_with_runtime(
        &mut self,
        pane_id: PaneId,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        extra_env: Vec<(String, String)>,
        focus_new_pane: bool,
        argv: Option<&[String]>,
    ) -> Option<std::io::Result<(usize, crate::workspace::tab::NewPane)>> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let pane_number = self.next_public_pane_number;
        let tab_number = self.tabs[tab_idx].number;
        let launch_env = self.launch_env_for_new_pane(tab_number, pane_number, extra_env);
        let tab = &mut self.tabs[tab_idx];
        let previous_focus = tab.layout.focused_leaf();
        let previous_focused_pane = tab.layout.focused();
        tab.layout.focus_pane(pane_id);
        let new_pane = match if let Some(argv) = argv {
            match ratio {
                Some(ratio) => tab.split_focused_argv_command_with_ratio(
                    direction,
                    ratio,
                    rows,
                    cols,
                    cwd,
                    argv,
                    &launch_env,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                ),
                None => tab.split_focused_argv_command(
                    direction,
                    rows,
                    cols,
                    cwd,
                    argv,
                    &launch_env,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                ),
            }
        } else {
            match ratio {
                Some(ratio) => tab.split_focused_with_ratio(
                    direction,
                    ratio,
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    shell_config,
                    &launch_env,
                ),
                None => tab.split_focused(
                    direction,
                    rows,
                    cols,
                    cwd,
                    scrollback_limit_bytes,
                    host_terminal_theme,
                    shell_config,
                    &launch_env,
                ),
            }
        } {
            Ok(new_pane) => new_pane,
            Err(err) => {
                let _ = tab.layout.focus_leaf(previous_focus);
                if matches!(previous_focus, LayoutLeaf::Collection(_)) {
                    tab.layout.focus_pane(previous_focused_pane);
                }
                return Some(Err(err));
            }
        };
        if !focus_new_pane {
            let _ = tab.layout.focus_leaf(previous_focus);
            if matches!(previous_focus, LayoutLeaf::Collection(_)) {
                tab.layout.focus_pane(previous_focused_pane);
            }
        }
        self.register_new_pane_with_number(new_pane.pane_id, pane_number);
        Some(Ok((tab_idx, new_pane)))
    }

    /// Close the focused pane. Returns true if the workspace should close.
    #[cfg(test)]
    pub fn close_focused(&mut self) -> bool {
        let Some(tab) = self.active_tab() else {
            return true;
        };
        let pane_count = tab.pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 && tab.layout.is_single_pane_leaf() {
            return tab_count <= 1 || self.close_active_tab_and_report();
        }

        if let Some((removed, _terminal_id)) = self.active_tab_mut().and_then(Tab::close_focused) {
            self.unregister_pane(removed);
        }
        false
    }

    /// Remove a specific pane from this workspace without terminating its runtime.
    /// Returns true if the workspace should close.
    pub fn remove_pane(&mut self, pane_id: PaneId) -> bool {
        let Some(tab_idx) = self.find_tab_index_for_pane(pane_id) else {
            return false;
        };
        let pane_count = self.tabs[tab_idx].pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 && self.tabs[tab_idx].layout.is_single_pane_leaf() {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].remove_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<TakenPane> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        let previous_active_tab = self.active_tab;
        let source_layout = self.tabs[tab_idx].layout.clone();
        let source_root_pane = self.tabs[tab_idx].root_pane;
        let source_zoomed = self.tabs[tab_idx].zoomed;
        let pane_count = self.tabs[tab_idx].pane_count();
        if pane_count <= 1 && self.tabs[tab_idx].layout.is_single_pane_leaf() {
            let mut tab = self.tabs.remove(tab_idx);
            let moved = tab.take_pane_for_move(pane_id)?;
            self.adjust_active_tab_after_removal(tab_idx);
            return Some(TakenPane {
                moved,
                recovery: PaneMoveSourceRecovery {
                    tab_idx,
                    previous_active_tab,
                    source_layout,
                    source_root_pane,
                    source_zoomed,
                    removed_tab: Some(tab),
                },
                removed_tab_idx: Some(tab_idx),
                workspace_empty: self.tabs.is_empty(),
            });
        }

        let moved = self.tabs[tab_idx].take_pane_for_move(pane_id)?;
        Some(TakenPane {
            moved,
            recovery: PaneMoveSourceRecovery {
                tab_idx,
                previous_active_tab,
                source_layout,
                source_root_pane,
                source_zoomed,
                removed_tab: None,
            },
            removed_tab_idx: None,
            workspace_empty: false,
        })
    }

    pub(crate) fn restore_failed_pane_move(
        &mut self,
        mut recovery: PaneMoveSourceRecovery,
        moved: MovedPane,
    ) {
        let pane_id = moved.pane_id;
        if let Some(mut tab) = recovery.removed_tab.take() {
            tab.layout = recovery.source_layout;
            tab.root_pane = recovery.source_root_pane;
            tab.zoomed = recovery.source_zoomed;
            tab.panes.insert(pane_id, moved.pane_state);
            self.tabs.insert(recovery.tab_idx.min(self.tabs.len()), tab);
        } else if let Some(tab) = self.tabs.get_mut(recovery.tab_idx) {
            tab.layout = recovery.source_layout;
            tab.root_pane = recovery.source_root_pane;
            tab.zoomed = recovery.source_zoomed;
            tab.panes.insert(pane_id, moved.pane_state);
        }
        self.active_tab = recovery
            .previous_active_tab
            .min(self.tabs.len().saturating_sub(1));
    }

    pub fn create_collection_near(
        &mut self,
        tab_idx: usize,
        target: LayoutLeaf,
        direction: Direction,
        ratio: f32,
        label: Option<String>,
    ) -> Result<CollectionId, CollectionMutationError> {
        let tab = self
            .tabs
            .get_mut(tab_idx)
            .ok_or(CollectionMutationError::LayoutMutationFailed)?;
        tab.create_collection_near(target, direction, ratio, label)
    }

    pub(crate) fn cascade_close_collection(
        &mut self,
        collection_id: CollectionId,
    ) -> Result<CollectionCloseOutcome, CollectionMutationError> {
        let tab_idx = self
            .tabs
            .iter()
            .position(|tab| tab.collection(collection_id).is_some())
            .ok_or(CollectionMutationError::CollectionNotFound)?;
        let members = self.tabs[tab_idx]
            .collection(collection_id)
            .map(|collection| collection.members().to_vec())
            .ok_or(CollectionMutationError::CollectionNotFound)?;
        let detached = members
            .iter()
            .map(|pane_id| {
                self.tabs[tab_idx]
                    .terminal_id(*pane_id)
                    .cloned()
                    .map(|terminal_id| (*pane_id, terminal_id))
                    .ok_or(CollectionMutationError::PaneNotFound)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let remove_tab = self.tabs[tab_idx].layout.leaf_count() == 1;
        if crate::workspace::tab::take_collection_mutation_failure_for_test() {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        if remove_tab {
            self.tabs.remove(tab_idx);
            self.adjust_active_tab_after_removal(tab_idx);
        } else {
            let mut layout = self.tabs[tab_idx].layout.clone();
            for pane_id in &members {
                if !layout.remove_collection_member(collection_id, *pane_id) {
                    return Err(CollectionMutationError::LayoutMutationFailed);
                }
            }
            if !layout.remove_collection(collection_id) {
                return Err(CollectionMutationError::LayoutMutationFailed);
            }
            self.tabs[tab_idx].layout = layout;
            for pane_id in &members {
                self.tabs[tab_idx].panes.remove(pane_id);
            }
            if self.tabs[tab_idx]
                .root_pane
                .is_some_and(|root| members.contains(&root))
            {
                self.tabs[tab_idx].root_pane = self.tabs[tab_idx]
                    .layout
                    .placed_pane_ids()
                    .into_iter()
                    .next();
            }
            self.tabs[tab_idx].zoomed = false;
        }
        for pane_id in &members {
            self.unregister_pane(*pane_id);
        }
        Ok(CollectionCloseOutcome {
            detached,
            removed_tab_idx: remove_tab.then_some(tab_idx),
            workspace_empty: self.tabs.is_empty(),
        })
    }

    pub(crate) fn promote_all_collection_members(
        &mut self,
        collection_id: CollectionId,
        target_pane_id: Option<PaneId>,
        direction: Direction,
        ratio: f32,
    ) -> Result<CollectionPromotionOutcome, CollectionMutationError> {
        let source_tab_idx = self
            .tabs
            .iter()
            .position(|tab| tab.collection(collection_id).is_some())
            .ok_or(CollectionMutationError::CollectionNotFound)?;
        let members = self.tabs[source_tab_idx]
            .collection(collection_id)
            .map(|collection| collection.members().to_vec())
            .ok_or(CollectionMutationError::CollectionNotFound)?;

        if let Some(target_pane_id) = target_pane_id {
            if self.find_tab_index_for_pane(target_pane_id) != Some(source_tab_idx) {
                return Err(CollectionMutationError::TargetNotTiled);
            }
            let promoted = self.tabs[source_tab_idx].promote_all_collection_members_near(
                collection_id,
                target_pane_id,
                direction,
                ratio,
            )?;
            return Ok(CollectionPromotionOutcome {
                members: promoted,
                target_tab_idx: source_tab_idx,
                created_tab: false,
                removed_tab_idx: None,
            });
        }

        // A collection-only source has no legal split anchor. Deterministically
        // reuse the first other tab with a tiled pane, or create a normal tab.
        let reusable = self.tabs.iter().enumerate().find_map(|(idx, tab)| {
            (idx != source_tab_idx)
                .then(|| {
                    tab.layout
                        .tiled_pane_ids()
                        .into_iter()
                        .next()
                        .map(|pane| (idx, pane))
                })
                .flatten()
        });
        let pane_states = members
            .iter()
            .map(|pane_id| {
                self.tabs[source_tab_idx]
                    .panes
                    .get(pane_id)
                    .cloned()
                    .map(|state| (*pane_id, state))
                    .ok_or(CollectionMutationError::PaneNotFound)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let source_is_only_leaf = self.tabs[source_tab_idx].layout.leaf_count() == 1;
        let source_layout = if source_is_only_leaf {
            None
        } else {
            let mut layout = self.tabs[source_tab_idx].layout.clone();
            for pane_id in &members {
                if !layout.remove_collection_member(collection_id, *pane_id) {
                    return Err(CollectionMutationError::LayoutMutationFailed);
                }
            }
            if !layout.remove_collection(collection_id) {
                return Err(CollectionMutationError::LayoutMutationFailed);
            }
            Some(layout)
        };
        if crate::workspace::tab::take_collection_mutation_failure_for_test() {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }

        let (mut target_tab_idx, created_tab) =
            if let Some((target_tab_idx, target_pane)) = reusable {
                let mut layout = self.tabs[target_tab_idx].layout.clone();
                let mut insertion_target = target_pane;
                for pane_id in &members {
                    if !layout.insert_pane_near(insertion_target, *pane_id, direction, ratio) {
                        return Err(CollectionMutationError::LayoutMutationFailed);
                    }
                    insertion_target = *pane_id;
                }
                self.tabs[target_tab_idx].layout = layout;
                for (pane_id, pane_state) in &pane_states {
                    self.tabs[target_tab_idx]
                        .panes
                        .insert(*pane_id, pane_state.clone());
                }
                (target_tab_idx, false)
            } else {
                let Some((first_id, first_state)) = pane_states.first() else {
                    return Err(CollectionMutationError::CollectionNotEmpty);
                };
                let moved = MovedPane {
                    pane_id: *first_id,
                    pane_state: first_state.clone(),
                    archived: false,
                };
                let number = self.next_public_tab_number;
                self.next_public_tab_number += 1;
                let source_tab = &self.tabs[source_tab_idx];
                let mut tab = Tab::from_existing_pane(
                    number,
                    None,
                    moved,
                    source_tab.events.clone(),
                    source_tab.render_notify.clone(),
                    source_tab.render_dirty.clone(),
                );
                let mut insertion_target = *first_id;
                for (pane_id, pane_state) in pane_states.iter().skip(1) {
                    tab.insert_existing_pane(
                        insertion_target,
                        MovedPane {
                            pane_id: *pane_id,
                            pane_state: pane_state.clone(),
                            archived: false,
                        },
                        direction,
                        ratio,
                    )
                    .map_err(|_| CollectionMutationError::LayoutMutationFailed)?;
                    insertion_target = *pane_id;
                }
                self.tabs.push(tab);
                (self.tabs.len() - 1, true)
            };

        let removed_tab_idx = if source_is_only_leaf {
            self.tabs.remove(source_tab_idx);
            self.adjust_active_tab_after_removal(source_tab_idx);
            if source_tab_idx < target_tab_idx {
                target_tab_idx -= 1;
            }
            Some(source_tab_idx)
        } else {
            self.tabs[source_tab_idx].layout =
                source_layout.ok_or(CollectionMutationError::LayoutMutationFailed)?;
            for pane_id in &members {
                self.tabs[source_tab_idx].panes.remove(pane_id);
            }
            if self.tabs[source_tab_idx]
                .root_pane
                .is_some_and(|root| members.contains(&root))
            {
                self.tabs[source_tab_idx].root_pane = self.tabs[source_tab_idx]
                    .layout
                    .placed_pane_ids()
                    .into_iter()
                    .next();
            }
            self.tabs[source_tab_idx].zoomed = false;
            None
        };

        Ok(CollectionPromotionOutcome {
            members,
            target_tab_idx,
            created_tab,
            removed_tab_idx,
        })
    }

    pub fn remove_empty_collection(
        &mut self,
        collection_id: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        let tab = self
            .tabs
            .iter_mut()
            .find(|tab| tab.collection(collection_id).is_some())
            .ok_or(CollectionMutationError::CollectionNotFound)?;
        tab.remove_empty_collection(collection_id)
    }

    // Production collection moves use the transactional cross-placement path; this direct
    // primitive remains useful for state-only invariant tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn collect_pane(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        let tab_idx = self
            .find_tab_index_for_pane(pane_id)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        self.tabs[tab_idx].collect_tiled_pane(pane_id, collection_id)
    }

    #[cfg(test)]
    pub fn move_collection_member(
        &mut self,
        pane_id: PaneId,
        source: CollectionId,
        destination: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        let tab_idx = self
            .find_tab_index_for_pane(pane_id)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        self.tabs[tab_idx].move_collection_member(pane_id, source, destination)
    }

    pub fn promote_collection_member_near(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
        target_pane_id: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> Result<(), CollectionMutationError> {
        let tab_idx = self
            .find_tab_index_for_pane(pane_id)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        if self.find_tab_index_for_pane(target_pane_id) != Some(tab_idx) {
            return Err(CollectionMutationError::TargetNotTiled);
        }
        self.tabs[tab_idx].promote_collection_member_near(
            pane_id,
            collection_id,
            target_pane_id,
            direction,
            ratio,
        )
    }

    pub fn select_collection_member(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        let tab_idx = self
            .find_tab_index_for_pane(pane_id)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        self.tabs[tab_idx].select_collection_member(collection_id, pane_id)
    }

    pub fn set_collection_member_archived(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
        archived: bool,
    ) -> Result<(), CollectionMutationError> {
        let tab_idx = self
            .find_tab_index_for_pane(pane_id)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        self.tabs[tab_idx].set_collection_member_archived(collection_id, pane_id, archived)
    }

    /// Undo a just-started archived-member restore in its original tab without changing its
    /// destructive-confirmation revision.
    pub(crate) fn rollback_collection_member_restore(
        &mut self,
        tab_idx: usize,
        pane_id: PaneId,
        collection_id: CollectionId,
        original_revision: u64,
    ) -> Result<(), CollectionMutationError> {
        let tab = self
            .tabs
            .get_mut(tab_idx)
            .ok_or(CollectionMutationError::PaneNotFound)?;
        tab.rollback_collection_member_restore(collection_id, pane_id, original_revision)
    }

    pub(crate) fn insert_moved_pane_into_collection(
        &mut self,
        tab_idx: usize,
        collection_id: CollectionId,
        moved: MovedPane,
    ) -> Result<PaneId, (CollectionMutationError, MovedPane)> {
        let pane_id = moved.pane_id;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return Err((CollectionMutationError::LayoutMutationFailed, moved));
        };
        let inserted = tab.insert_moved_pane_into_collection(collection_id, moved)?;
        if !self.public_pane_numbers.contains_key(&pane_id) {
            self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
        }
        Ok(inserted)
    }

    pub(crate) fn insert_moved_pane_into_tab(
        &mut self,
        tab_idx: usize,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
    ) -> Result<PaneId, MovedPane> {
        let pane_id = moved.pane_id;
        let Some(tab) = self.tabs.get_mut(tab_idx) else {
            return Err(moved);
        };
        tab.insert_existing_pane(target_pane_id, moved, direction, ratio)?;
        if !self.public_pane_numbers.contains_key(&pane_id) {
            self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
        }
        Ok(pane_id)
    }

    pub(crate) fn create_tab_from_existing_pane(
        &mut self,
        moved: MovedPane,
        label: Option<String>,
        fallback_events: mpsc::Sender<AppEvent>,
        fallback_render_notify: Arc<Notify>,
        fallback_render_dirty: Arc<AtomicBool>,
    ) -> usize {
        let number = self.next_public_tab_number;
        self.next_public_tab_number += 1;
        let pane_id = moved.pane_id;
        let (events, render_notify, render_dirty) = self
            .active_tab()
            .map(|tab| {
                (
                    tab.events.clone(),
                    tab.render_notify.clone(),
                    tab.render_dirty.clone(),
                )
            })
            .unwrap_or((
                fallback_events,
                fallback_render_notify,
                fallback_render_dirty,
            ));
        let tab =
            Tab::from_existing_pane(number, label, moved, events, render_notify, render_dirty);
        if !self.public_pane_numbers.contains_key(&pane_id) {
            self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
        }
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    pub(crate) fn unregister_moved_pane(&mut self, pane_id: PaneId) {
        self.unregister_pane(pane_id);
    }

    pub fn public_pane_number(&self, pane_id: PaneId) -> Option<usize> {
        self.public_pane_numbers.get(&pane_id).copied()
    }

    fn launch_env_for_new_pane(
        &self,
        tab_number: usize,
        pane_number: usize,
        extra_env: Vec<(String, String)>,
    ) -> PaneLaunchEnv {
        PaneLaunchEnv::from_extra(extra_env).with_identity(
            self.id.clone(),
            public_tab_id_for_number(&self.id, tab_number),
            public_pane_id_for_number(&self.id, pane_number),
        )
    }

    pub fn public_tab_number(&self, tab_idx: usize) -> Option<usize> {
        self.tabs.get(tab_idx).map(|tab| tab.number)
    }

    #[cfg(test)]
    pub fn public_tab_number_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        let tab_idx = self.find_tab_index_for_pane(pane_id)?;
        self.public_tab_number(tab_idx)
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    #[cfg(test)]
    pub fn resolved_identity_cwd(&self) -> Option<PathBuf> {
        Some(self.identity_cwd.clone())
    }

    pub fn resolved_identity_cwd_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        self.tabs
            .first()
            .and_then(|tab| {
                tab.root_pane
                    .and_then(|pane_id| tab.cwd_for_pane(pane_id, terminals, terminal_runtimes))
            })
            .or_else(|| Some(self.identity_cwd.clone()))
    }

    #[cfg(test)]
    pub fn display_name(&self) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.automatic_display_name_for_cwd(&self.identity_cwd)
    }

    pub(crate) fn display_name_from_terminals(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
    ) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        let cwd = self
            .tabs
            .first()
            .and_then(|tab| tab.root_pane.and_then(|pane_id| tab.terminal_id(pane_id)))
            .and_then(|terminal_id| terminals.get(terminal_id))
            .map(|terminal| &terminal.cwd)
            .unwrap_or(&self.identity_cwd);
        self.automatic_display_name_for_cwd(cwd)
    }

    pub fn display_name_from(
        &self,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> String {
        if let Some(name) = &self.custom_name {
            return name.clone();
        }

        self.resolved_identity_cwd_from(terminals, terminal_runtimes)
            .map(|cwd| self.automatic_display_name_for_cwd(&cwd))
            .unwrap_or_else(|| "workspace".into())
    }

    fn automatic_display_name_for_cwd(&self, cwd: &std::path::Path) -> String {
        if cwd == self.cached_identity_cwd {
            self.cached_auto_label.clone()
        } else {
            fallback_label_from_cwd(cwd)
        }
    }

    pub fn branch(&self) -> Option<String> {
        self.cached_git_branch.clone()
    }

    pub fn git_ahead_behind(&self) -> Option<(usize, usize)> {
        self.cached_git_ahead_behind
    }

    pub fn git_space(&self) -> Option<&GitSpaceMetadata> {
        self.cached_git_space.as_ref()
    }

    pub fn worktree_space(&self) -> Option<&WorktreeSpaceMembership> {
        self.worktree_space.as_ref()
    }

    #[cfg(test)]
    pub fn refresh_git_ahead_behind(&mut self) {
        let cwd = self.resolved_identity_cwd();
        self.cached_git_branch = cwd.as_deref().and_then(git_branch);
        self.cached_git_ahead_behind = cwd.as_deref().and_then(git_ahead_behind);
        self.cached_git_space = cwd.as_deref().and_then(git_space_metadata);
    }

    pub fn find_tab_index_for_pane(&self, pane_id: PaneId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.contains_key(&pane_id))
    }

    pub fn pane_state(&self, pane_id: PaneId) -> Option<&PaneState> {
        self.tabs.iter().find_map(|tab| tab.panes.get(&pane_id))
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.tabs.iter().find_map(|tab| tab.terminal_id(pane_id))
    }

    pub fn focused_pane_id(&self) -> Option<PaneId> {
        let tab = self.active_tab()?;
        match tab.layout.focused_leaf() {
            LayoutLeaf::Pane(pane_id) => Some(pane_id),
            LayoutLeaf::Collection(collection_id) => tab
                .collection(collection_id)
                .and_then(|collection| collection.selected()),
        }
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> bool {
        let tab_idx = match self.find_tab_index_for_pane(pane_id) {
            Some(idx) => idx,
            None => return false,
        };
        let pane_count = self.tabs[tab_idx].pane_count();
        let tab_count = self.tabs.len();
        if pane_count <= 1 && self.tabs[tab_idx].layout.is_single_pane_leaf() {
            if tab_count <= 1 {
                return true;
            }
            self.tabs.remove(tab_idx);
            self.unregister_pane(pane_id);
            if self.active_tab >= self.tabs.len() {
                self.active_tab = self.tabs.len() - 1;
            } else if tab_idx <= self.active_tab && self.active_tab > 0 {
                self.active_tab -= 1;
            }
            return false;
        }

        if let Some((removed, _terminal_id)) = self.tabs[tab_idx].close_pane(pane_id) {
            self.unregister_pane(removed);
        }
        false
    }

    #[cfg(test)]
    fn register_new_pane(&mut self, pane_id: PaneId) {
        self.register_new_pane_with_number(pane_id, self.next_public_pane_number);
    }

    fn register_new_pane_with_number(&mut self, pane_id: PaneId, number: usize) {
        self.public_pane_numbers.insert(pane_id, number);
        self.next_public_pane_number = self.next_public_pane_number.max(number + 1);
    }

    fn unregister_pane(&mut self, pane_id: PaneId) {
        self.public_pane_numbers.remove(&pane_id);
    }

    #[cfg(test)]
    fn close_active_tab_and_report(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return true;
        }
        self.close_active_tab();
        false
    }
}

pub(crate) struct CollectionPromotionOutcome {
    pub members: Vec<PaneId>,
    pub target_tab_idx: usize,
    pub created_tab: bool,
    pub removed_tab_idx: Option<usize>,
}

pub(crate) struct CollectionCloseOutcome {
    pub detached: Vec<(PaneId, crate::terminal::TerminalId)>,
    pub removed_tab_idx: Option<usize>,
    pub workspace_empty: bool,
}

pub(crate) struct PaneMoveSourceRecovery {
    tab_idx: usize,
    previous_active_tab: usize,
    source_layout: TileLayout,
    source_root_pane: Option<PaneId>,
    source_zoomed: bool,
    removed_tab: Option<Tab>,
}

pub(crate) struct TakenPane {
    pub moved: MovedPane,
    pub recovery: PaneMoveSourceRecovery,
    pub removed_tab_idx: Option<usize>,
    pub workspace_empty: bool,
}

#[cfg(test)]
impl Workspace {
    pub(crate) fn test_new(name: &str) -> Self {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let identity_cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let (layout, root_id) = TileLayout::new();
        let terminal_id = TerminalId::alloc();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));
        let tab = Tab {
            custom_name: None,
            number: 1,
            root_pane: Some(root_id),
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        let mut public_pane_numbers = HashMap::new();
        public_pane_numbers.insert(tab.root_pane.expect("new tab must have a root pane"), 1);
        Self {
            id: generate_workspace_id(),
            custom_name: Some(name.to_string()),
            identity_cwd: identity_cwd.clone(),
            cached_identity_cwd: identity_cwd.clone(),
            cached_auto_label: fallback_label_from_cwd(&identity_cwd),
            cached_git_status_key: identity_cwd.clone(),
            cached_git_branch: git_branch(&identity_cwd),
            cached_git_ahead_behind: None,
            cached_git_space: None,
            worktree_space: None,
            metadata_tokens: crate::metadata_tokens::MetadataTokens::default(),
            metadata_token_sequences: HashMap::new(),
            public_pane_numbers,
            next_public_pane_number: 2,
            next_public_tab_number: 2,
            tabs: vec![tab],
            active_tab: 0,
            test_runtimes: HashMap::new(),
        }
    }

    pub(crate) fn insert_test_runtime(&mut self, pane_id: PaneId, runtime: TerminalRuntime) {
        self.test_runtimes.insert(pane_id, runtime);
    }

    pub(crate) fn test_split(&mut self, direction: Direction) -> PaneId {
        let tab = self.active_tab_mut().expect("workspace must have tab");
        let new_id = tab.layout.split_focused(direction);
        tab.panes
            .insert(new_id, PaneState::new(TerminalId::alloc()));
        self.register_new_pane(new_id);
        new_id
    }

    pub(crate) fn test_add_tab(&mut self, name: Option<&str>) -> usize {
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));
        let (layout, root_id) = TileLayout::new();
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(TerminalId::alloc()));
        let tab = Tab {
            custom_name: name.map(str::to_string),
            number: self.next_public_tab_number,
            root_pane: Some(root_id),
            layout,
            panes,
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        };
        self.next_public_tab_number += 1;
        self.register_new_pane(root_id);
        self.tabs.push(tab);
        self.tabs.len() - 1
    }

    pub(crate) fn test_adversarial_identity_state() -> Self {
        let mut ws = Self::test_new("adversarial-identity");
        let removed_pane = ws.test_split(Direction::Horizontal);
        ws.test_split(Direction::Vertical);
        assert!(!ws.close_pane(removed_pane));
        let _unused_raw_id = PaneId::alloc();
        let later_pane = ws.test_split(Direction::Horizontal);

        let removed_tab = ws.test_add_tab(Some("removed"));
        let survivor_tab = ws.test_add_tab(None);
        let final_tab = ws.test_add_tab(None);
        let survivor_root = ws.tabs[survivor_tab]
            .root_pane
            .expect("test tab has root pane");
        let final_root = ws.tabs[final_tab]
            .root_pane
            .expect("test tab has root pane");
        assert!(ws.close_tab(removed_tab));
        assert!(ws.move_tab(0, ws.tabs.len()));
        ws.switch_tab(
            ws.find_tab_index_for_pane(survivor_root)
                .expect("survivor tab should still exist"),
        );

        assert_ne!(
            ws.active_tab + 1,
            ws.tabs[ws.active_tab].number,
            "adversarial active tab must distinguish position from public tab number"
        );
        assert_ne!(
            later_pane.raw() as usize,
            ws.public_pane_number(later_pane).unwrap(),
            "adversarial pane must distinguish raw pane id from public pane number"
        );
        assert_eq!(ws.find_tab_index_for_pane(final_root), Some(1));
        ws
    }

    pub(crate) fn assert_invariants_for_test(&self) {
        assert!(
            !self.tabs.is_empty(),
            "workspace {} must contain at least one tab",
            self.id
        );
        assert!(
            self.active_tab < self.tabs.len(),
            "workspace {} active_tab {} out of bounds for {} tabs",
            self.id,
            self.active_tab,
            self.tabs.len()
        );

        let mut tab_numbers = std::collections::HashSet::new();
        let mut max_tab_number = 0usize;
        let mut live_panes = std::collections::HashSet::new();
        let mut terminal_ids = std::collections::HashSet::new();

        for (tab_idx, tab) in self.tabs.iter().enumerate() {
            assert!(
                tab.number > 0,
                "workspace {} tab {} has invalid public tab number 0",
                self.id,
                tab_idx
            );
            assert!(
                tab_numbers.insert(tab.number),
                "workspace {} has duplicate public tab number {}",
                self.id,
                tab.number
            );
            max_tab_number = max_tab_number.max(tab.number);
            match tab.root_pane {
                Some(root_pane) => assert!(
                    tab.panes.contains_key(&root_pane),
                    "workspace {} tab {} root pane {:?} is missing from tab panes",
                    self.id,
                    tab_idx,
                    root_pane
                ),
                None => assert!(
                    tab.panes.is_empty(),
                    "workspace {} tab {} without a root pane must contain no panes",
                    self.id,
                    tab_idx
                ),
            }
            assert_eq!(
                tab.root_pane.is_some(),
                !tab.panes.is_empty(),
                "workspace {} tab {} root pane presence must match pane presence",
                self.id,
                tab_idx
            );

            let leaves = tab.layout.leaves();
            let leaf_set: std::collections::HashSet<_> = leaves.iter().copied().collect();
            assert_eq!(
                leaves.len(),
                leaf_set.len(),
                "workspace {} tab {} layout contains duplicate leaves",
                self.id,
                tab_idx
            );
            assert!(
                leaf_set.contains(&tab.layout.focused_leaf()),
                "workspace {} tab {} focused leaf {:?} is not in layout",
                self.id,
                tab_idx,
                tab.layout.focused_leaf()
            );

            let tiled_panes: std::collections::HashSet<_> =
                tab.layout.tiled_pane_ids().into_iter().collect();
            let mut placed_panes = tiled_panes.clone();
            let collection_leaf_ids: std::collections::HashSet<_> = leaves
                .iter()
                .filter_map(|leaf| match leaf {
                    LayoutLeaf::Collection(id) => Some(*id),
                    LayoutLeaf::Pane(_) => None,
                })
                .collect();
            let collection_ids: std::collections::HashSet<_> = tab
                .layout
                .collections()
                .map(|collection| collection.id)
                .collect();
            assert_eq!(
                collection_leaf_ids, collection_ids,
                "workspace {} tab {} collection records must exactly match collection leaves",
                self.id, tab_idx
            );
            for collection in tab.layout.collections() {
                let member_set: std::collections::HashSet<_> =
                    collection.members().iter().copied().collect();
                assert_eq!(
                    member_set.len(),
                    collection.members().len(),
                    "workspace {} tab {} collection {:?} has duplicate members",
                    self.id,
                    tab_idx,
                    collection.id
                );
                for member in collection.members() {
                    assert!(
                        placed_panes.insert(*member),
                        "workspace {} tab {} pane {:?} has more than one placement",
                        self.id,
                        tab_idx,
                        member
                    );
                    assert_eq!(
                        tab.layout.placement(*member),
                        Some(PanePlacement::Collection(collection.id))
                    );
                }
                if let Some(selected) = collection.selected() {
                    assert!(member_set.contains(&selected));
                } else {
                    assert!(collection.members().is_empty());
                }
                for archived in collection.archived_members() {
                    assert!(member_set.contains(&archived));
                }
            }
            let pane_set: std::collections::HashSet<_> = tab.panes.keys().copied().collect();
            assert_eq!(
                placed_panes, pane_set,
                "workspace {} tab {} every pane must have exactly one placement",
                self.id, tab_idx
            );

            for (pane_id, pane) in &tab.panes {
                assert!(
                    live_panes.insert(*pane_id),
                    "workspace {} pane {:?} appears in more than one tab",
                    self.id,
                    pane_id
                );
                assert!(
                    self.public_pane_numbers.contains_key(pane_id),
                    "workspace {} live pane {:?} has no public pane number",
                    self.id,
                    pane_id
                );
                assert!(
                    terminal_ids.insert(pane.attached_terminal_id.clone()),
                    "workspace {} terminal {} is attached to multiple panes",
                    self.id,
                    pane.attached_terminal_id
                );
            }
        }

        assert!(
            self.next_public_tab_number > 0,
            "workspace {} next_public_tab_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_tab_number > max_tab_number,
            "workspace {} next_public_tab_number {} must be greater than max live public tab number {}",
            self.id,
            self.next_public_tab_number,
            max_tab_number
        );

        let public_pane_keys: std::collections::HashSet<_> =
            self.public_pane_numbers.keys().copied().collect();
        assert_eq!(
            public_pane_keys, live_panes,
            "workspace {} public pane map must exactly match live panes",
            self.id
        );

        let mut pane_numbers = std::collections::HashSet::new();
        let mut max_pane_number = 0usize;
        for (pane_id, pane_number) in &self.public_pane_numbers {
            assert!(
                *pane_number > 0,
                "workspace {} pane {:?} has invalid public pane number 0",
                self.id,
                pane_id
            );
            assert!(
                pane_numbers.insert(*pane_number),
                "workspace {} duplicate public pane number {} for pane {:?}",
                self.id,
                pane_number,
                pane_id
            );
            max_pane_number = max_pane_number.max(*pane_number);
        }
        assert!(
            self.next_public_pane_number > 0,
            "workspace {} next_public_pane_number must be greater than 0",
            self.id
        );
        assert!(
            self.next_public_pane_number > max_pane_number,
            "workspace {} next_public_pane_number {} must be greater than max live public pane number {}",
            self.id,
            self.next_public_pane_number,
            max_pane_number
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_workspace_ids_are_short_base32_handles() {
        let first = generate_workspace_id();
        let second = generate_workspace_id();

        assert!(first.starts_with('w'));
        assert!(second.starts_with('w'));
        assert_ne!(first, second);
        assert_eq!(
            public_workspace_number(&first).map(encode_public_number),
            first.strip_prefix('w').map(str::to_owned)
        );
        assert_eq!(
            public_workspace_number(&second).map(encode_public_number),
            second.strip_prefix('w').map(str::to_owned)
        );

        // Handle length is determined by the counter value, not test execution order.
        assert_eq!(format!("w{}", encode_public_number(1)).len(), 2);
        assert_eq!(format!("w{}", encode_public_number(1024)).len(), 3);
    }

    #[test]
    fn public_numbers_round_trip_readable_base32_handles() {
        assert_eq!(encode_public_number(1), "1");
        assert_eq!(encode_public_number(9), "9");
        assert_eq!(encode_public_number(10), "A");
        assert_eq!(encode_public_number(31), "Z");
        assert_eq!(encode_public_number(32), "0");
        assert_eq!(encode_public_number(33), "11");

        for value in [1, 9, 10, 31, 32, 33, 1024, 1025] {
            let encoded = encode_public_number(value);
            assert_eq!(decode_public_number(&encoded), Some(value));
        }
    }

    #[test]
    fn reserving_restored_workspace_ids_prevents_reuse() {
        let mut restored = Workspace::test_new("restored");
        restored.id = "wZ".to_string();

        reserve_workspace_ids(&[restored]);

        let generated = generate_workspace_id();
        assert_ne!(generated, "wZ");
        assert!(public_workspace_number(&generated) > public_workspace_number("wZ"));
    }

    #[test]
    fn pane_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let second = ws.test_split(Direction::Horizontal);
        let third = ws.test_split(Direction::Vertical);

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), Some(2));
        assert_eq!(ws.public_pane_number(third), Some(3));

        assert!(!ws.close_pane(second));

        assert_eq!(ws.public_pane_number(root), Some(1));
        assert_eq!(ws.public_pane_number(second), None);
        assert_eq!(ws.public_pane_number(third), Some(3));

        let fourth = ws.test_split(Direction::Horizontal);
        assert_eq!(ws.public_pane_number(fourth), Some(4));
    }

    #[test]
    fn tab_public_numbers_are_stable_and_not_reused_after_close() {
        let mut ws = Workspace::test_new("test");
        let first_root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let second_tab = ws.test_add_tab(None);
        let second_root = ws.tabs[second_tab]
            .root_pane
            .expect("test tab has root pane");
        let third_tab = ws.test_add_tab(None);
        let third_root = ws.tabs[third_tab]
            .root_pane
            .expect("test tab has root pane");

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(second_root), Some(2));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        assert!(ws.close_tab(second_tab));

        assert_eq!(ws.public_tab_number_for_pane(first_root), Some(1));
        assert_eq!(ws.public_tab_number_for_pane(third_root), Some(3));

        let fourth_tab = ws.test_add_tab(None);
        let fourth_root = ws.tabs[fourth_tab]
            .root_pane
            .expect("test tab has root pane");
        assert_eq!(ws.public_tab_number_for_pane(fourth_root), Some(4));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn adversarial_identity_state_satisfies_workspace_invariants_after_mutation() {
        let mut ws = Workspace::test_adversarial_identity_state();
        ws.assert_invariants_for_test();

        let active_public = ws.tabs[ws.active_tab].number;
        assert_ne!(ws.active_tab + 1, active_public);
        let divergent_pane = ws
            .public_pane_numbers
            .iter()
            .find_map(|(pane_id, public_number)| {
                (pane_id.raw() as usize != *public_number).then_some(*pane_id)
            })
            .expect("adversarial state should contain raw/public pane divergence");
        assert_ne!(
            divergent_pane.raw() as usize,
            ws.public_pane_number(divergent_pane).unwrap()
        );

        let new_pane = ws.test_split(Direction::Vertical);
        assert!(ws.public_pane_number(new_pane).is_some());
        assert!(ws.move_tab(ws.active_tab, ws.tabs.len()));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn failed_moved_pane_insert_returns_pane_for_recovery() {
        let mut source = Workspace::test_new("source");
        let source_pane = source.tabs[0].root_pane.expect("test tab has root pane");
        let taken = source
            .take_pane_for_move(source_pane)
            .expect("source pane should be movable");
        let mut target = Workspace::test_new("target");
        let missing_target = PaneId::alloc();

        let recovered = target
            .insert_moved_pane_into_tab(0, missing_target, taken.moved, Direction::Horizontal, 0.5)
            .expect_err("invalid target should return the moved pane");

        assert_eq!(recovered.pane_id, source_pane);
        assert!(!target.tabs[0].panes.contains_key(&source_pane));
    }

    #[test]
    fn empty_collection_is_a_focusable_persistent_leaf_without_a_pane() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let collection = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                Direction::Horizontal,
                0.5,
                Some("helpers".into()),
            )
            .expect("create collection");

        assert_eq!(ws.tabs[0].pane_count(), 1);
        assert_eq!(ws.tabs[0].layout.pane_ids(), vec![root]);
        assert!(ws.tabs[0]
            .layout
            .focus_leaf(LayoutLeaf::Collection(collection)));
        assert_eq!(
            ws.tabs[0].layout.focused_leaf(),
            LayoutLeaf::Collection(collection)
        );
        assert_eq!(ws.tabs[0].collection(collection).unwrap().selected(), None);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn collection_members_have_exactly_one_placement_and_preserve_pane_identity() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let child = ws.test_split(Direction::Horizontal);
        let child_terminal = ws.tabs[0].terminal_id(child).unwrap().clone();
        let child_public_number = ws.public_pane_number(child);
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("create collection");

        ws.collect_pane(child, collection).expect("collect child");
        ws.set_collection_member_archived(child, collection, true)
            .expect("archive child");

        assert_eq!(
            ws.tabs[0].pane_placement(child),
            Some(PanePlacement::Collection(collection))
        );
        assert!(!ws.tabs[0].layout.tiled_pane_ids().contains(&child));
        assert!(ws.tabs[0].layout.pane_ids().contains(&child));
        assert_eq!(
            ws.tabs[0].collection(collection).unwrap().members(),
            &[child]
        );
        assert_eq!(
            ws.tabs[0].collection(collection).unwrap().selected(),
            Some(child)
        );
        assert!(ws.tabs[0]
            .collection(collection)
            .unwrap()
            .is_archived(child));
        assert_eq!(ws.tabs[0].terminal_id(child), Some(&child_terminal));
        assert_eq!(ws.public_pane_number(child), child_public_number);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn collection_mutations_validate_before_changing_state() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let child = ws.test_split(Direction::Horizontal);
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("create collection");
        ws.collect_pane(child, collection).expect("collect child");
        let members_before = ws.tabs[0]
            .collection(collection)
            .unwrap()
            .members()
            .to_vec();
        let missing = CollectionId::from_raw(8_000_001).expect("valid missing id");

        assert_eq!(
            ws.move_collection_member(child, collection, missing),
            Err(CollectionMutationError::CollectionNotFound)
        );
        assert_eq!(
            ws.tabs[0].collection(collection).unwrap().members(),
            members_before
        );
        assert_eq!(
            ws.tabs[0].pane_placement(child),
            Some(PanePlacement::Collection(collection))
        );
        ws.assert_invariants_for_test();
    }

    #[test]
    fn archive_updates_are_idempotent_and_move_preserves_archive_state() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let child = ws.test_split(Direction::Horizontal);
        let first = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("first collection");
        let second = ws
            .create_collection_near(
                0,
                LayoutLeaf::Collection(first),
                Direction::Horizontal,
                0.5,
                None,
            )
            .expect("second collection");
        ws.collect_pane(child, first).expect("collect child");

        ws.set_collection_member_archived(child, first, true)
            .expect("first archive");
        ws.set_collection_member_archived(child, first, true)
            .expect("idempotent archive");
        ws.move_collection_member(child, first, second)
            .expect("move archived child");

        assert!(ws.tabs[0].collection(second).unwrap().is_archived(child));
        ws.set_collection_member_archived(child, second, false)
            .expect("first unarchive");
        ws.set_collection_member_archived(child, second, false)
            .expect("idempotent unarchive");
        assert!(!ws.tabs[0].collection(second).unwrap().is_archived(child));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn closing_sole_pane_preserves_empty_collection_tab_without_stale_root() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Horizontal, 0.5, None)
            .expect("create collection");
        ws.test_add_tab(Some("other"));

        assert!(!ws.close_focused());

        assert_eq!(ws.tabs.len(), 2);
        assert!(ws.tabs[0].collection(collection).is_some());
        assert!(ws.tabs[0].panes.is_empty());
        assert_eq!(ws.tabs[0].root_pane, None);
        assert_eq!(ws.public_pane_number(root), None);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn removing_sole_pane_preserves_empty_collection_tab() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Horizontal, 0.5, None)
            .expect("create collection");
        ws.test_add_tab(Some("other"));

        assert!(!ws.remove_pane(root));

        assert_eq!(ws.tabs.len(), 2);
        assert!(ws.tabs[0].collection(collection).is_some());
        assert!(ws.tabs[0].panes.is_empty());
        assert_eq!(ws.tabs[0].root_pane, None);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn closing_sole_grouped_member_leaves_valid_empty_collection_tab() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Horizontal, 0.5, None)
            .expect("create collection");
        ws.collect_pane(root, collection)
            .expect("collect root pane");

        assert!(!ws.close_pane(root));

        assert_eq!(ws.tabs.len(), 1);
        assert!(ws.tabs[0].collection(collection).is_some());
        assert!(ws.tabs[0]
            .collection(collection)
            .unwrap()
            .members()
            .is_empty());
        assert!(ws.tabs[0].panes.is_empty());
        assert_eq!(ws.tabs[0].root_pane, None);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn moving_sole_pane_preserves_empty_collection_tab() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Horizontal, 0.5, None)
            .expect("create collection");

        let taken = ws.take_pane_for_move(root).expect("sole pane is movable");
        // A move keeps its public number while it may still be reinserted in this
        // workspace. Finalizing a move out unregisters the detached pane.
        ws.unregister_moved_pane(root);

        assert_eq!(taken.moved.pane_id, root);
        assert_eq!(taken.removed_tab_idx, None);
        assert!(!taken.workspace_empty);
        assert_eq!(ws.tabs.len(), 1);
        assert!(ws.tabs[0].collection(collection).is_some());
        assert!(ws.tabs[0].panes.is_empty());
        assert_eq!(ws.tabs[0].root_pane, None);
        ws.assert_invariants_for_test();
    }

    #[test]
    fn cross_tab_collection_move_preserves_archive_state() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let member = ws.test_split(Direction::Horizontal);
        let source = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("source collection");
        ws.collect_pane(member, source).expect("collect member");
        ws.set_collection_member_archived(member, source, true)
            .expect("archive member");
        let destination_tab = ws.test_add_tab(Some("destination"));
        let destination_root = ws.tabs[destination_tab]
            .root_pane
            .expect("destination has pane");
        let destination = ws
            .create_collection_near(
                destination_tab,
                LayoutLeaf::Pane(destination_root),
                Direction::Horizontal,
                0.5,
                None,
            )
            .expect("destination collection");

        let taken = ws.take_pane_for_move(member).expect("member is movable");
        assert!(ws
            .insert_moved_pane_into_collection(destination_tab, destination, taken.moved)
            .is_ok());

        assert!(ws.tabs[destination_tab]
            .collection(destination)
            .unwrap()
            .is_archived(member));
        ws.assert_invariants_for_test();
    }

    #[test]
    fn cross_workspace_collection_move_preserves_archive_state() {
        let mut source_ws = Workspace::test_new("source");
        let source_root = source_ws.tabs[0].root_pane.expect("source has pane");
        let member = source_ws.test_split(Direction::Horizontal);
        let source_collection = source_ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(source_root),
                Direction::Vertical,
                0.5,
                None,
            )
            .expect("source collection");
        source_ws
            .collect_pane(member, source_collection)
            .expect("collect member");
        source_ws
            .set_collection_member_archived(member, source_collection, true)
            .expect("archive member");

        let mut destination_ws = Workspace::test_new("destination");
        let destination_root = destination_ws.tabs[0]
            .root_pane
            .expect("destination has pane");
        let destination_collection = destination_ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(destination_root),
                Direction::Horizontal,
                0.5,
                None,
            )
            .expect("destination collection");

        let taken = source_ws
            .take_pane_for_move(member)
            .expect("member is movable");
        assert!(destination_ws
            .insert_moved_pane_into_collection(0, destination_collection, taken.moved)
            .is_ok());
        source_ws.unregister_moved_pane(member);

        assert!(destination_ws.tabs[0]
            .collection(destination_collection)
            .unwrap()
            .is_archived(member));
        source_ws.assert_invariants_for_test();
        destination_ws.assert_invariants_for_test();
    }

    #[test]
    fn split_failure_restores_collection_leaf_focus_and_selected_member() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let selected_child = ws.test_split(Direction::Horizontal);
        let targeted_child = ws.test_split(Direction::Vertical);
        let collection = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("create collection");
        ws.collect_pane(selected_child, collection)
            .expect("collect selected child");
        ws.collect_pane(targeted_child, collection)
            .expect("collect targeted child");
        ws.tabs[0].layout.focus_pane(selected_child);
        let before = ws.tabs[0].layout.focused_leaf();

        let result = ws
            .split_pane_argv_command(
                targeted_child,
                Direction::Horizontal,
                24,
                80,
                Some(PathBuf::from("/definitely/missing/herdr-cwd")),
                &["definitely-missing-herdr-command".into()],
                Vec::new(),
                1024,
                crate::terminal_theme::TerminalTheme::default(),
                false,
            )
            .expect("pane target exists");

        assert!(result.is_err());
        assert_eq!(ws.tabs[0].layout.focused_leaf(), before);
        assert_eq!(ws.tabs[0].layout.focused(), selected_child);
        assert_eq!(
            ws.tabs[0].collection(collection).unwrap().selected(),
            Some(selected_child)
        );
        ws.assert_invariants_for_test();
    }

    #[test]
    fn member_can_move_between_collections_and_promote_without_identity_change() {
        let mut ws = Workspace::test_new("collections");
        let root = ws.tabs[0].root_pane.expect("test tab has root pane");
        let child = ws.test_split(Direction::Horizontal);
        let terminal = ws.tabs[0].terminal_id(child).unwrap().clone();
        let first = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("first collection");
        let second = ws
            .create_collection_near(
                0,
                LayoutLeaf::Collection(first),
                Direction::Horizontal,
                0.5,
                None,
            )
            .expect("second collection");

        ws.collect_pane(child, first).expect("collect child");
        ws.move_collection_member(child, first, second)
            .expect("move child");
        ws.promote_collection_member_near(child, second, root, Direction::Horizontal, 0.4)
            .expect("promote child");

        assert_eq!(ws.tabs[0].pane_placement(child), Some(PanePlacement::Tiled));
        assert_eq!(ws.tabs[0].terminal_id(child), Some(&terminal));
        assert!(ws.tabs[0].collection(first).unwrap().members().is_empty());
        assert!(ws.tabs[0].collection(second).unwrap().members().is_empty());
        ws.assert_invariants_for_test();
    }

    #[tokio::test]
    async fn new_workspace_retains_discovered_git_metadata() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "herdr-workspace-git-metadata-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join(".git")).expect("create git directory");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write git head");
        #[cfg(windows)]
        let command = "C:\\Windows\\System32\\whoami.exe";
        #[cfg(not(windows))]
        let command = "/usr/bin/true";
        let argv = vec![command.to_string()];
        let (events, _) = mpsc::channel(64);
        let render_notify = Arc::new(Notify::new());
        let render_dirty = Arc::new(AtomicBool::new(false));

        let (workspace, _terminal, runtime) = Workspace::new_argv_command(
            root.clone(),
            24,
            80,
            &argv,
            1024,
            crate::terminal_theme::TerminalTheme::default(),
            events,
            render_notify,
            render_dirty,
        )
        .expect("create workspace");

        let space = workspace
            .git_space()
            .expect("workspace should retain discovered git metadata");
        assert_eq!(space.repo_root, root);

        runtime.shutdown();
        std::fs::remove_dir_all(root).expect("remove test repo");
    }

    #[test]
    fn display_name_reads_cached_identity_without_rechecking_filesystem() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "herdr-workspace-label-cache-{}-{stamp}",
            std::process::id()
        ));
        let cwd = root.join("deep/nested");
        std::fs::create_dir_all(&cwd).expect("create nested cwd");

        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        ws.cached_identity_cwd = cwd;
        ws.cached_auto_label = "cached-repo".into();

        std::fs::remove_dir_all(root).expect("remove cwd after cache admission");

        assert_eq!(ws.display_name(), "cached-repo");
    }

    #[test]
    fn terminal_aware_display_name_uses_latest_admitted_identity_cache() {
        let mut ws = Workspace::test_new("ignored");
        let root_pane = ws.tabs[0].root_pane.expect("test tab has root pane");
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        ws.custom_name = None;
        ws.identity_cwd = PathBuf::from("/old/workspace");
        ws.cached_identity_cwd = PathBuf::from("/new/repo/deep");
        ws.cached_auto_label = "repo".into();
        let terminals = HashMap::from([(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/new/repo/deep")),
        )]);

        assert_eq!(ws.display_name_from_terminals(&terminals), "repo");
        assert_eq!(ws.display_name(), "workspace");
    }

    #[test]
    fn workspace_identity_follows_first_tab_root_pane_cwd() {
        let mut ws = Workspace::test_new("ignored");
        ws.custom_name = None;
        let root_pane = ws.tabs[0].root_pane.expect("test tab has root pane");
        let terminal_id = ws.tabs[0].terminal_id(root_pane).unwrap().clone();
        let mut terminals = HashMap::new();
        terminals.insert(
            terminal_id.clone(),
            TerminalState::new(terminal_id, PathBuf::from("/herdr-test/pion")),
        );
        let terminal_runtimes = TerminalRuntimeRegistry::new();

        assert_eq!(ws.display_name_from(&terminals, &terminal_runtimes), "pion");
        assert_eq!(
            ws.resolved_identity_cwd_from(&terminals, &terminal_runtimes),
            Some(PathBuf::from("/herdr-test/pion"))
        );
    }

    #[test]
    fn moving_tab_keeps_active_identity_and_stable_tab_numbers() {
        let mut ws = Workspace::test_new("test");
        let moved_root = ws.tabs[0].root_pane.expect("test tab has root pane");
        ws.test_add_tab(Some("foo"));
        let final_auto_idx = ws.test_add_tab(None);
        let active_root = ws.tabs[final_auto_idx]
            .root_pane
            .expect("test tab has root pane");
        ws.switch_tab(final_auto_idx);

        assert!(ws.move_tab(0, ws.tabs.len()));

        let labels: Vec<_> = (0..ws.tabs.len())
            .map(|tab_idx| ws.tab_display_name(tab_idx).unwrap())
            .collect();
        assert_eq!(labels, vec!["foo", "2", "3"]);
        assert_eq!(ws.tabs[0].custom_name.as_deref(), Some("foo"));
        assert!(ws.tabs[1].custom_name.is_none());
        assert!(ws.tabs[2].custom_name.is_none());
        assert_eq!(ws.tabs[0].number, 2);
        assert_eq!(ws.tabs[1].number, 3);
        assert_eq!(ws.tabs[2].number, 1);
        assert_eq!(
            ws.tabs[2].root_pane.expect("test tab has root pane"),
            moved_root
        );
        assert_eq!(
            ws.tabs[ws.active_tab]
                .root_pane
                .expect("test tab has root pane"),
            active_root
        );
        ws.assert_invariants_for_test();
    }
}
