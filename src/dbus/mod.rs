// SPDX-License-Identifier: GPL-3.0-or-later
//! D-Bus, on threads of its own.
//!
//! zbus needs an async executor and calloop is resolutely synchronous, so the two are kept
//! apart rather than reconciled: the blocking API runs on dedicated threads which report into
//! the main loop through a `calloop::channel`. That is the same shape `wlrix-greeter` uses for
//! greetd, and it means no async runtime ever touches the state machine.
//!
//! Two threads. One owns the session bus and serves the inhibit interfaces; the other owns the
//! system bus and holds logind's delay inhibitor. They are separate because their lifetimes
//! are: the session one parks on a signal iterator for the life of the process, the logind one
//! spends most of its time blocked waiting for a suspend that may never come.
//!
//! ## A name already taken is not a failure
//!
//! `org.freedesktop.ScreenSaver` will already be owned whenever this runs inside another
//! desktop -- which is where most of the development happens. Refusing to start would take idle
//! blanking down with it, and blanking is the point of the program; taking the name from a live
//! KDE session would break KDE's own inhibit handling, which is worse than not having ours. So
//! each name is requested independently, and a refusal is a message naming who has it.

mod inhibit;
mod logind;
mod power;
mod screensaver;

use std::sync::{
    Arc, Mutex,
    mpsc::{self, Sender},
};
use std::time::{Duration, Instant};

use zbus::{
    fdo::RequestNameFlags,
    message::Header,
    names::{BusName, WellKnownName},
};

use crate::config::Config;
use inhibit::Inhibits;

/// Something the buses have to say to the main loop.
pub enum Event {
    /// Whether anything is asking the session to stay awake, and who.
    Inhibited { active: bool, detail: String },
    /// `SimulateUserActivity`.
    Activity,
    /// `ScreenSaver.Lock`.
    Lock,
    /// The machine is about to suspend and logind is holding the door.
    BeforeSleep,
    /// Back from suspend, with a fresh delay inhibitor already taken.
    AfterSleep,
    /// A name we wanted belongs to someone else.
    NameUnavailable { name: String, owner: String },
}

/// State the interfaces and the main loop both touch.
///
/// The interfaces run on the bus thread and must answer a method call synchronously, so the
/// registry lives behind a mutex rather than being owned by the loop. What crosses to the loop
/// is only the conclusion -- a bool -- through the channel.
pub struct Shared {
    inhibits: Mutex<Inhibits>,
    /// When the screen went dark or locked, if it has. Written by the main loop, read by
    /// `GetActive`.
    active: Mutex<Option<Instant>>,
    sender: calloop::channel::Sender<Event>,
}

impl Shared {
    fn with_inhibits<T>(&self, f: impl FnOnce(&mut Inhibits) -> T) -> T {
        // A poisoned mutex means an interface method panicked mid-update. The registry is a map
        // of cookies, not something that can be left half-written in a way that matters, so
        // carrying on with it is better than taking the session's inhibit handling down.
        let mut inhibits = match self.inhibits.lock() {
            Ok(inhibits) => inhibits,
            Err(poisoned) => poisoned.into_inner(),
        };
        f(&mut inhibits)
    }

    /// Tell the loop what the inhibit picture looks like now.
    fn report_inhibits(&self) {
        let (active, detail) =
            self.with_inhibits(|inhibits| (inhibits.active(), inhibits.detail()));
        let _ = self.sender.send(Event::Inhibited { active, detail });
    }

    fn report_activity(&self) {
        let _ = self.sender.send(Event::Activity);
    }

    fn report_lock(&self) {
        let _ = self.sender.send(Event::Lock);
    }

