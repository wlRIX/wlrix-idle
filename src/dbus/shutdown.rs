// SPDX-License-Identifier: GPL-3.0-or-later
//! Saving the session before the machine shuts down.
//!
//! The compositor saves the open windows when the session ends, so they can be reopened at the
//! next login. A logout ends the compositor first and the applications after it, so they are
//! all still there when it saves. A shutdown does not: logind signals every process in the
//! session at once, and an application that quits faster than the compositor takes its windows
//! with it before they are written down.
//!
//! So this holds a logind *shutdown* delay inhibitor, the same arrangement [`super::logind`]
//! uses before sleep. On `PrepareForShutdown` it sends the compositor `SIGUSR1` -- "save the
//! session now" -- waits for the file to be written, and lets go. The compositor then keeps
//! that snapshot through the rest of the shutdown rather than overwriting it.
//!
//! Its own thread and its own connection, never touching the main loop: like the sleep
//! inhibitor, nothing wedged elsewhere may hold the machine up, and the wait is bounded.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use zbus::zvariant::OwnedFd;

/// How long to wait for the compositor to write the session before letting the shutdown go.
///
/// Well inside logind's own `InhibitDelayMaxSec` (five seconds by default), so it is this
/// bound that ends the wait rather than logind giving up on us.
const SAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// How often to look at the file while waiting.
const POLL: Duration = Duration::from_millis(50);

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    /// `true` just before shutting down, `false` if the shutdown was cancelled.
    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool);
}

/// Hold a shutdown delay inhibitor and save the session whenever a shutdown starts.
///
/// Runs on its own thread. Returns only if the bus goes away.
pub fn run() {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(err) => {
            warn!("no system bus ({err}); the session will not be saved before a shutdown");
            return;
        }
    };
    let manager = match ManagerProxyBlocking::new(&connection) {
        Ok(manager) => manager,
        Err(err) => {
            warn!(
                "could not reach logind ({err}); the session will not be saved before a shutdown"
            );
            return;
        }
    };

    let mut inhibitor = take(&manager);
    if inhibitor.is_none() {
        return;
    }

    let signals = match manager.receive_prepare_for_shutdown() {
        Ok(signals) => signals,
        Err(err) => {
            warn!("could not listen for shutdown ({err})");
            return;
        }
    };

    for signal in signals {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.start {
            save_session();
            // What lets the shutdown go on, on every path.
            drop(inhibitor.take());
        } else if inhibitor.is_none() {
            // Cancelled. Take it again, for the next one.
            inhibitor = take(&manager);
        }
    }
}

/// Ask the compositor to save, and wait a bounded time for the file to change.
fn save_session() {
    let Some(pid) = compositor_pid() else {
        warn!("no compositor to save the session before shutting down");
        return;
    };
    let file = session_file();
    let before = file.as_deref().and_then(modified);

    // SAFETY: `kill` has no memory-safety preconditions.
    if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
        warn!(
            "could not ask the compositor to save the session: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    let Some(file) = file else {
        return;
    };
    let deadline = Instant::now() + SAVE_TIMEOUT;
    while Instant::now() < deadline {
        let now = modified(&file);
        if now.is_some() && now != before {
            info!("the session was saved before shutting down");
            return;
        }
        std::thread::sleep(POLL);
    }
    // In Explicitly mode the compositor deliberately writes nothing, so this is also the quiet
    // path for that.
    info!("no session written before the shutdown deadline; shutting down anyway");
}

/// The compositor's pid, from its pidfile -- if it names a running compositor.
fn compositor_pid() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(runtime_dir().join("wlrix-compositor.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // The comm name, cut to `TASK_COMM_LEN`: a pidfile left by a crash may name a pid the kernel
    // has since given to something else.
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    (comm.trim_end() == "wlrix-composito").then_some(pid)
}

/// `$XDG_RUNTIME_DIR`, else the temp dir: where the compositor puts its pidfile.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// `$XDG_STATE_HOME/wlrix/session.toml`, where the compositor saves the session.
fn session_file() -> Option<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    Some(state.join("wlrix/session.toml"))
}

fn modified(file: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(file).ok()?.modified().ok()
}

/// Take the delay inhibitor. `None` means we could not, and said so.
fn take(manager: &ManagerProxyBlocking<'_>) -> Option<OwnedFd> {
    match manager.inhibit(
        "shutdown",
        "wlrix-idle",
        "Saving the session before shutting down",
        "delay",
    ) {
        Ok(fd) => Some(fd),
        Err(err) => {
            warn!("could not take a shutdown inhibitor ({err}); the session may not be saved");
            None
        }
    }
}
