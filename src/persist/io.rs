use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing::warn;

/// An exclusive lease for one physical session directory. Background and
/// synchronous writers share this file description and serialize within it;
/// another server cannot overwrite a route-ready snapshot while it is held.
pub(crate) struct SessionWriter {
    _lock: std::fs::File,
    order: Mutex<()>,
    #[cfg(unix)]
    physical: PathBuf,
    #[cfg(unix)]
    directory_identity: (u64, u64),
    #[cfg(unix)]
    lock_identity: (u64, u64),
}

impl SessionWriter {
    #[cfg(unix)]
    pub(crate) fn acquire(path: &Path) -> std::io::Result<Arc<Self>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let target = resolve_write_target(path)?;
        let parent = target
            .parent()
            .ok_or_else(|| std::io::Error::other("session parent missing"))?;
        ensure_parent_durable(parent)?;
        let physical = std::fs::canonicalize(parent)?;
        // The directory entry itself must survive a crash, not just the
        // session.json rename within it.
        std::fs::File::open(
            physical
                .parent()
                .ok_or_else(|| std::io::Error::other("session directory ancestor missing"))?,
        )?
        .sync_all()?;
        let meta = std::fs::metadata(&physical)?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.permissions().mode() & 0o022 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session directory is not exclusively owned",
            ));
        }
        let lock_path = physical.join(".session-writer.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&lock_path)?;
        let lock_meta = lock.metadata()?;
        if !lock_meta.is_file()
            || lock_meta.uid() != unsafe { libc::geteuid() }
            || lock_meta.mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session writer lock is not private",
            ));
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            _lock: lock,
            order: Mutex::new(()),
            physical,
            directory_identity: (meta.dev(), meta.ino()),
            lock_identity: (lock_meta.dev(), lock_meta.ino()),
        }))
    }

    #[cfg(unix)]
    pub(crate) fn validate(&self, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let target = resolve_write_target(path)?;
        let parent = std::fs::canonicalize(
            target
                .parent()
                .ok_or_else(|| std::io::Error::other("session parent missing"))?,
        )?;
        let dir = std::fs::metadata(&parent)?;
        let lock = std::fs::symlink_metadata(self.physical.join(".session-writer.lock"))?;
        let own = self._lock.metadata()?;
        if parent != self.physical
            || (dir.dev(), dir.ino()) != self.directory_identity
            || (lock.dev(), lock.ino()) != self.lock_identity
            || (own.dev(), own.ino()) != self.lock_identity
            || !lock.is_file()
        {
            return Err(std::io::Error::other(
                "session directory writer lease replaced",
            ));
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub(crate) fn validate(&self, _path: &Path) -> std::io::Result<()> {
        Err(std::io::Error::other("session writer unsupported"))
    }

    #[cfg(not(unix))]
    pub(crate) fn acquire(_path: &Path) -> std::io::Result<Arc<Self>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "durable session writer is unavailable",
        ))
    }
}

fn ensure_parent_durable(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("session directory parent missing"))?;
    ensure_parent_durable(parent)?;
    std::fs::create_dir(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

static NEXT_SESSION_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurableStep {
    Write,
    FileSync,
    Rename,
    DirectorySync,
}
#[cfg(test)]
thread_local! { static DURABLE_FAIL: std::cell::Cell<Option<DurableStep>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
pub(crate) fn inject_durable_failure(step: Option<DurableStep>) {
    DURABLE_FAIL.with(|fail| fail.set(step));
}

#[cfg(test)]
fn maybe_fail(step: DurableStep) -> std::io::Result<()> {
    if DURABLE_FAIL.with(|fail| fail.get()) == Some(step) {
        return Err(std::io::Error::other(format!("injected {step:?} failure")));
    }
    Ok(())
}

use super::snapshot::{
    parse_history_snapshot, parse_snapshot, snapshot_file_version, SessionHistorySnapshot,
    SessionSnapshot, SNAPSHOT_VERSION,
};

fn session_path() -> PathBuf {
    crate::session::data_dir().join("session.json")
}

fn session_history_path() -> PathBuf {
    crate::session::data_dir().join("session-history.json")
}

// Follow symlinks manually so a write through a (possibly dangling) symlink
// lands on the target. `fs::canonicalize` requires the target to exist, which
// excludes the dangling-symlink case stow users hit on the very first save.
fn resolve_write_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..16 {
        let meta = match std::fs::symlink_metadata(&current) {
            Ok(meta) => meta,
            Err(_) => return Ok(current),
        };
        if !meta.file_type().is_symlink() {
            return Ok(current);
        }
        let link = std::fs::read_link(&current)?;
        current = if link.is_absolute() {
            link
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(link)
        };
    }
    Ok(current)
}

pub(super) fn save_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    save_session_durable_to_path(path, snapshot)
}

