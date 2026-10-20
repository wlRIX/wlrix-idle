// SPDX-License-Identifier: GPL-3.0-or-later
//! A stand-in logind, for trying the lid and the before-sleep lock without closing a real lid or
//! suspending a real machine.
//!
//! It takes `org.freedesktop.login1` on whatever bus `DBUS_SYSTEM_BUS_ADDRESS` names -- never
//! the real system bus: start a private one and point both this and `wlrix-idle` at it.
//!
//! ```text
//! dbus-daemon --session --print-address --fork      # the address it prints is the fake system bus
//! DBUS_SYSTEM_BUS_ADDRESS=<address> cargo run --example fake_logind
//! DBUS_SYSTEM_BUS_ADDRESS=<address> wlrix-idle
//! ```
//!
//! Then type lines on its standard input: `close`, `open`, `docked on|off`, `ac on|off`.
//!
//! What it serves is what `wlrix-idle` uses: `Inhibit` (answered with one end of a pipe, whose
//! closing it reports), `Suspend` -- which runs the delay-inhibitor dance, `PrepareForSleep(true)`,
//! a pause, then `PrepareForSleep(false)`, without sleeping -- `Hibernate`, which is refused as
//! on a machine where hibernation is not set up, and the `LidClosed`, `Docked` and
//! `OnExternalPower` properties. Every call is logged on standard error.

use std::collections::HashMap;
use std::io::BufRead;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::Value;

const PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

#[derive(Default)]
struct State {
    lid_closed: bool,
    docked: bool,
    external_power: bool,
}

struct Manager {
    state: Arc<Mutex<State>>,
    connection: Arc<Mutex<Option<zbus::blocking::Connection>>>,
}

#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::fdo::Result<zbus::zvariant::OwnedFd> {
        eprintln!("fake_logind: Inhibit({what:?}, {who:?}, {why:?}, {mode:?})");
        let mut fds = [0; 2];
        // SAFETY: a two-element array for pipe(2) to fill.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(zbus::fdo::Error::Failed("pipe".into()));
        }
        // SAFETY: both are fresh descriptors from pipe(2), owned from here on.
        let (read, write) = unsafe {
            use std::os::fd::FromRawFd;
            (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
        };
        let what = what.to_owned();
        // The write end hangs up when the holder closes the read end: that is the release.
        std::thread::spawn(move || {
            let mut poll = libc::pollfd {
                fd: write.as_raw_fd(),
                events: 0,
                revents: 0,
            };
            // SAFETY: one pollfd, waited on forever.
            unsafe { libc::poll(&mut poll, 1, -1) };
            eprintln!("fake_logind: inhibitor {what:?} released");
        });
        Ok(read.into())
    }

    fn suspend(&self, interactive: bool) {
        eprintln!("fake_logind: Suspend(interactive {interactive})");
        let connection = self.connection.clone();
        std::thread::spawn(move || {
            let Some(connection) = connection.lock().unwrap().clone() else {
                return;
            };
            let sleep = |start: bool| {
                let _ = connection.emit_signal(
                    None::<&str>,
                    PATH,
                    MANAGER,
                    "PrepareForSleep",
                    &(start,),
                );
            };
            sleep(true);
            eprintln!("fake_logind: PrepareForSleep(true); 'sleeping' for two seconds");
            std::thread::sleep(Duration::from_secs(2));
            sleep(false);
            eprintln!("fake_logind: PrepareForSleep(false); awake");
        });
    }

    fn hibernate(&self, interactive: bool) -> zbus::fdo::Result<()> {
        eprintln!("fake_logind: Hibernate(interactive {interactive}); refusing");
        Err(zbus::fdo::Error::NotSupported(
            "Sleep verb \"hibernate\" not supported".into(),
        ))
    }

    #[zbus(property)]
    fn lid_closed(&self) -> bool {
        self.state.lock().unwrap().lid_closed
    }

    #[zbus(property)]
    fn docked(&self) -> bool {
        self.state.lock().unwrap().docked
    }

    #[zbus(property)]
    fn on_external_power(&self) -> bool {
        self.state.lock().unwrap().external_power
    }
}

fn main() -> zbus::Result<()> {
    let state = Arc::new(Mutex::new(State::default()));
    let slot = Arc::new(Mutex::new(None));
    let connection = zbus::blocking::connection::Builder::system()?
        .name("org.freedesktop.login1")?
        .serve_at(
            PATH,
            Manager {
                state: state.clone(),
                connection: slot.clone(),
            },
        )?
        .build()?;
    *slot.lock().unwrap() = Some(connection.clone());
    eprintln!("fake_logind: ready");

    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|err| zbus::Error::Failure(err.to_string()))?;
        let words: Vec<&str> = line.split_whitespace().collect();
        let mut fake = state.lock().unwrap();
        match words.as_slice() {
            ["close"] => fake.lid_closed = true,
            ["open"] => fake.lid_closed = false,
            ["docked", on] => fake.docked = *on == "on",
            ["ac", on] => fake.external_power = *on == "on",
            [] => continue,
            _ => {
                eprintln!("fake_logind: did not understand {line:?}");
                continue;
            }
        }
        let changed: HashMap<&str, Value> =
            HashMap::from([("LidClosed", Value::from(fake.lid_closed))]);
        drop(fake);
        eprintln!("fake_logind: {line}");
        let invalidated: Vec<&str> = Vec::new();
        connection.emit_signal(
            None::<&str>,
            PATH,
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &(MANAGER, changed, invalidated),
        )?;
    }
    Ok(())
}
