// SPDX-License-Identifier: GPL-3.0-or-later
//! Who has asked the session to stay awake, and on what authority.
//!
//! Two interfaces feed one registry. `org.freedesktop.ScreenSaver` is what a video player or a
//! browser uses to say "do not blank this"; `org.freedesktop.PowerManagement.Inhibit` is the
//! older, coarser "do not suspend this". They are kept apart because `HasInhibit` is a question
//! about the second one alone and answering it with the union would make it lie -- but for
//! deciding whether to run a countdown, either is enough.
//!
//! ## Cookies
//!
//! Sequential, skipping zero and anything currently live. Not random: a bus is already a
//! per-user trust boundary, so a guessable cookie buys an attacker nothing they could not do by
//! calling `Inhibit` themselves -- and a cookie you can follow through a log is worth a great
//! deal when working out why a screen would not blank.
//!
//! ## Clients that die
//!
//! The important case, and the one that cannot be tested by hand. An application that takes an
//! inhibit and then crashes never calls `UnInhibit`, and nothing else would ever release it: the
//! session simply stops blanking, for the rest of the login, with no sign of why. So the caller's
//! unique bus name is recorded with every cookie, and [`Inhibits::drop_owner`] clears them when
//! the bus says that name has gone.

use std::collections::HashMap;

/// Which interface an inhibit came in through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ScreenSaver,
    PowerManagement,
}

/// One outstanding inhibit.
#[derive(Debug, Clone)]
pub struct Holder {
    /// The caller's unique bus name, e.g. `:1.42`. What ties a cookie to a living process.
    pub bus_name: String,
    /// What the application called itself.
    pub app: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Inhibits {
    next: u32,
    screensaver: HashMap<u32, Holder>,
    power: HashMap<u32, Holder>,
}

impl Inhibits {
    fn map(&mut self, kind: Kind) -> &mut HashMap<u32, Holder> {
        match kind {
            Kind::ScreenSaver => &mut self.screensaver,
            Kind::PowerManagement => &mut self.power,
        }
    }

    /// Take an inhibit and hand back its cookie.
    pub fn add(&mut self, kind: Kind, holder: Holder) -> u32 {
        let cookie = self.allocate();
        self.map(kind).insert(cookie, holder);
        cookie
    }

    /// A cookie no caller currently holds. Zero is never used, so a client that keeps a cookie
    /// in a zero-initialized field cannot accidentally release someone else's.
    fn allocate(&mut self) -> u32 {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next == 0 {
                continue;
            }
            if !self.screensaver.contains_key(&self.next) && !self.power.contains_key(&self.next) {
                return self.next;
            }
        }
    }

    /// Release an inhibit, if the caller is the one that took it.
    ///
    /// A mismatch is refused rather than honored. Without this, one application passing around
    /// a stale cookie can void another's inhibit -- and the symptom is a screen that blanks
    /// during someone else's film, which nobody would ever trace back to here.
    pub fn remove(&mut self, kind: Kind, cookie: u32, caller: &str) -> Result<(), String> {
        let Some(holder) = self.map(kind).get(&cookie) else {
            return Err(format!("no inhibit with cookie {cookie}"));
        };
        if holder.bus_name != caller {
            return Err(format!(
                "cookie {cookie} belongs to {}, not to {caller}",
                holder.bus_name
            ));
        }
        self.map(kind).remove(&cookie);
        Ok(())
    }

    /// Forget everything a departed bus name was holding. Returns how many were dropped.
    pub fn drop_owner(&mut self, bus_name: &str) -> usize {
        let before = self.screensaver.len() + self.power.len();
        self.screensaver
            .retain(|_, holder| holder.bus_name != bus_name);
        self.power.retain(|_, holder| holder.bus_name != bus_name);
        before - (self.screensaver.len() + self.power.len())
    }

    /// Whether anything at all is asking the session to stay awake.
    pub fn active(&self) -> bool {
        !self.screensaver.is_empty() || !self.power.is_empty()
    }

