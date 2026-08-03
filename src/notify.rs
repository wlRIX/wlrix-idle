// SPDX-License-Identifier: GPL-3.0-or-later
//! `ext-idle-notify-v1`, the client side: being told the session went idle.
//!
//! One `ext_idle_notification_v1` per configured timeout, each carrying its index in the step
//! list as its user data. The compositor arms a notification the moment it is created -- see
//! `wlrix-compositor/src/idle.rs`, *"Counting starts now, not at the next input"* -- which is
//! what makes creating one mean "start a full countdown from here". That is the whole basis of
//! how inhibiting works in [`crate::idle`]: to stop counting, destroy the objects; to start
//! again from the full timeout, create them.
//!
//! **Version 1 on purpose**, though the compositor offers 2. The only thing version 2 adds is
//! `get_input_idle_notification`, which ignores idle inhibitors -- and inhibitors are exactly
//! what we want honored. An `zwp_idle_inhibitor_v1` belongs to some other client's surface, so
//! this program cannot see it; binding version 1 leaves the compositor to hold our countdowns
//! off while a film is playing, with the same restart-from-full semantics we use ourselves.
//!
//! smithay-client-toolkit wraps neither interface, so the two `Dispatch` impls are written out,
//! the way `wlrix-desktop` does for `ext-foreign-toplevel-list-v1`.

use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

use crate::idle::Idle;

/// See the module docs: not 2.
pub const VERSION: u32 = 1;

// The manager has no events; it only hands out notifications.
delegate_noop!(Idle: ignore ExtIdleNotifierV1);

impl Dispatch<ExtIdleNotificationV1, usize> for Idle {
    fn event(
        state: &mut Self,
        _resource: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        index: &usize,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.went_idle(*index),
            // Real input. The compositor has already restarted every countdown of its own
            // accord, so this only has to undo what the timeouts did -- re-arming here would
            // destroy and recreate a set of notifications that are already running.
            ext_idle_notification_v1::Event::Resumed => {
                state.unwind("input");
            }
            _ => {}
        }
    }
}
