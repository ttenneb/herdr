use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_COLLECTION_MUTATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn fail_next_collection_mutation_for_test() {
    FAIL_NEXT_COLLECTION_MUTATION.with(|fail| fail.set(true));
}

#[cfg(test)]
pub(crate) fn take_collection_mutation_failure_for_test() -> bool {
    FAIL_NEXT_COLLECTION_MUTATION.with(|fail| fail.replace(false))
}

#[cfg(not(test))]
pub(crate) fn take_collection_mutation_failure_for_test() -> bool {
    false
}

use ratatui::layout::Direction;
use tokio::sync::{mpsc, Notify};

use crate::events::AppEvent;
use crate::layout::{
    CollectionId, LayoutLeaf, Node, PaneCollection, PaneId, PanePlacement, TileLayout,
};
use crate::pane::{PaneLaunchEnv, PaneState};
use crate::terminal::{TerminalId, TerminalRuntime, TerminalRuntimeRegistry, TerminalState};

pub(crate) type DetachedPane = (PaneId, TerminalId);

pub(crate) struct MovedPane {
    pub pane_id: PaneId,
    pub pane_state: PaneState,
    /// Collection archive state retained during cross-tab/workspace moves.
    pub archived: bool,
}

pub struct NewPane {
    pub pane_id: PaneId,
    pub terminal: TerminalState,
    pub runtime: TerminalRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionMutationError {
    CollectionNotFound,
    CollectionNotEmpty,
    PaneNotFound,
    PaneAlreadyPlaced,
    // Constructed by the direct state-only collection primitive used in tests.
    #[cfg_attr(not(test), allow(dead_code))]
    PaneNotTiled,
    PaneNotMember,
    TargetNotTiled,
    LastLayoutLeaf,
    LayoutMutationFailed,
}

#[derive(Debug)]
pub enum CollectionCreateMemberError {
    Collection(CollectionMutationError),
    Spawn(std::io::Error),
}

impl std::fmt::Display for CollectionCreateMemberError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Collection(err) => write!(formatter, "{err:?}"),
            Self::Spawn(err) => err.fmt(formatter),
        }
    }
}

enum SplitCommand<'a> {
    Shell {
        command: &'a str,
        launch_env: &'a PaneLaunchEnv,
    },
    Argv {
        argv: &'a [String],
        launch_env: &'a PaneLaunchEnv,
    },
}

pub struct Tab {
    pub custom_name: Option<String>,
    pub number: usize,
    /// Pane identity source when the tab contains panes. A tab containing only
    /// persistent empty collections intentionally has no pane identity.
    pub root_pane: Option<PaneId>,
    pub layout: TileLayout,
    /// Pane viewport state — always present, testable without PTYs.
    pub panes: HashMap<PaneId, PaneState>,
    #[cfg(test)]
    pub runtimes: HashMap<PaneId, TerminalRuntime>,
    pub zoomed: bool,
    pub events: mpsc::Sender<AppEvent>,
    pub(crate) render_notify: Arc<Notify>,
    pub(crate) render_dirty: Arc<AtomicBool>,
}