fn save_session_durable_to_path(path: &Path, snapshot: &SessionSnapshot) -> std::io::Result<()> {
    use std::io::Write;
    let target = resolve_write_target(path)?;
    let parent = target
        .parent()
        .ok_or_else(|| std::io::Error::other("session parent missing"))?;
    ensure_parent_durable(parent)?;
    let json = serde_json::to_vec_pretty(snapshot)?;
    let nonce = NEXT_SESSION_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".session-{}-{nonce}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&tmp)?;
    let write_result = (|| -> std::io::Result<()> {
        #[cfg(test)]
        maybe_fail(DurableStep::Write)?;
        file.write_all(&json)?;
        #[cfg(test)]
        maybe_fail(DurableStep::FileSync)?;
        file.sync_all()?;
        #[cfg(test)]
        maybe_fail(DurableStep::Rename)?;
        std::fs::rename(&tmp, &target)?;
        #[cfg(test)]
        maybe_fail(DurableStep::DirectorySync)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result
}

pub(crate) fn save_snapshot_ordered(
    path: &Path,
    snapshot: &SessionSnapshot,
    writer: &Arc<SessionWriter>,
) -> std::io::Result<()> {
    let _order = writer
        .order
        .lock()
        .map_err(|_| std::io::Error::other("session writer poisoned"))?;
    writer.validate(path)?;
    save_session_durable_to_path(path, snapshot)
}

pub(crate) fn save_ordered(
    path: &Path,
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
    writer: Option<&Arc<SessionWriter>>,
) -> std::io::Result<()> {
    let acquired;
    let writer = match writer {
        Some(writer) => writer,
        None => {
            acquired = SessionWriter::acquire(path)?;
            &acquired
        }
    };
    let _order = writer
        .order
        .lock()
        .map_err(|_| std::io::Error::other("session writer poisoned"))?;
    writer.validate(path)?;
    save_to_paths(
        path,
        &path.with_file_name("session-history.json"),
        snapshot,
        history,
    )
}

pub(crate) fn clear_ordered(
    path: &Path,
    writer: Option<&Arc<SessionWriter>>,
) -> std::io::Result<()> {
    let acquired;
    let writer = match writer {
        Some(writer) => writer,
        None => {
            acquired = SessionWriter::acquire(path)?;
            &acquired
        }
    };
    let _order = writer
        .order
        .lock()
        .map_err(|_| std::io::Error::other("session writer poisoned"))?;
    writer.validate(path)?;
    clear_path(path)?;
    std::fs::File::open(
        path.parent()
            .ok_or_else(|| std::io::Error::other("session parent missing"))?,
    )?
    .sync_all()?;
    clear_path(&path.with_file_name("session-history.json"))
}

fn save_json_to_path<T: serde::Serialize>(path: &Path, snapshot: &T) -> std::io::Result<()> {
    let target = resolve_write_target(path)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(snapshot)?;
    let tmp_path = target.with_extension("json.tmp");
    std::fs::write(&tmp_path, &json)?;
    if let Err(err) = std::fs::rename(&tmp_path, &target) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}

pub(super) fn save_to_paths(
    session_path: &Path,
    history_path: &Path,
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
) -> std::io::Result<()> {
    save_to_path(session_path, snapshot)?;
    if let Some(history) = history {
        save_json_to_path(history_path, history)?;
    } else {
        clear_path(history_path)?;
    }
    Ok(())
}

pub(super) fn clear_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

pub fn clear_history() {
    let path = session_history_path();
    if let Err(err) = clear_path(&path) {
        crate::logging::session_clear_failed(&path, &err.to_string());
    }
}

pub fn load() -> Option<SessionSnapshot> {
    let path = session_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session file");
            return None;
        }
    };
    match parse_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session file, ignoring");
            None
        }
    }
}

