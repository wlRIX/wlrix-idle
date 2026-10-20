// SPDX-License-Identifier: GPL-3.0-or-later
//! The lid: taking it over from logind, and telling the loop when it opens or shuts.
//!
//! logind acts on the lid itself (`HandleLidSwitch=suspend`, set in `/etc/systemd/logind.conf`
//! for everybody and only by root) unless somebody holds a `handle-lid-switch` *block*
//! inhibitor. This thread holds one for the life of the process, so while `wlrix-idle` runs the
//! lid is the person's own `[lid]` -- and when it exits, crashes included, the descriptor closes
//! and logind's own setting is back in force without anyone having to clean up.
//!
//! logind still tracks the lid while inhibited and says so through `LidClosed`, which emits
//! `PropertiesChanged`. When it changes, `Docked` and `OnExternalPower` are read as they are at
//! that moment and sent along with it: those two decide which of `[lid]`'s actions applies, and
//! they are only ever wanted at the instant the lid moves.

use zbus::zvariant::{OwnedFd, OwnedValue};

use super::Event;

const LOGIND: &str = "org.freedesktop.login1";
const MANAGER_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Hold the lid for as long as the process lives, sending [`Event::Lid`] when it moves.
///
/// Runs on its own thread. Returns only if the bus goes away or logind cannot be reached, in
/// which case logind keeps handling the lid itself.
pub fn run(sender: calloop::channel::Sender<Event>) {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(err) => {
            warn!("no system bus ({err}); logind handles the lid");
            return;
        }
    };
    let _inhibitor: OwnedFd = match connection.call_method(
        Some(LOGIND),
        MANAGER_PATH,
        Some(MANAGER),
        "Inhibit",
        &(
            "handle-lid-switch",
            "wlrix-idle",
            "Closing the lid does what [lid] in idle.toml says",
            "block",
        ),
    ) {
        Ok(reply) => match reply.body().deserialize() {
            Ok(fd) => fd,
            Err(err) => {
                warn!("logind answered the lid inhibitor oddly ({err}); it handles the lid");
                return;
            }
        },
        Err(err) => {
            warn!("could not take the lid from logind ({err}); it handles the lid");
            return;
        }
    };
    info!("handling the lid");

    let Ok(proxy) = zbus::blocking::fdo::PropertiesProxy::builder(&connection)
        .destination(LOGIND)
        .and_then(|builder| builder.path(MANAGER_PATH))
        .and_then(|builder| builder.build())
    else {
        warn!("could not watch logind's lid; it is held but nothing will act on it");
        return;
    };
    let signals = match proxy.receive_properties_changed() {
        Ok(signals) => signals,
        Err(err) => {
            warn!("could not watch logind's lid ({err})");
            return;
        }
    };

    let mut closed = read(&connection, "LidClosed").unwrap_or(false);
    for signal in signals {
        let Ok(args) = signal.args() else {
            continue;
        };
        if args.interface_name.as_str() != MANAGER {
            continue;
        }
        let now = match args.changed_properties.get("LidClosed") {
            Some(value) => bool::try_from(value).ok(),
            None if args.invalidated_properties.contains(&"LidClosed") => {
                read(&connection, "LidClosed")
            }
            None => None,
        };
        let Some(now) = now else {
            continue;
        };
        if now == closed {
            continue;
        }
        closed = now;
        let event = Event::Lid {
            closed,
            docked: read(&connection, "Docked").unwrap_or(false),
            // Unknown is treated as on battery: the cautious reading, where suspending saves
            // the battery and the work.
            external_power: read(&connection, "OnExternalPower").unwrap_or(false),
        };
        if sender.send(event).is_err() {
            return;
        }
    }
}

/// One of logind's boolean properties.
fn read(connection: &zbus::blocking::Connection, property: &str) -> Option<bool> {
    let reply = connection
        .call_method(
            Some(LOGIND),
            MANAGER_PATH,
            Some(PROPERTIES),
            "Get",
            &(MANAGER, property),
        )
        .ok()?;
    let value: OwnedValue = reply.body().deserialize().ok()?;
    bool::try_from(value).ok()
}

/// How the machine should go to sleep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sleep {
    Suspend,
    Hibernate,
}

/// Ask logind to sleep, on a thread of its own, sending [`Event::SleepRefused`] if it says no.
///
/// Not on the loop: logind answers only once it has asked every delay inhibitor -- this
/// program's own among them, whose answer comes *from* the loop.
pub fn sleep(kind: Sleep, sender: calloop::channel::Sender<Event>) {
    let spawned = std::thread::Builder::new()
        .name("wlrix-idle-sleep".to_string())
        .spawn(move || {
            let method = match kind {
                Sleep::Suspend => "Suspend",
                Sleep::Hibernate => "Hibernate",
            };
            let result = zbus::blocking::Connection::system().and_then(|connection| {
                // `interactive = false`: nobody is there to answer a polkit prompt with the lid
                // shut.
                connection.call_method(Some(LOGIND), MANAGER_PATH, Some(MANAGER), method, &(false,))
            });
            if let Err(err) = result {
                let _ = sender.send(Event::SleepRefused {
                    why: format!("{method}: {err}"),
                });
            }
        });
    if let Err(err) = spawned {
        warn!("could not start the sleep thread: {err}");
    }
}
