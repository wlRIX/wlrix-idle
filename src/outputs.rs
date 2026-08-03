// SPDX-License-Identifier: GPL-3.0-or-later
//! Switching the monitors off and on: `zwlr-output-power-management-v1`.
//!
//! The compositor allows one control object per output and tells a second one it lost
//! (`wlrix-compositor/src/power.rs`), so holding controls for the whole session would make
//! `wlopm` fail permanently -- for a feature that fires perhaps twice an hour. wlRIX's pieces
//! stay usable next to the standard tools, so the controls are taken when the screens go off
//! and released when they come back. While the session is awake nothing is held at all.
//!
//! Keeping them for the duration of the blank, rather than destroying them the instant the mode
//! is set, is what makes the protocol's `failed` event observable: an object destroyed in the
//! same breath would be gone before the compositor could say the request did not take.
//!
//! What is tracked here is *intent*, not observed state. [`crate::idle::Idle::blanked`] means
//! "this program owes the user a screen", and it is acted on whatever the compositor believes
//! -- which is also why the mode events below are only logged, never fed back into the state
//! machine.

use wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_output::WlOutput};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};

use crate::idle::Idle;

pub const VERSION: u32 = 1;

/// One output we are currently holding switched off.
pub struct Control {
    pub output: WlOutput,
    pub resource: ZwlrOutputPowerV1,
}

/// Switch every output off, taking a control for each.
///
/// Does nothing if the screens are already off, so a second timeout that also asks to blank
/// costs a function call rather than a fresh set of protocol objects.
pub fn blank(idle: &mut Idle) {
    if idle.blanked {
        return;
    }
    let Some(manager) = idle.power_manager.clone() else {
        warn!("asked to blank, but the compositor offers no output power control");
        return;
    };
    idle.blanked = true;

    let qh = idle.qh.clone();
    let outputs: Vec<WlOutput> = idle.outputs();
    if outputs.is_empty() {
        warn!("asked to blank, but no outputs are known");
        return;
    }
    for output in outputs {
        let resource = manager.get_output_power(&output, &qh, output.clone());
        resource.set_mode(Mode::Off);
        idle.controls.push(Control { output, resource });
    }
    info!("monitors off");
}

/// Switch the monitors back on and let go of the controls.
pub fn unblank(idle: &mut Idle) {
    if !idle.blanked {
        return;
    }
    idle.blanked = false;
    for control in idle.controls.drain(..) {
        control.resource.set_mode(Mode::On);
        control.resource.destroy();
    }
    info!("monitors on");
}

/// An output appeared while the screens are off.
///
/// A monitor plugged in at three in the morning must not light up the room, so it is switched
/// off to match the rest rather than being left as it arrived.
pub fn blank_new_output(idle: &mut Idle, output: WlOutput) {
    if !idle.blanked {
        return;
    }
    let Some(manager) = idle.power_manager.clone() else {
        return;
    };
    let qh = idle.qh.clone();
    let resource = manager.get_output_power(&output, &qh, output.clone());
    resource.set_mode(Mode::Off);
    idle.controls.push(Control { output, resource });
    info!("switched off an output that appeared while the screens were off");
}

/// An output went away. Its control went with it, so only our record needs clearing.
pub fn forget_output(idle: &mut Idle, output: &WlOutput) {
    idle.controls.retain(|control| &control.output != output);
}

/// Switch the monitors on for the last time, on the way out.
///
/// Separate from [`unblank`] because it has to reach the compositor before the process dies:
/// nothing else will ever switch these screens back on. The compositor deliberately leaves a
/// blank a client asked for alone (`power.rs`, `SetMode`), so no keypress undoes it, and by the
/// time anyone notices there is no session left to fix it from.
pub fn unblank_on_exit(idle: &mut Idle) {
    if !idle.blanked {
        return;
    }
    unblank(idle);
    if let Err(err) = idle.conn.roundtrip() {
        warn!("could not switch the monitors back on before exiting: {err}");
    }
}

// The manager has no events.
wayland_client::delegate_noop!(Idle: ignore ZwlrOutputPowerManagerV1);

impl Dispatch<ZwlrOutputPowerV1, WlOutput> for Idle {
    fn event(
        state: &mut Self,
        resource: &ZwlrOutputPowerV1,
        event: zwlr_output_power_v1::Event,
        _output: &WlOutput,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Someone else already holds this output, or it went away. The object is dead
            // either way; drop it, and say so, because the visible symptom is one monitor
            // staying lit when the rest went dark.
            zwlr_output_power_v1::Event::Failed => {
                warn!("an output refused to be switched off; something else is controlling it");
                state
                    .controls
                    .retain(|control| &control.resource != resource);
                resource.destroy();
            }
            // Only ever informational -- see the module docs on tracking intent.
            zwlr_output_power_v1::Event::Mode { .. } => {}
            _ => {}
        }
    }
}
