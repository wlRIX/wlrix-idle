// SPDX-License-Identifier: GPL-3.0-or-later
//! Deciding what counts as a controller.
//!
//! Deliberately **not** udev's `ID_INPUT_JOYSTICK`, which is wrong often enough to matter. On
//! the machine this was written on, `/dev/input/event15` is a Keychron K8 Pro's "System
//! Control" node -- the power and sleep keys a keyboard exposes as a separate HID device -- and
//! udev tags it as a joystick, symlink and all. Trusting that tag makes the user's keyboard a
//! gamepad, so a stray brush of the keyboard would count as controller input and the classifier
//! would be worse than nothing.
//!
//! What is used instead is the capability bits, which describe what the device can actually
//! report. The rules below are deliberately positive-then-negative: something must have a
//! recognisable gamepad or joystick button, *and* must not look like a keyboard or a pointer.
//! The negative half exists because plenty of things carry a stray `BTN_*`.
//!
//! Everything here is a pure function over [`Caps`] so it can be tested against the devices
//! that matter without any of them being plugged in.

/// `KEY_ESC`. A device with escape and a letter key is a keyboard, whatever else it claims --
//  the same test libinput uses.
const KEY_ESC: u16 = 1;
/// `KEY_Q`, the letter half of that test.
const KEY_Q: u16 = 16;
/// `BTN_LEFT`, the mouse button.
const BTN_LEFT: u16 = 0x110;
/// `BTN_TRIGGER`, the first joystick button: flight sticks and wheels have this and no
/// `BTN_SOUTH`.
const BTN_TRIGGER: u16 = 0x120;
/// `BTN_SOUTH` (A / cross), the first gamepad button.
const BTN_SOUTH: u16 = 0x130;
/// `BTN_DPAD_UP`, for pads that report a hat as buttons rather than as an axis.
const BTN_DPAD_UP: u16 = 0x220;

/// What a device says it can report. The subset of the capability bits this decision needs.
#[derive(Debug, Default, Clone)]
pub struct Caps {
    pub has_key: bool,
    pub has_rel: bool,
    pub keys: Vec<u16>,
}

impl Caps {
    fn has(&self, key: u16) -> bool {
        self.keys.contains(&key)
    }
}

/// Whether this device is something a person plays games with.
pub fn is_gamepad(caps: &Caps) -> bool {
    if !caps.has_key {
        return false;
    }
    // A gamepad, joystick or wheel button. The System Control node fails here: it carries
    // `KEY_POWER` and `KEY_SLEEP` and no `BTN_*` at all, while still being tagged a joystick.
    if !(caps.has(BTN_SOUTH) || caps.has(BTN_TRIGGER) || caps.has(BTN_DPAD_UP)) {
        return false;
    }
    // A keyboard with media keys can carry odd button codes; escape plus a letter settles it.
    if caps.has(KEY_ESC) && caps.has(KEY_Q) {
        return false;
    }
    // A mouse or trackpad. Relative motion plus a left button is not something anyone plays
    // with, and a graphics tablet with absolute axes lands here too.
    if caps.has_rel && caps.has(BTN_LEFT) {
        return false;
    }
    true
}

/// Whether a device name passes the config's filters.
///
/// Case-insensitive substrings, because the names the kernel reports are long and inconsistent
/// -- "Microsoft X-Box 360 pad" and "Xbox Wireless Controller" are the same thing to anyone
/// writing a config file. `deny` wins, so a broad `allow` can be narrowed.
pub fn name_allowed(name: &str, allow: &[String], deny: &[String]) -> bool {
    let name = name.to_lowercase();
    let matches = |patterns: &[String]| {
        patterns
            .iter()
            .any(|pattern| name.contains(&pattern.to_lowercase()))
    };
    if matches(deny) {
        return false;
    }
    allow.is_empty() || matches(allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(has_key: bool, has_rel: bool, keys: &[u16]) -> Caps {
        Caps {
            has_key,
            has_rel,
            keys: keys.to_vec(),
        }
    }

    #[test]
    fn a_keychron_system_control_node_is_not_a_gamepad() {
        // The device this classifier exists for. `/dev/input/event15` on the machine this was
        // written on: a keyboard's power/sleep HID node, reporting KEY and ABS, which udev tags
        // `ID_INPUT_JOYSTICK=1` and gives an `-event-joystick` symlink. It has no BTN_* at all.
        const KEY_POWER: u16 = 116;
        const KEY_SLEEP: u16 = 142;
        assert!(!is_gamepad(&caps(true, false, &[KEY_POWER, KEY_SLEEP])));
    }

    #[test]
    fn an_xbox_pad_is_a_gamepad() {
        const BTN_EAST: u16 = 0x131;
        const BTN_START: u16 = 0x13b;
        assert!(is_gamepad(&caps(
            true,
            false,
            &[BTN_SOUTH, BTN_EAST, BTN_START]
        )));
    }

    #[test]
    fn a_wheel_with_only_btn_joystick_counts() {
        // Wheels and flight sticks report BTN_TRIGGER and never BTN_SOUTH.
        assert!(is_gamepad(&caps(true, false, &[BTN_TRIGGER])));
    }

    #[test]
    fn a_pad_that_reports_its_hat_as_buttons_counts() {
        const BTN_DPAD_DOWN: u16 = 0x221;
        assert!(is_gamepad(&caps(
            true,
            false,
            &[BTN_DPAD_UP, BTN_DPAD_DOWN]
        )));
    }

    #[test]
    fn a_plain_keyboard_is_not_a_gamepad() {
        assert!(!is_gamepad(&caps(true, false, &[KEY_ESC, KEY_Q])));
    }

    #[test]
    fn a_keyboard_that_also_claims_a_gamepad_button_is_still_a_keyboard() {
        assert!(!is_gamepad(&caps(
            true,
            false,
            &[KEY_ESC, KEY_Q, BTN_SOUTH]
        )));
    }

    #[test]
    fn a_mouse_with_absolute_axes_is_not_a_gamepad() {
        // Some mice and most trackpads report absolute axes alongside relative motion.
        assert!(!is_gamepad(&caps(true, true, &[BTN_LEFT, BTN_TRIGGER])));
    }

    #[test]
    fn a_device_that_reports_no_buttons_at_all_is_not_a_gamepad() {
        // Accelerometers and lid switches show up in /dev/input too.
        assert!(!is_gamepad(&caps(false, false, &[])));
    }

    #[test]
    fn an_empty_allow_list_means_every_controller() {
        assert!(name_allowed("Xbox Wireless Controller", &[], &[]));
    }

    #[test]
    fn allow_and_deny_match_case_insensitive_substrings() {
        let allow = vec!["xbox".to_string()];
        assert!(name_allowed("Microsoft X-Box 360 pad", &[], &[]));
        assert!(name_allowed("Xbox Wireless Controller", &allow, &[]));
        assert!(!name_allowed("Sony DualSense", &allow, &[]));
    }

    #[test]
    fn deny_beats_allow() {
        // So a broad allow can be narrowed without rewriting it as a list of exceptions.
        let allow = vec!["controller".to_string()];
        let deny = vec!["touchpad".to_string()];
        assert!(!name_allowed("Wireless Controller Touchpad", &allow, &deny));
    }
}
