// SPDX-License-Identifier: GPL-3.0-or-later
//! Locking before the machine sleeps.
//!
//! logind lets a process register a *delay* inhibitor: a file descriptor it holds, which logind
//! waits on -- up to `InhibitDelayMaxSec`, five seconds by default -- before actually
//! suspending. That window is the only place a locker can start with any guarantee of being up
//! before the screen comes back on the other side.
//!
//! The descriptor never leaves this thread. Handing it to the main loop would mean a wedged
//! loop wedging the machine's suspend, so instead the thread owns the whole sequence: tell the
//! loop what is happening, wait a bounded time for it to say it is done, then let go. If the
//! wait runs out the machine sleeps anyway, which is the right answer -- a hung locker is a
//! reason to log something, not a reason to keep a laptop awake in a bag.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use zbus::zvariant::OwnedFd;

use super::Event;

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    /// `what` is a space-separated list -- we only ever ask for `sleep`. `mode` is `delay` or
    /// `block`; `block` would refuse the suspend outright, which is not ours to do.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    /// `true` just before sleeping, `false` just after waking.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool);
}

/// Hold a delay inhibitor and run the before-sleep sequence for as long as the process lives.
///
/// Runs on its own thread. Returns only if the bus goes away.
pub fn run(sender: calloop::channel::Sender<Event>, ack: Receiver<()>, timeout: Duration) {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        // A session without a system bus is unusual but survivable: everything except
        // before-sleep still works.
        Err(err) => {
            warn!("no system bus ({err}); nothing will happen before the machine sleeps");
            return;
        }
    };

    let manager = match ManagerProxyBlocking::new(&connection) {
        Ok(manager) => manager,
        Err(err) => {
            warn!("could not reach logind ({err}); nothing will happen before the machine sleeps");
            return;
        }
    };

    let mut inhibitor = take(&manager);
    if inhibitor.is_none() {
        return;
    }

    let signals = match manager.receive_prepare_for_sleep() {
        Ok(signals) => signals,
        Err(err) => {
            warn!("could not listen for suspend ({err})");
            return;
        }
    };

    for signal in signals {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.start {
            let _ = sender.send(Event::BeforeSleep);
            // Bounded, and bounded here rather than on the loop: a main loop that has stopped
            // answering must not be able to stop the machine sleeping.
            match ack.recv_timeout(timeout) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => {
                    warn!("nothing finished before the sleep deadline; sleeping anyway");
                }
                // The main loop is gone, which means so is the rest of the program.
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Dropping the descriptor is what allows the suspend to proceed. It has to happen on
            // every path out of the branch above, including the one where something went wrong.
            drop(inhibitor.take());
        } else {
            // Awake again. The descriptor was consumed by the suspend, so a fresh one is needed
            // before the next one.
            inhibitor = take(&manager);
            let _ = sender.send(Event::AfterSleep);
            if inhibitor.is_none() {
                return;
            }
        }
    }
}

/// Take the delay inhibitor. `None` means we could not, and said so.
fn take(manager: &ManagerProxyBlocking<'_>) -> Option<OwnedFd> {
    match manager.inhibit(
        "sleep",
        "wlrix-idle",
        "Locking the session before sleep",
        "delay",
    ) {
        Ok(fd) => Some(fd),
        Err(err) => {
            warn!("could not take a sleep inhibitor ({err}); the machine may sleep unlocked");
            None
        }
    }
}
