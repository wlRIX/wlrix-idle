// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.ScreenSaver`: the interface applications use to say "not now".
//!
//! This is what Firefox, mpv, Steam and every video player reach for. Nothing in a wlRIX session
//! served it before, so those requests went nowhere and the screen blanked halfway through a
//! film -- with the application having done everything right.
//!
//! Served at **both** `/org/freedesktop/ScreenSaver` and `/ScreenSaver`. The short path is not
//! in any specification, but KDE and GNOME both register it and older VLC and some Firefox
//! builds use it. Two lines here against a bug report a year from now.
//!
//! `ActiveChanged` is deliberately not declared. Emitting it would have to come from this
//! thread, which spends its life parked on the name-watch iterator, while the thing that
//! changes -- the screen going dark -- happens on the main loop. Nothing is known to depend on
//! it: the applications that care call `Inhibit` and never listen. `GetActive` answers the same
//! question by polling, and is served.

use std::sync::Arc;

use zbus::message::Header;

use super::{
    Shared,
    inhibit::{Holder, Kind},
};

pub const NAME: &str = "org.freedesktop.ScreenSaver";
pub const PATH: &str = "/org/freedesktop/ScreenSaver";
/// Where KDE and GNOME also put it, and where older clients look.
pub const LEGACY_PATH: &str = "/ScreenSaver";

pub struct ScreenSaver {
    pub shared: Arc<Shared>,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    /// Ask the session not to blank. The cookie comes back to `UnInhibit`.
    fn inhibit(
        &self,
        #[zbus(header)] header: Header<'_>,
        application_name: String,
        reason: String,
    ) -> u32 {
        let bus_name = super::caller(&header);
        let cookie = self.shared.with_inhibits(|inhibits| {
            inhibits.add(
                Kind::ScreenSaver,
                Holder {
                    bus_name,
                    app: application_name.clone(),
                    reason: reason.clone(),
                },
            )
        });
        self.shared.report_inhibits();
        cookie
    }

    /// Give an inhibit back.
    fn un_inhibit(&self, #[zbus(header)] header: Header<'_>, cookie: u32) {
        let caller = super::caller(&header);
        let outcome = self
            .shared
            .with_inhibits(|inhibits| inhibits.remove(Kind::ScreenSaver, cookie, &caller));
        if let Err(err) = outcome {
            // Not an error reply: a client releasing a cookie twice is common and harmless, and
            // failing the call would make it look like something went wrong with the session.
            warn!("refused an UnInhibit: {err}");
            return;
        }
        self.shared.report_inhibits();
    }

    /// The user is there, even if the compositor could not tell.
    fn simulate_user_activity(&self) {
        self.shared.report_activity();
    }

    /// Lock now, without waiting for a timeout.
    fn lock(&self) {
        self.shared.report_lock();
    }

    /// Whether the screensaver is currently showing -- for us, whether the screen has gone dark
    /// or locked because a timeout ran out.
    fn get_active(&self) -> bool {
        self.shared.active().is_some()
    }

    /// How long it has been showing, in seconds. Zero when it is not.
    fn get_active_time(&self) -> u32 {
        self.shared
            .active()
            .map(|since| since.elapsed().as_secs().min(u32::MAX as u64) as u32)
            .unwrap_or(0)
    }

    /// How long the session has been idle. Not tracked separately from the above: the
    /// compositor owns the idle clock and this process only learns about it in steps.
    fn get_session_idle_time(&self) -> u32 {
        self.get_active_time()
    }
}
