// SPDX-License-Identifier: GPL-3.0-or-later
//! Which profile applies: UPower's `OnBattery`.
//!
//! `true` only on a laptop running on its battery; a desktop, or a laptop on mains, is `false`,
//! and so is a machine with no UPower at all -- the plugged-in profile is the one that applies
//! when nothing says otherwise. The property emits `PropertiesChanged`, so the thread sends the
//! value once at the start and again whenever it changes.

use zbus::MatchRule;
use zbus::message::Type;
use zbus::zvariant::OwnedValue;

use super::Event;

const UPOWER: &str = "org.freedesktop.UPower";
const PATH: &str = "/org/freedesktop/UPower";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Watch the power source for as long as the process lives. Returns if the bus goes away.
pub fn run(sender: calloop::channel::Sender<Event>) {
    let connection = match zbus::blocking::Connection::system() {
        Ok(connection) => connection,
        Err(err) => {
            warn!("no system bus ({err}); the plugged-in profile applies");
            return;
        }
    };
    let watched = zbus::blocking::fdo::DBusProxy::new(&connection).and_then(|bus| {
        bus.add_match_rule(
            MatchRule::builder()
                .msg_type(Type::Signal)
                .interface(PROPERTIES)?
                .member("PropertiesChanged")?
                .path(PATH)?
                .build(),
        )
        .map_err(zbus::Error::from)
    });
    if let Err(err) = watched {
        warn!("could not watch UPower ({err}); the plugged-in profile applies");
        return;
    }

    let mut last = read(&connection);
    if sender.send(Event::OnBattery(last)).is_err() {
        return;
    }
    for message in zbus::blocking::MessageIterator::from(&connection) {
        let Ok(message) = message else { continue };
        if message.message_type() != Type::Signal {
            continue;
        }
        let now = read(&connection);
        if now != last {
            last = now;
            if sender.send(Event::OnBattery(now)).is_err() {
                return;
            }
        }
    }
}

/// `OnBattery`, or `false` when UPower does not answer.
fn read(connection: &zbus::blocking::Connection) -> bool {
    connection
        .call_method(
            Some(UPOWER),
            PATH,
            Some(PROPERTIES),
            "Get",
            &(UPOWER, "OnBattery"),
        )
        .ok()
        .and_then(|reply| reply.body().deserialize::<OwnedValue>().ok())
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false)
}