pub fn load_history() -> Option<SessionHistorySnapshot> {
    let path = session_history_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            warn!(err = %err, "failed to read session history file");
            return None;
        }
    };
    match parse_history_snapshot(&content) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            if let Some(version) = snapshot_file_version(&content) {
                if version > SNAPSHOT_VERSION {
                    warn!(
                        file_version = version,
                        supported = SNAPSHOT_VERSION,
                        "session history file is from a newer herdr version, ignoring"
                    );
                    return None;
                }
            }
            warn!(err = %err, "failed to parse session history file, ignoring");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::snapshot::{
        PaneHistorySnapshot, TabHistorySnapshot, WorkspaceHistorySnapshot,
    };

    fn temp_session_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-session-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("session.json")
    }

    fn temp_session_paths(name: &str) -> (PathBuf, PathBuf) {
        let session = temp_session_path(name);
        let history = session.with_file_name("session-history.json");
        (session, history)
    }

    fn empty_snapshot() -> SessionSnapshot {
        SessionSnapshot {
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
        }
    }

    fn history_snapshot(secret: &str) -> SessionHistorySnapshot {
        SessionHistorySnapshot {
            version: SNAPSHOT_VERSION,
            workspaces: vec![WorkspaceHistorySnapshot {
                tabs: vec![TabHistorySnapshot {
                    panes: std::collections::HashMap::from([(
                        0,
                        PaneHistorySnapshot {
                            ansi: secret.to_string(),
                            lines: 1,
                        },
                    )]),
                }],
            }],
        }
    }

    #[cfg(unix)]
    #[test]
    fn durable_session_barrier_propagates_every_write_stage_failure() {
        let path = temp_session_path("durable-faults");
        for step in [
            DurableStep::Write,
            DurableStep::FileSync,
            DurableStep::Rename,
            DurableStep::DirectorySync,
        ] {
            inject_durable_failure(Some(step));
            let result = save_to_path(&path, &empty_snapshot());
            inject_durable_failure(None);
            assert!(result.is_err(), "{step:?} must not acknowledge durability");
            assert!(!path
                .parent()
                .unwrap()
                .read_dir()
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp")));
        }
        save_to_path(&path, &empty_snapshot()).unwrap();
        assert!(path.is_file());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_session_writer_denies_competing_stale_saver() {
        let path = temp_session_path("exclusive-writer");
        let lease = SessionWriter::acquire(&path).unwrap();
        assert!(SessionWriter::acquire(&path).is_err());
        let mut durable = empty_snapshot();
        durable.selected = 7;
        save_snapshot_ordered(&path, &durable, &lease).unwrap();
        let mut stale = empty_snapshot();
        stale.selected = 1;
        let stale_path = path.clone();
        let stale_attempt =
            std::thread::spawn(move || save_ordered(&stale_path, &stale, None, None));
        assert!(
            stale_attempt.join().unwrap().is_err(),
            "concurrent older snapshot must not overwrite acknowledged edge"
        );
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["selected"], 7);
        assert!(
            clear_ordered(&path, None).is_err(),
            "competing clear cannot remove the acknowledged edge"
        );
        assert!(path.exists());
        clear_ordered(&path, Some(&lease)).unwrap();
        assert!(!path.exists());
        drop(lease);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn removed_writer_lock_quarantines_old_lease() {
        let path = temp_session_path("replaced-lock");
        let old = SessionWriter::acquire(&path).unwrap();
        std::fs::remove_file(path.parent().unwrap().join(".session-writer.lock")).unwrap();
        let new = SessionWriter::acquire(&path).unwrap();
        assert!(old.validate(&path).is_err());
        assert!(save_snapshot_ordered(&path, &empty_snapshot(), &old).is_err());
        save_snapshot_ordered(&path, &empty_snapshot(), &new).unwrap();
        drop(old);
        drop(new);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn save_to_paths_writes_pane_history_only_to_history_file() {
        let (session_path, history_path) = temp_session_paths("split-history");

        save_to_paths(
            &session_path,
            &history_path,
            &empty_snapshot(),
            Some(&history_snapshot("split-secret")),
        )
        .unwrap();

        let session = std::fs::read_to_string(&session_path).unwrap();
        let history = std::fs::read_to_string(&history_path).unwrap();
        assert!(!session.contains("split-secret"));
        assert!(!session.contains("history"));
        assert!(history.contains("split-secret"));
    }

    #[test]
    fn save_to_paths_removes_stale_history_when_history_is_disabled() {
        let (session_path, history_path) = temp_session_paths("clear-history");
        save_to_paths(
            &session_path,
            &history_path,
            &empty_snapshot(),
            Some(&history_snapshot("stale-secret")),
        )
        .unwrap();

        save_to_paths(&session_path, &history_path, &empty_snapshot(), None).unwrap();

        assert!(session_path.exists());
        assert!(!history_path.exists());
    }

    #[test]
    fn clear_path_removes_existing_session_file() {
        let path = temp_session_path("clear-existing");
        save_to_path(&path, &empty_snapshot()).unwrap();

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn clear_path_ignores_missing_session_file() {
        let path = temp_session_path("clear-missing");

        clear_path(&path).unwrap();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_preserves_existing_symlink() {
        let target = temp_session_path("symlink-target");
        let link = target.with_file_name("link.json");
        save_to_path(&target, &empty_snapshot()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let mut snap = empty_snapshot();
        snap.selected = 7;
        save_to_path(&link, &snap).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let parsed = parse_snapshot(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(parsed.selected, 7);
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_writes_through_dangling_symlink() {
        let target = temp_session_path("dangling-target");
        let link = target.with_file_name("link.json");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_to_path_resolves_relative_symlink() {
        let session = temp_session_path("relative-symlink");
        let dir = session.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        let target = dir.join("real.json");
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();

        save_to_path(&link, &empty_snapshot()).unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(target.exists());
    }
}
