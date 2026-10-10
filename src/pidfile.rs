// SPDX-License-Identifier: GPL-3.0-or-later
//! A pidfile, so the config can be reloaded without hunting for the process.
//!
//! `SIGHUP` re-reads `idle.toml` (see [`crate::signals`]), and the whole point of that is being
//! able to change a timeout without restarting -- which needs a pid, and a sibling process
//! cannot read one from the environment. So it goes in a well-known file under the per-user
//! runtime directory, the same arrangement `wlrix-compositor` uses.
//!
//! The file is removed on a clean exit via the returned [`Guard`]. A crash leaves it stale; a
//! reader should treat "no such process" as "not running" rather than trusting the file
//! blindly.
//!
//! # Two copies at once
//!
//! The path is fixed, so a second `wlrix-idle` -- started by hand while the session's is running,
//! or inside a nested compositor for testing -- would overwrite the running one's pidfile on
//! startup and delete it on exit, and every later settings change would find nothing to signal. So
//! the file belongs to whoever took it first: a file naming another live `wlrix-idle` (checked by
//! `/proc/<pid>/comm`, so a pid reused after a crash does not count) is left alone, and on exit the
//! file is removed only if it still names this process. The same rule as
//! `wlrix-compositor/src/pidfile.rs`, kept in step by hand.

use std::path::{Path, PathBuf};

/// Named for the process, beside the compositor's.
const PID_NAME: &str = "wlrix-idle.pid";

/// What the kernel calls this program in `/proc/<pid>/comm` (the name, cut to 15 bytes).
const COMM: &str = "wlrix-idle";

/// `$XDG_RUNTIME_DIR` (owned by one user, cleaned up on logout), else the temp dir -- the same
/// rule the compositor's pidfile follows, so the two sit together.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// Where the pidfile lives.
pub fn path() -> PathBuf {
    runtime_dir().join(PID_NAME)
}

/// Write this process's pid, unless another running copy of this program already holds the file.
/// Returns a guard that removes the file when dropped; failure to write is logged and swallowed,
/// since a missing pidfile is not worth refusing to start over -- it only costs the reload its
/// convenience.
pub fn write() -> Option<Guard> {
    claim(&path(), std::process::id(), is_running)
}

/// [`write`], with the path, this process's pid and the "is that another copy of this program?"
/// test passed in, so the decisions can be tested without a second copy.
fn claim(path: &Path, me: u32, is_running: impl Fn(u32) -> bool) -> Option<Guard> {
    if let Some(holder) = read(path)
        && holder != me
        && is_running(holder)
    {
        info!(
            "another wlrix-idle (pid {holder}) holds {}; running without one",
            path.display()
        );
        return None;
    }

    match create(path).and_then(|mut file| {
        use std::io::Write;
        write!(file, "{me}")
    }) {
        Ok(()) => Some(Guard {
            path: path.to_owned(),
            pid: me,
        }),
        Err(err) => {
            warn!("could not write {}: {err}", path.display());
            None
        }
    }
}

/// The pid a pidfile names, if it exists and names one.
fn read(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Whether `pid` is a running `wlrix-idle`. By name, not by "is anything alive there": a pidfile
/// left by a crash may name a pid the kernel has since given to something else.
fn is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim_end() == COMM)
}

/// Create (or truncate) the pidfile. `O_NOFOLLOW` because the path is predictable and the
/// temp-dir fallback is world-writable: without it, anyone could leave a symlink there and have
/// this process truncate a file of their choosing.
fn create(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Removes the pidfile on drop, so a live pidfile means a live daemon.
pub struct Guard {
    path: PathBuf,
    pid: u32,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Only if it is still ours: another copy that found this one gone may have written its
        // own pid since, and removing that would hide a process that is still running.
        if read(&self.path) != Some(self.pid) {
            return;
        }
        if let Err(err) = std::fs::remove_file(&self.path) {
            warn!("could not remove {}: {err}", self.path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pidfile path in a fresh directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wlrix-idle-pidfile-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            self.0.join(PID_NAME)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ME: u32 = 2000;
    const OTHER: u32 = 1000;

    #[test]
    fn an_absent_file_is_written_and_removed_on_exit() {
        let scratch = Scratch::new("absent");
        let guard = claim(&scratch.file(), ME, |_| false).expect("should claim the file");
        assert_eq!(read(&scratch.file()), Some(ME));
        drop(guard);
        assert!(!scratch.file().exists());
    }

    #[test]
    fn a_running_copys_file_is_left_alone() {
        let scratch = Scratch::new("running");
        std::fs::write(scratch.file(), OTHER.to_string()).unwrap();
        assert!(claim(&scratch.file(), ME, |pid| pid == OTHER).is_none());
        assert_eq!(read(&scratch.file()), Some(OTHER));
    }

    #[test]
    fn a_stale_file_is_taken_over() {
        let scratch = Scratch::new("stale");
        std::fs::write(scratch.file(), OTHER.to_string()).unwrap();
        let _guard = claim(&scratch.file(), ME, |_| false).expect("should take over");
        assert_eq!(read(&scratch.file()), Some(ME));
    }

    #[test]
    fn a_file_somebody_else_has_since_taken_is_not_removed() {
        let scratch = Scratch::new("taken");
        let guard = claim(&scratch.file(), ME, |_| false).unwrap();
        std::fs::write(scratch.file(), OTHER.to_string()).unwrap();
        drop(guard);
        assert_eq!(read(&scratch.file()), Some(OTHER));
    }

    #[test]
    fn the_comm_name_fits_what_the_kernel_keeps() {
        // `TASK_COMM_LEN` is 16 including the terminator; a longer name is cut to 15.
        assert!(COMM.len() <= 15);
        assert!(!is_running(u32::MAX));
    }
}
