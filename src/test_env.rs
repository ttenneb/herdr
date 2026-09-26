//! Test-only serialization of process-global state shared by parallel tests.
//!
//! Unit tests run on many threads of one process. Some tests replace process
//! environment such as `PATH`, `HOME`, `DISPLAY` or `XDG_CONFIG_HOME`. Others
//! spawn children that resolve programs, or read config, through that same
//! environment. Before this module each area had its own lock, so a `PATH`
//! swap in one module could make an unrelated spawn fail in another.
//!
//! - [`lock`] is exclusive. Take it before mutating process environment.
//! - [`shared`] may be held by many tests at once. Take it in tests whose
//!   children or config lookups depend on the environment staying intact.
//!
//! Guards are re-entrant per thread, so a test that already holds a guard can
//! call helpers that take one. Upgrading from shared to exclusive on the same
//! thread is a bug and panics instead of deadlocking.

use std::cell::Cell;
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

static ENV: RwLock<()> = RwLock::new(());

#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    None,
    Shared,
    Exclusive,
}

thread_local! {
    static HELD: Cell<Held> = const { Cell::new(Held::None) };
}

#[must_use = "the environment is only protected while the guard is alive"]
pub(crate) struct EnvGuard {
    previous: Held,
    _read: Option<RwLockReadGuard<'static, ()>>,
    _write: Option<RwLockWriteGuard<'static, ()>>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        HELD.with(|held| held.set(self.previous));
    }
}

/// Exclusive access: no other test holds [`lock`] or [`shared`] meanwhile.
pub(crate) fn lock() -> EnvGuard {
    let previous = HELD.with(Cell::get);
    match previous {
        Held::Exclusive => EnvGuard {
            previous,
            _read: None,
            _write: None,
        },
        Held::Shared => panic!("test_env::lock() while this thread holds test_env::shared()"),
        Held::None => {
            let write = ENV.write().unwrap_or_else(PoisonError::into_inner);
            HELD.with(|held| held.set(Held::Exclusive));
            EnvGuard {
                previous,
                _read: None,
                _write: Some(write),
            }
        }
    }
}

/// Shared access: excludes [`lock`] holders only.
pub(crate) fn shared() -> EnvGuard {
    let previous = HELD.with(Cell::get);
    if previous != Held::None {
        return EnvGuard {
            previous,
            _read: None,
            _write: None,
        };
    }
    let read = ENV.read().unwrap_or_else(PoisonError::into_inner);
    HELD.with(|held| held.set(Held::Shared));
    EnvGuard {
        previous,
        _read: Some(read),
        _write: None,
    }
}

/// Test builds only: where `config_dir()` and `state_dir()` resolve when a
/// test has not deliberately pointed them somewhere.
///
/// Those directories follow `XDG_CONFIG_HOME`, `XDG_STATE_HOME` and `HOME`,
/// which tests holding [`lock`] replace. Any other test that reaches them
/// (a plugin registry refresh on an event, a session save, a manifest cache)
/// would otherwise write into whichever test directory is set right now, or
/// read and write the developer's real config and state dirs. The environment
/// is honoured only for a [`lock`] holder that set `xdg_var` or `HOME`; every
/// other caller gets a private per-process directory.
pub(crate) fn sandbox_dir(kind: &str, xdg_var: &str) -> Option<std::path::PathBuf> {
    if HELD.with(Cell::get) == Held::Exclusive
        && (std::env::var_os(xdg_var).is_some() || home_overridden())
    {
        return None;
    }
    static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| {
        std::env::temp_dir().join(format!("herdr-test-home-{}", std::process::id()))
    });
    Some(root.join(kind))
}

/// Whether `HOME` differs from the account's home directory.
fn home_overridden() -> bool {
    let Some(home) = std::env::var_os("HOME") else {
        return true;
    };
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut buffer = vec![0u8; 16 * 1024];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: getpwuid_r writes only into `entry` and `buffer`, whose
        // sizes are passed; `result` is null or points at `entry`.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status != 0 || result.is_null() || entry.pw_dir.is_null() {
            return true;
        }
        // SAFETY: pw_dir is a NUL-terminated string inside `buffer`.
        let account_home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        account_home.to_bytes() != home.as_bytes()
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        false
    }
}

/// Write an owner-only executable script without this process ever holding a
/// writable descriptor on it.
///
/// Writing a script in-process and then executing it races with every other
/// test thread that forks: the child briefly inherits the write descriptor, and
/// `execve` of the script then fails with `ETXTBSY`. A short-lived `cat`
/// child owns the only writable descriptor and has exited before we return.
#[cfg(unix)]
pub(crate) fn write_executable(path: &std::path::Path, contents: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg("umask 077 && /bin/rm -f -- \"$1\" && /bin/cat > \"$1\" && /bin/chmod 700 -- \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn script writer");
    child
        .stdin
        .take()
        .expect("script writer stdin")
        .write_all(contents.as_bytes())
        .expect("write script contents");
    let status = child.wait().expect("wait for script writer");
    assert!(
        status.success(),
        "script writer failed for {}",
        path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_state_dirs_follow_the_environment_only_under_the_lock() {
        let sandboxed = crate::config::config_dir();
        assert!(sandboxed.starts_with(sandbox_dir("config", "XDG_CONFIG_HOME").unwrap()));
        assert!(
            crate::config::state_dir().starts_with(sandbox_dir("state", "XDG_STATE_HOME").unwrap())
        );
        let _guard = lock();
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/nonexistent/xdg-config");
        assert_eq!(
            crate::config::config_dir(),
            std::path::Path::new("/nonexistent/xdg-config").join(crate::config::app_dir_name())
        );
        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn guards_nest_on_one_thread() {
        let _outer = lock();
        let _inner = lock();
        let _shared = shared();
    }

    #[test]
    fn shared_guards_nest_and_release() {
        {
            let _outer = shared();
            let _inner = shared();
        }
        let _exclusive = lock();
    }

    #[test]
    #[should_panic(expected = "while this thread holds test_env::shared()")]
    fn upgrading_shared_to_exclusive_panics() {
        let _shared = shared();
        let _exclusive = lock();
    }

    #[cfg(unix)]
    #[test]
    fn write_executable_creates_private_runnable_script() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "herdr-test-env-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hello");
        write_executable(&script, "#!/bin/sh\nprintf hello\n");
        let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let output = std::process::Command::new(&script).output().unwrap();
        assert_eq!(output.stdout, b"hello");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