impl Tab {
    pub fn new(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            events,
            render_notify,
            render_dirty,
            None,
        )
    }

    pub fn new_argv_command(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        argv: &[String],
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        Self::new_with_runtime(
            number,
            initial_cwd,
            rows,
            cols,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            events,
            render_notify,
            render_dirty,
            Some(argv),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_runtime(
        number: usize,
        initial_cwd: PathBuf,
        rows: u16,
        cols: u16,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
        argv: Option<&[String]>,
    ) -> std::io::Result<(Self, TerminalState, TerminalRuntime)> {
        let (layout, root_id) = TileLayout::new();
        let runtime = if let Some(argv) = argv {
            TerminalRuntime::spawn_argv_command(
                root_id,
                rows,
                cols,
                initial_cwd.clone(),
                argv,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                events.clone(),
                render_notify.clone(),
                render_dirty.clone(),
            )?
        } else {
            TerminalRuntime::spawn(
                root_id,
                rows,
                cols,
                initial_cwd.clone(),
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                launch_env,
                events.clone(),
                render_notify.clone(),
                render_dirty.clone(),
            )?
        };

        let terminal_id = TerminalId::alloc();
        let terminal = match argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), initial_cwd).with_launch_argv(argv.to_vec())
            }
            None => TerminalState::new(terminal_id.clone(), initial_cwd),
        };
        let mut panes = HashMap::new();
        panes.insert(root_id, PaneState::new(terminal_id));

        Ok((
            Self {
                custom_name: None,
                number,
                root_pane: Some(root_id),
                layout,
                panes,
                #[cfg(test)]
                runtimes: HashMap::new(),
                zoomed: false,
                events,
                render_notify,
                render_dirty,
            },
            terminal,
            runtime,
        ))
    }

    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub fn pane_placement(&self, pane_id: PaneId) -> Option<PanePlacement> {
        self.panes
            .contains_key(&pane_id)
            .then(|| self.layout.placement(pane_id))
            .flatten()
    }

    pub fn collection(&self, collection_id: CollectionId) -> Option<&PaneCollection> {
        self.layout.collection(collection_id)
    }

    pub fn create_collection_near(
        &mut self,
        target: LayoutLeaf,
        direction: Direction,
        ratio: f32,
        label: Option<String>,
    ) -> Result<CollectionId, CollectionMutationError> {
        let collection_id =
            CollectionId::alloc().map_err(|_| CollectionMutationError::LayoutMutationFailed)?;
        if !self
            .layout
            .insert_collection_near(target, collection_id, direction, ratio)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        let _ = self.layout.set_collection_label(collection_id, label);
        self.zoomed = false;
        Ok(collection_id)
    }

    pub fn remove_empty_collection(
        &mut self,
        collection_id: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        let collection = self
            .layout
            .collection(collection_id)
            .ok_or(CollectionMutationError::CollectionNotFound)?;
        if !collection.members().is_empty() {
            return Err(CollectionMutationError::CollectionNotEmpty);
        }
        if self.layout.leaf_count() <= 1 {
            return Err(CollectionMutationError::LastLayoutLeaf);
        }
        if take_collection_mutation_failure_for_test()
            || !self.layout.remove_collection(collection_id)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        self.zoomed = false;
        Ok(())
    }

    // Production collection moves use the transactional cross-placement path; this direct
    // primitive remains useful for state-only invariant tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn collect_tiled_pane(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        if !self.panes.contains_key(&pane_id) {
            return Err(CollectionMutationError::PaneNotFound);
        }
        if self.layout.collection(collection_id).is_none() {
            return Err(CollectionMutationError::CollectionNotFound);
        }
        if self.layout.placement(pane_id) != Some(PanePlacement::Tiled) {
            return Err(CollectionMutationError::PaneNotTiled);
        }
        if self.layout.leaf_count() <= 1 {
            return Err(CollectionMutationError::LastLayoutLeaf);
        }
        let mut layout = self.layout.clone();
        if !layout.remove_tiled_pane_for_collection(pane_id)
            || !layout.add_collection_member(collection_id, pane_id)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        self.layout = layout;
        self.zoomed = false;
        Ok(())
    }

    pub fn insert_moved_pane_into_collection(
        &mut self,
        collection_id: CollectionId,
        moved: MovedPane,
    ) -> Result<PaneId, (CollectionMutationError, MovedPane)> {
        if self.layout.collection(collection_id).is_none() {
            return Err((CollectionMutationError::CollectionNotFound, moved));
        }
        if self.panes.contains_key(&moved.pane_id) || self.layout.placement(moved.pane_id).is_some()
        {
            return Err((CollectionMutationError::PaneAlreadyPlaced, moved));
        }
        let pane_id = moved.pane_id;
        let archived = moved.archived;
        let mut layout = self.layout.clone();
        if !layout.add_collection_member(collection_id, pane_id)
            || !layout.set_member_archived(collection_id, pane_id, archived)
        {
            return Err((CollectionMutationError::LayoutMutationFailed, moved));
        }
        self.layout = layout;
        self.panes.insert(pane_id, moved.pane_state);
        self.root_pane.get_or_insert(pane_id);
        self.zoomed = false;
        Ok(pane_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_collection_member(
        &mut self,
        collection_id: CollectionId,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
    ) -> Result<NewPane, CollectionCreateMemberError> {
        let pane_id = PaneId::alloc();
        self.validate_collection_insert(collection_id, pane_id)
            .map_err(CollectionCreateMemberError::Collection)?;
        let actual_cwd =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let runtime = TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            actual_cwd.clone(),
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            self.events.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        )
        .map_err(CollectionCreateMemberError::Spawn)?;
        let terminal_id = TerminalId::alloc();
        let terminal = TerminalState::new(terminal_id.clone(), actual_cwd);
        let mut layout = self.layout.clone();
        if !layout.add_collection_member(collection_id, pane_id) {
            runtime.shutdown();
            return Err(CollectionCreateMemberError::Collection(
                CollectionMutationError::LayoutMutationFailed,
            ));
        }
        self.layout = layout;
        self.panes.insert(pane_id, PaneState::new(terminal_id));
        self.root_pane.get_or_insert(pane_id);
        self.zoomed = false;
        Ok(NewPane {
            pane_id,
            terminal,
            runtime,
        })
    }

    pub(crate) fn validate_collection_insert(
        &self,
        collection_id: CollectionId,
        pane_id: PaneId,
    ) -> Result<(), CollectionMutationError> {
        if self.layout.collection(collection_id).is_none() {
            return Err(CollectionMutationError::CollectionNotFound);
        }
        if self.panes.contains_key(&pane_id) || self.layout.placement(pane_id).is_some() {
            return Err(CollectionMutationError::PaneAlreadyPlaced);
        }
        let mut layout = self.layout.clone();
        if take_collection_mutation_failure_for_test()
            || !layout.add_collection_member(collection_id, pane_id)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn move_collection_member(
        &mut self,
        pane_id: PaneId,
        source: CollectionId,
        destination: CollectionId,
    ) -> Result<(), CollectionMutationError> {
        if source == destination {
            return self
                .layout
                .collection(source)
                .filter(|collection| collection.members().contains(&pane_id))
                .map(|_| ())
                .ok_or(CollectionMutationError::PaneNotMember);
        }
        if self.layout.collection(destination).is_none() {
            return Err(CollectionMutationError::CollectionNotFound);
        }
        if self.layout.placement(pane_id) != Some(PanePlacement::Collection(source)) {
            return Err(CollectionMutationError::PaneNotMember);
        }
        let was_archived = self
            .layout
            .collection(source)
            .is_some_and(|collection| collection.is_archived(pane_id));
        let mut layout = self.layout.clone();
        if !layout.remove_collection_member(source, pane_id)
            || !layout.add_collection_member(destination, pane_id)
            || !layout.set_member_archived(destination, pane_id, was_archived)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        self.layout = layout;
        Ok(())
    }

    pub fn promote_collection_member_near(
        &mut self,
        pane_id: PaneId,
        collection_id: CollectionId,
        target_pane_id: PaneId,
        direction: Direction,
        ratio: f32,
    ) -> Result<(), CollectionMutationError> {
        if self.layout.placement(pane_id) != Some(PanePlacement::Collection(collection_id)) {
            return Err(CollectionMutationError::PaneNotMember);
        }
        if self.layout.placement(target_pane_id) != Some(PanePlacement::Tiled) {
            return Err(CollectionMutationError::TargetNotTiled);
        }
        let mut layout = self.layout.clone();
        if !layout.remove_collection_member(collection_id, pane_id)
            || !layout.insert_pane_near(target_pane_id, pane_id, direction, ratio)
        {
            return Err(CollectionMutationError::LayoutMutationFailed);
        }
        self.layout = layout;
        self.zoomed = false;
        Ok(())
    }

    pub fn select_collection_member(
        &mut self,
        collection_id: CollectionId,
        pane_id: PaneId,
    ) -> Result<(), CollectionMutationError> {
        if !self.layout.select_collection_member(collection_id, pane_id) {
            return Err(CollectionMutationError::PaneNotMember);
        }
        Ok(())
    }

    pub fn set_collection_member_archived(
        &mut self,
        collection_id: CollectionId,
        pane_id: PaneId,
        archived: bool,
    ) -> Result<(), CollectionMutationError> {
        if !self
            .layout
            .set_member_archived(collection_id, pane_id, archived)
        {
            return Err(CollectionMutationError::PaneNotMember);
        }
        Ok(())
    }

    pub(crate) fn rollback_collection_member_restore(
        &mut self,
        collection_id: CollectionId,
        pane_id: PaneId,
        original_revision: u64,
    ) -> Result<(), CollectionMutationError> {
        if !self
            .layout
            .rollback_member_restore(collection_id, pane_id, original_revision)
        {
            return Err(CollectionMutationError::PaneNotMember);
        }
        Ok(())
    }

    pub fn is_auto_named(&self) -> bool {
        self.custom_name.is_none()
    }

    pub fn set_custom_name(&mut self, name: String) {
        self.custom_name = Some(name);
    }

    pub fn split_focused(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
    ) -> std::io::Result<NewPane> {
        self.split_focused_with_runtime(
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            None,
        )
    }

    pub fn split_focused_with_ratio(
        &mut self,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
    ) -> std::io::Result<NewPane> {
        self.split_focused_with_runtime(
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            shell_config,
            launch_env,
            None,
        )
    }

    pub fn split_focused_command(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        command: &str,
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<NewPane> {
        self.split_focused_with_runtime(
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            Some(SplitCommand::Shell {
                command,
                launch_env,
            }),
        )
    }

    pub fn split_focused_argv_command(
        &mut self,
        direction: Direction,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<NewPane> {
        self.split_focused_with_runtime(
            direction,
            None,
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            Some(SplitCommand::Argv { argv, launch_env }),
        )
    }

    pub fn split_focused_argv_command_with_ratio(
        &mut self,
        direction: Direction,
        ratio: f32,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        argv: &[String],
        launch_env: &PaneLaunchEnv,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
    ) -> std::io::Result<NewPane> {
        self.split_focused_with_runtime(
            direction,
            Some(ratio),
            rows,
            cols,
            cwd,
            scrollback_limit_bytes,
            host_terminal_theme,
            crate::pane::PaneShellConfig::new("", crate::config::ShellModeConfig::NonLogin),
            launch_env,
            Some(SplitCommand::Argv { argv, launch_env }),
        )
    }

    fn split_focused_with_runtime(
        &mut self,
        direction: Direction,
        ratio: Option<f32>,
        rows: u16,
        cols: u16,
        cwd: Option<PathBuf>,
        scrollback_limit_bytes: usize,
        host_terminal_theme: crate::terminal_theme::TerminalTheme,
        shell_config: crate::pane::PaneShellConfig<'_>,
        launch_env: &PaneLaunchEnv,
        command: Option<SplitCommand<'_>>,
    ) -> std::io::Result<NewPane> {
        let previous_focus = self.layout.focused_leaf();
        let new_id = match ratio {
            Some(ratio) => self.layout.split_focused_with_ratio(direction, ratio),
            None => self.layout.split_focused(direction),
        };
        let actual_cwd =
            cwd.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| "/".into()));
        let launch_argv = if let Some(SplitCommand::Argv { argv, .. }) = &command {
            Some((*argv).to_vec())
        } else {
            None
        };
        let runtime = match command {
            Some(SplitCommand::Shell {
                command,
                launch_env,
            }) => TerminalRuntime::spawn_shell_command(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                command,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
            Some(SplitCommand::Argv { argv, launch_env }) => TerminalRuntime::spawn_argv_command(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                argv,
                launch_env,
                crate::pane::AgentDetection::Enabled,
                scrollback_limit_bytes,
                host_terminal_theme,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
            None => TerminalRuntime::spawn(
                new_id,
                rows,
                cols,
                actual_cwd.clone(),
                scrollback_limit_bytes,
                host_terminal_theme,
                shell_config,
                launch_env,
                self.events.clone(),
                self.render_notify.clone(),
                self.render_dirty.clone(),
            ),
        };
        let runtime = match runtime {
            Ok(runtime) => runtime,
            Err(err) => {
                self.layout.close_focused();
                let _ = self.layout.focus_leaf(previous_focus);
                return Err(err);
            }
        };
        let terminal_id = TerminalId::alloc();
        let terminal = match launch_argv {
            Some(argv) => {
                TerminalState::new(terminal_id.clone(), actual_cwd).with_launch_argv(argv)
            }
            None => TerminalState::new(terminal_id.clone(), actual_cwd),
        };
        self.panes.insert(new_id, PaneState::new(terminal_id));
        self.zoomed = false;
        Ok(NewPane {
            pane_id: new_id,
            terminal,
            runtime,
        })
    }

    #[cfg(test)]
    pub fn close_focused(&mut self) -> Option<DetachedPane> {
        let pane_id = self.layout.focused();
        self.detach_pane(pane_id)
    }

    pub fn close_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub fn remove_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        self.detach_pane(pane_id)
    }

    pub(crate) fn from_existing_pane(
        number: usize,
        custom_name: Option<String>,
        moved: MovedPane,
        events: mpsc::Sender<AppEvent>,
        render_notify: Arc<Notify>,
        render_dirty: Arc<AtomicBool>,
    ) -> Self {
        let mut panes = HashMap::new();
        let pane_id = moved.pane_id;
        panes.insert(pane_id, moved.pane_state);
        Self {
            custom_name,
            number,
            root_pane: Some(pane_id),
            layout: TileLayout::from_saved(Node::Pane(pane_id), pane_id),
            panes,
            #[cfg(test)]
            runtimes: HashMap::new(),
            zoomed: false,
            events,
            render_notify,
            render_dirty,
        }
    }

    pub(crate) fn take_pane_for_move(&mut self, pane_id: PaneId) -> Option<MovedPane> {
        if !self.panes.contains_key(&pane_id) {
            return None;
        }
        let placement = self.layout.placement(pane_id)?;
        let archived = match placement {
            PanePlacement::Collection(collection_id) => self
                .layout
                .collection(collection_id)
                .is_some_and(|collection| collection.is_archived(pane_id)),
            PanePlacement::Tiled => false,
        };

        // The workspace removes an ordinary one-pane tab as a unit. Its
        // detached layout is not observed again, so no leaf mutation is needed.
        if self.pane_count() == 1 && self.layout.is_single_pane_leaf() {
            let pane_state = self.panes.remove(&pane_id)?;
            self.zoomed = false;
            return Some(MovedPane {
                pane_id,
                pane_state,
                archived,
            });
        }

        let next_root = self.promoted_root_if_needed(pane_id);
        match placement {
            PanePlacement::Tiled => {
                let previous_focus = self.layout.focused_leaf();
                let _ = self.layout.focus_leaf(LayoutLeaf::Pane(pane_id));
                if !self.layout.close_focused() {
                    return None;
                }
                if previous_focus != LayoutLeaf::Pane(pane_id) {
                    let _ = self.layout.focus_leaf(previous_focus);
                }
            }
            PanePlacement::Collection(collection_id) => {
                if !self.layout.remove_collection_member(collection_id, pane_id) {
                    return None;
                }
            }
        }
        if self.root_pane == Some(pane_id) {
            self.root_pane = next_root;
        }

        let pane_state = self.panes.remove(&pane_id)?;
        self.zoomed = false;
        Some(MovedPane {
            pane_id,
            pane_state,
            archived,
        })
    }

    pub(crate) fn insert_existing_pane(
        &mut self,
        target_pane_id: PaneId,
        moved: MovedPane,
        direction: Direction,
        ratio: f32,
    ) -> Result<PaneId, MovedPane> {
        let mut layout = self.layout.clone();
        if !layout.insert_pane_near(target_pane_id, moved.pane_id, direction, ratio) {
            return Err(moved);
        }
        let pane_id = moved.pane_id;
        self.layout = layout;
        self.panes.insert(pane_id, moved.pane_state);
        self.root_pane.get_or_insert(pane_id);
        self.zoomed = false;
        Ok(pane_id)
    }

    fn detach_pane(&mut self, pane_id: PaneId) -> Option<DetachedPane> {
        let placement = self.layout.placement(pane_id)?;
        if placement == PanePlacement::Tiled && self.layout.leaf_count() <= 1 {
            return None;
        }

        let next_root = self.promoted_root_if_needed(pane_id);

        match placement {
            PanePlacement::Tiled => {
                if self.layout.focused_leaf() == LayoutLeaf::Pane(pane_id) {
                    self.layout.close_focused();
                } else {
                    let previous_focus = self.layout.focused_leaf();
                    self.layout.focus_pane(pane_id);
                    self.layout.close_focused();
                    let _ = self.layout.focus_leaf(previous_focus);
                }
            }
            PanePlacement::Collection(collection_id) => {
                if !self.layout.remove_collection_member(collection_id, pane_id) {
                    return None;
                }
            }
        }

        let pane = self.panes.remove(&pane_id)?;
        let terminal_id = pane.attached_terminal_id;
        self.zoomed = false;
        if self.root_pane == Some(pane_id) {
            self.root_pane = next_root;
        }
        Some((pane_id, terminal_id))
    }

    fn promoted_root_if_needed(&self, closing: PaneId) -> Option<PaneId> {
        if self.root_pane != Some(closing) {
            return self.root_pane;
        }
        self.layout
            .placed_pane_ids()
            .into_iter()
            .find(|id| *id != closing)
    }

    pub fn terminal_id(&self, pane_id: PaneId) -> Option<&TerminalId> {
        self.panes
            .get(&pane_id)
            .map(|pane| &pane.attached_terminal_id)
    }

    pub fn cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminals: &HashMap<TerminalId, TerminalState>,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.cwd())
            .or_else(|| {
                terminals
                    .get(terminal_id)
                    .map(|terminal| terminal.cwd.clone())
            })
    }

    pub fn foreground_cwd_for_pane(
        &self,
        pane_id: PaneId,
        terminal_runtimes: &TerminalRuntimeRegistry,
    ) -> Option<PathBuf> {
        let terminal_id = self.terminal_id(pane_id)?;
        terminal_runtimes
            .get(terminal_id)
            .and_then(|rt| rt.foreground_cwd())
    }
}
