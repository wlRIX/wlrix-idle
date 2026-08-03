// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.freedesktop.PowerManagement.Inhibit`: the older, coarser "do not suspend this".
//!
//! Predates `org.freedesktop.ScreenSaver` and is still what some applications reach for --
//! usually alongside the other one rather than instead of it, which is why the two registries
//! are kept apart: an application that takes both must be able to give each back independently.
//!
//! For deciding whether to run a countdown either kind counts. `HasInhibit`, though, answers
//! only for this one. Reporting the union there would tell a caller the machine is being kept
//! awake when in fact all anyone asked for was a screen that stays lit.

use std::sync::Arc;

use zbus::message::Header;

use super::{
    Shared,
    inhibit::{Holder, Kind},
};

pub const NAME: &str = "org.freedesktop.PowerManagement.Inhibit";
pub const PATH: &str = "/org/freedesktop/PowerManagement/Inhibit";

pub struct PowerManagement {
    pub shared: Arc<Shared>,
}

#[zbus::interface(name = "org.freedesktop.PowerManagement.Inhibit")]
impl PowerManagement {
    fn inhibit(
        &self,
        #[zbus(header)] header: Header<'_>,
        application: String,
        reason: String,
    ) -> u32 {
        let bus_name = super::caller(&header);
        let cookie = self.shared.with_inhibits(|inhibits| {
            inhibits.add(
                Kind::PowerManagement,
                Holder {
                    bus_name,
                    app: application.clone(),
                    reason: reason.clone(),
                },
            )
        });
        self.shared.report_inhibits();
        cookie
    }

    fn un_inhibit(&self, #[zbus(header)] header: Header<'_>, cookie: u32) {
        let caller = super::caller(&header);
        let outcome = self
            .shared
            .with_inhibits(|inhibits| inhibits.remove(Kind::PowerManagement, cookie, &caller));
        if let Err(err) = outcome {
            warn!("refused an UnInhibit: {err}");
            return;
        }
        self.shared.report_inhibits();
    }

    fn has_inhibit(&self) -> bool {
        self.shared
            .with_inhibits(|inhibits| inhibits.has_power_inhibit())
    }
}