    fn active(&self) -> Option<Instant> {
        match self.active.lock() {
            Ok(active) => *active,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// What the main loop keeps hold of.
pub struct Handle {
    /// Answers a [`Event::BeforeSleep`]. Dropping the handle answers it too, by disconnecting.
    sleep_ack: Sender<()>,
    shared: Option<Arc<Shared>>,
}

impl Handle {
    /// Say that whatever had to happen before sleeping has been started.
    pub fn ack_sleep(&self) {
        let _ = self.sleep_ack.send(());
    }

    /// Record that the screen has gone dark or come back, for `GetActive`.
    pub fn set_active(&self, active: bool) {
        let Some(shared) = &self.shared else {
            return;
        };
        let mut current = match shared.active.lock() {
            Ok(current) => current,
            Err(poisoned) => poisoned.into_inner(),
        };
        match (active, *current) {
            // Already active: keep the original instant, so `GetActiveTime` measures from when
            // the screen actually went dark rather than from the last timeout to fire.
            (true, Some(_)) => {}
            (true, None) => *current = Some(Instant::now()),
            (false, _) => *current = None,
        }
    }
}

/// Start whichever bus threads the config asks for.
///
/// `None` when there is nothing to do -- no interfaces wanted and nothing to run before sleep --
/// in which case the main loop never learns about D-Bus at all.
pub fn spawn(config: &Config, replace: bool) -> Option<(Handle, calloop::channel::Channel<Event>)> {
    let wants_session = config.dbus.screensaver || config.dbus.power_management;
    let wants_logind = config.dbus.logind && config.before_sleep.does_something();
    if !wants_session && !wants_logind {
        return None;
    }

    let (sender, channel) = calloop::channel::channel();
    let (ack_sender, ack_receiver) = mpsc::channel();

    let shared = wants_session.then(|| {
        let shared = Arc::new(Shared {
            inhibits: Mutex::new(Inhibits::default()),
            active: Mutex::new(None),
            sender: sender.clone(),
        });
        let thread_shared = Arc::clone(&shared);
        let screensaver = config.dbus.screensaver;
        let power_management = config.dbus.power_management;
        std::thread::Builder::new()
            .name("wlrix-idle-session-bus".to_string())
            .spawn(move || session_bus(thread_shared, screensaver, power_management, replace))
            .map_err(|err| warn!("could not start the session bus thread: {err}"))
            .ok();
        shared
    });

    if wants_logind {
        let sender = sender.clone();
        let timeout = Duration::from_secs(config.before_sleep.timeout_secs);
        std::thread::Builder::new()
            .name("wlrix-idle-logind".to_string())
            .spawn(move || logind::run(sender, ack_receiver, timeout))
            .map_err(|err| warn!("could not start the logind thread: {err}"))
            .ok();
    }

    Some((
        Handle {
            sleep_ack: ack_sender,
            shared,
        },
        channel,
    ))
}

/// Own the session bus names and serve the inhibit interfaces, then watch for callers dying.
fn session_bus(shared: Arc<Shared>, screensaver: bool, power_management: bool, replace: bool) {
    let mut builder = match zbus::blocking::connection::Builder::session() {
        Ok(builder) => builder,
        Err(err) => {
            warn!("no session bus ({err}); applications cannot ask the screen to stay on");
            return;
        }
    };

    // The objects are served before any name is requested, so that a client which resolves the
    // name the instant it is acquired never finds an empty connection behind it.
    if screensaver {
        for path in [screensaver::PATH, screensaver::LEGACY_PATH] {
            builder = match builder.serve_at(
                path,
                screensaver::ScreenSaver {
                    shared: Arc::clone(&shared),
                },
            ) {
                Ok(builder) => builder,
                Err(err) => {
                    warn!("could not serve {path}: {err}");
                    return;
                }
            };
        }
    }
    if power_management {
        builder = match builder.serve_at(
            power::PATH,
            power::PowerManagement {
                shared: Arc::clone(&shared),
            },
        ) {
            Ok(builder) => builder,
            Err(err) => {
                warn!("could not serve {}: {err}", power::PATH);
                return;
            }
        };
    }

    let connection = match builder.build() {
        Ok(connection) => connection,
        Err(err) => {
            warn!("could not connect to the session bus: {err}");
            return;
        }
    };

    // Requested one at a time, and after the connection exists, so that one name being taken
    // does not cost us the other.
    if screensaver {
        request_name(&connection, &shared, screensaver::NAME, replace);
    }
    if power_management {
        request_name(&connection, &shared, power::NAME, replace);
    }

    watch_for_departures(&connection, &shared);
}

/// Take a well-known name, or say who has it.
fn request_name(
    connection: &zbus::blocking::Connection,
    shared: &Shared,
    name: &str,
    replace: bool,
) {
    let Ok(well_known) = WellKnownName::try_from(name) else {
        return;
    };
    // `DoNotQueue`: standing in a queue for a name would mean acquiring it later, silently, at
    // the moment another desktop's daemon exited -- which is not a thing anybody asked for.
    // `AllowReplacement` so a newer instance started with `--replace` can take over cleanly.
    let mut flags = RequestNameFlags::DoNotQueue | RequestNameFlags::AllowReplacement;
    if replace {
        flags |= RequestNameFlags::ReplaceExisting;
    }

    match connection.request_name_with_flags(well_known, flags) {
        Ok(_) => info!("serving {name}"),
        Err(err) => {
            let owner = owner_of(connection, name).unwrap_or_else(|| "something else".to_string());
            let _ = shared.sender.send(Event::NameUnavailable {
                name: name.to_string(),
                owner,
            });
            // The error itself is only interesting when it is not the ordinary "taken" case.
            if !matches!(err, zbus::Error::NameTaken) {
                warn!("could not take {name}: {err}");
            }
        }
    }
}

/// Who holds a name, described well enough to be recognized in a log.
fn owner_of(connection: &zbus::blocking::Connection, name: &str) -> Option<String> {
    let proxy = zbus::blocking::fdo::DBusProxy::new(connection).ok()?;
    let bus_name = BusName::try_from(name).ok()?;
    let owner = proxy.get_name_owner(bus_name).ok()?;
    let unique = BusName::from(owner.inner().clone());
    match proxy.get_connection_unix_process_id(unique.clone()) {
        Ok(pid) => Some(format!(
            "{} (pid {pid})",
            process_name(pid).unwrap_or(unique.to_string())
        )),
        Err(_) => Some(unique.to_string()),
    }
}

/// The command behind a pid, so the log says `kwin_wayland` rather than `:1.34`.
fn process_name(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Drop the inhibits of clients that have gone away.
///
/// The match rule is narrowed to `new_owner == ""` -- names disappearing -- so this thread is
/// not woken for every service that starts on the bus.
fn watch_for_departures(connection: &zbus::blocking::Connection, shared: &Shared) {
    let proxy = match zbus::blocking::fdo::DBusProxy::new(connection) {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!(
                "could not watch the bus for departures ({err}); an application that crashes will keep the session awake"
            );
            return;
        }
    };
    let signals = match proxy.receive_name_owner_changed_with_args(&[(2, "")]) {
        Ok(signals) => signals,
        Err(err) => {
            warn!(
                "could not watch the bus for departures ({err}); an application that crashes will keep the session awake"
            );
            return;
        }
    };

    for signal in signals {
        let Ok(args) = signal.args() else {
            continue;
        };
        let gone = args.name().to_string();
        let dropped = shared.with_inhibits(|inhibits| inhibits.drop_owner(&gone));
        if dropped > 0 {
            info!("{gone} went away holding {dropped} inhibit(s); releasing them");
            shared.report_inhibits();
        }
    }
}

/// The unique bus name behind a method call.
///
/// Empty only on a peer-to-peer connection, which is not how any of this is reached; an empty
/// string simply never matches a stored holder, so a cookie taken that way can only be released
/// by the owner going away.
fn caller(header: &Header<'_>) -> String {
    header
        .sender()
        .map(|sender| sender.to_string())
        .unwrap_or_default()
}