    /// Whether anything is asking the *machine* not to suspend.
    ///
    /// Only the power-management set, deliberately. "Do not blank my screen" and "do not
    /// suspend my machine" are different questions, and `HasInhibit` asks the second.
    pub fn has_power_inhibit(&self) -> bool {
        !self.power.is_empty()
    }

    /// Something to put in the log, naming who is responsible.
    pub fn detail(&self) -> String {
        let mut names: Vec<String> = self
            .screensaver
            .values()
            .chain(self.power.values())
            .map(|holder| {
                if holder.reason.is_empty() {
                    holder.app.clone()
                } else {
                    format!("{} ({})", holder.app, holder.reason)
                }
            })
            .collect();
        names.sort();
        names.dedup();
        if names.is_empty() {
            "nothing".to_string()
        } else {
            names.join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(bus_name: &str) -> Holder {
        Holder {
            bus_name: bus_name.to_string(),
            app: "mpv".to_string(),
            reason: "playing".to_string(),
        }
    }

    #[test]
    fn a_cookie_is_never_zero_and_never_reused_while_live() {
        // Wound to the point where the next increment wraps to zero.
        let mut inhibits = Inhibits {
            next: u32::MAX,
            ..Inhibits::default()
        };
        let first = inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert_ne!(first, 0, "zero would collide with an uninitialized field");

        let second = inhibits.add(Kind::PowerManagement, holder(":1.2"));
        assert_ne!(first, second, "a live cookie must not be handed out twice");
    }

    #[test]
    fn a_cookie_from_one_interface_does_not_release_the_other() {
        let mut inhibits = Inhibits::default();
        let cookie = inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert!(
            inhibits
                .remove(Kind::PowerManagement, cookie, ":1.1")
                .is_err()
        );
        assert!(inhibits.active());
    }

    #[test]
    fn uninhibit_from_a_different_bus_name_is_refused() {
        let mut inhibits = Inhibits::default();
        let cookie = inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert!(inhibits.remove(Kind::ScreenSaver, cookie, ":1.9").is_err());
        assert!(inhibits.active(), "someone else's inhibit still stands");
        assert!(inhibits.remove(Kind::ScreenSaver, cookie, ":1.1").is_ok());
        assert!(!inhibits.active());
    }

    #[test]
    fn a_client_that_vanishes_loses_only_its_own_cookies() {
        // The whole reason NameOwnerChanged is watched: a browser that crashes mid-video would
        // otherwise keep the session awake until logout.
        let mut inhibits = Inhibits::default();
        inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        inhibits.add(Kind::PowerManagement, holder(":1.1"));
        inhibits.add(Kind::ScreenSaver, holder(":1.2"));

        assert_eq!(inhibits.drop_owner(":1.1"), 2);
        assert!(inhibits.active(), ":1.2 is still holding one");
        assert_eq!(inhibits.drop_owner(":1.2"), 1);
        assert!(!inhibits.active());
    }

    #[test]
    fn dropping_a_name_that_holds_nothing_changes_nothing() {
        // NameOwnerChanged fires for every name on the bus; most have nothing to do with us.
        let mut inhibits = Inhibits::default();
        inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert_eq!(inhibits.drop_owner(":1.7"), 0);
        assert!(inhibits.active());
    }

    #[test]
    fn has_inhibit_reports_only_the_power_management_set() {
        let mut inhibits = Inhibits::default();
        inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert!(
            !inhibits.has_power_inhibit(),
            "not blanking the screen is not the same as not suspending"
        );
        assert!(inhibits.active(), "but it does hold off the countdown");

        inhibits.add(Kind::PowerManagement, holder(":1.2"));
        assert!(inhibits.has_power_inhibit());
    }

    #[test]
    fn the_detail_names_who_is_responsible() {
        let mut inhibits = Inhibits::default();
        inhibits.add(Kind::ScreenSaver, holder(":1.1"));
        assert_eq!(inhibits.detail(), "mpv (playing)");
        assert_eq!(Inhibits::default().detail(), "nothing");
    }
}
