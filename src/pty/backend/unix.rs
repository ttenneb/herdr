use std::os::fd::{FromRawFd, OwnedFd};

use portable_pty::{native_pty_system, Child, CommandBuilder, PtySize};

use crate::pty::fd;

pub(crate) struct SpawnedPty {
    pub master_fd: OwnedFd,
    pub child: Box<dyn Child + Send + Sync>,
}

pub(crate) fn spawn_with_portable_pty(
    rows: u16,
    cols: u16,
    cmd: CommandBuilder,
) -> std::io::Result<SpawnedPty> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    let master_fd = pair
        .master
        .as_raw_fd()
        .ok_or_else(|| std::io::Error::other("pty master fd is unavailable"))?;
    let actor_fd = fd::duplicate_cloexec_fd(master_fd)?;
    let actor_fd = unsafe { OwnedFd::from_raw_fd(actor_fd) };
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| std::io::Error::other(err.to_string()))?;
    drop(pair);

    Ok(SpawnedPty {
        master_fd: actor_fd,
        child,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, RawFd};

    fn pts_number(fd: RawFd) -> Option<u32> {
        let mut number: libc::c_uint = 0;
        (unsafe { libc::ioctl(fd, libc::TIOCGPTN, &mut number) } == 0).then_some(number)
    }

    /// Parent descriptors that belong to one pty pair: masters are `/dev/ptmx`
    /// fds whose pty number matches, slaves are `/dev/pts/<number>`. Other
    /// tests open their own ptys concurrently, so a process-wide count of pty
    /// fds is not a property of this setup; this is.
    fn parent_fds_for_pty(number: u32) -> (Vec<RawFd>, Vec<RawFd>) {
        let slave = format!("/dev/pts/{number}");
        let (mut masters, mut slaves) = (Vec::new(), Vec::new());
        for entry in std::fs::read_dir("/proc/self/fd").expect("list /proc/self/fd") {
            let Ok(entry) = entry else { continue };
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<RawFd>().ok())
            else {
                continue;
            };
            let Ok(target) = std::fs::read_link(entry.path()) else {
                continue;
            };
            if target == std::path::Path::new("/dev/ptmx") && pts_number(fd) == Some(number) {
                masters.push(fd);
            } else if target == std::path::Path::new(&slave) {
                slaves.push(fd);
            }
        }
        (masters, slaves)
    }

    #[test]
    fn portable_pty_setup_leaves_one_parent_pty_fd() {
        let mut cmd = CommandBuilder::new("/bin/cat");
        cmd.env(crate::HERDR_ENV_VAR, crate::HERDR_ENV_VALUE);

        let mut spawned =
            spawn_with_portable_pty(24, 80, cmd).expect("portable pty setup succeeds");
        let master = spawned.master_fd.as_raw_fd();
        let number = pts_number(master).expect("spawned master is a pty master");
        let (masters, slaves) = parent_fds_for_pty(number);

        assert_eq!(
            (masters, slaves),
            (vec![master], Vec::new()),
            "portable-pty setup should leave only the Herdr-owned master fd of pty {number} in the parent"
        );

        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        drop(spawned.master_fd);
    }
}
