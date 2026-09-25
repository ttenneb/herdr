//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/herdr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.
//! Installed plugins are persisted separately at `plugins.json`.

mod io;
pub mod plugin_registry;
mod restore;
mod snapshot;

pub use self::io::{clear_history, load, load_history};
pub(crate) use self::io::{clear_ordered, save_ordered, save_snapshot_ordered, SessionWriter};
#[cfg(test)]
pub(crate) use self::io::{inject_durable_failure, DurableStep};
pub use self::restore::restore;
#[cfg(unix)]
pub use self::restore::{handoff_pane_aliases, restore_handoff};
pub use self::snapshot::{
    capture, capture_history, DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot,
    SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};
