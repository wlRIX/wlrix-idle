// SPDX-License-Identifier: GPL-3.0-or-later
//! Controllers, read straight from evdev.
//!
//! This is the reason the program exists. libinput classifies a gamepad as a joystick and
//! ignores it, so the compositor's input path never sees a stick move -- which means
//! `ext-idle-notify` does not either, and a session spent playing something with a pad looks
//! from the outside exactly like a session nobody is sitting at. The screen blanks mid-game.
//!
//! ## Activity, not an inhibitor
//!
//! A controller counts as the user doing something, not as something holding the countdown
//! off. An inhibitor would get the timeout edge roughly right by accident -- the hold ends,
//! and it blanks eventually either way -- but it gets the waking edge flatly wrong: with the
//! screen already off, an inhibitor only stops counting, it does not turn the monitors back on.
//! Pressing a button should wake the screen, which is what treating it as activity does.
//!
//! The case this misses is sitting through a long cutscene with the pad in your hands and never
//! touching it. That is not a controller problem, it is an "the application knows it is busy"
//! problem, and games already answer it by asking `org.freedesktop.ScreenSaver` not to blank.
//!
//! ## Permissions
//!
//! Nothing is needed. udev's `70-uaccess.rules` gives the logged-in user an ACL on
//! joystick-tagged nodes and on nothing else, so a session daemon can read a controller and
//! **cannot** read a keyboard or a mouse even if the classification below were wrong.
//!
//! One consequence worth knowing: the ACL is dropped when the session goes inactive, on a VT
//! switch. ACLs are only checked at `open`, so devices already open keep working -- which is
//! why nothing here ever reopens a device it already has, and why a rescan hitting `EACCES` is
//! reported as "not ours right now" rather than as a failure.

mod scan;
mod watch;

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use calloop::{
    Interest, Mode, PostAction, RegistrationToken,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use evdev::{Device, EventType};

use crate::config;
use crate::idle::Idle;

use scan::Caps;
use watch::{DEVICE_DIR, Watch};

/// How long to wait before rescanning after a device node appears.
///
/// The node exists before udev has given it an ACL, so an immediate open gets `EACCES`. The
/// `ATTRIB` from udev's `setfacl` normally covers this; the timer is the belt to that pair of
/// braces, for the case where the two arrive in an order inotify coalesces away.
const SETTLE: Duration = Duration::from_millis(300);

/// A controller we are watching.
pub struct Pad {
    device: Device,
    path: PathBuf,
    name: String,
    motion: Motion,
    min_interval: Duration,
    last_report: Option<Instant>,
}

impl AsFd for Pad {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.device.as_fd()
    }
}

/// Per-axis state, for telling a deliberate push from a stick sitting still.
///
/// An analog stick at rest is never quite at rest: it reports a slow trickle of values a count
/// or two either side of center, forever. Taking those at face value would mean a controller
/// left on a desk kept the session awake indefinitely, which is worse than not watching it.
#[derive(Debug, Default)]
struct Motion {
    /// The last value that counted as movement, per axis.
    baseline: HashMap<u16, i32>,
    /// How far from the baseline is far enough, per axis.
    threshold: HashMap<u16, i32>,
}

/// How far an axis has to move to count, in the units that axis reports.
///
/// `deadzone` is a fraction of the distance from the resting point to the end of the axis, not
/// of the axis's whole span. Those differ by a factor of two for a stick, which rests in the
/// middle and swings both ways: a quarter of the *span* of a -32768..32767 stick would be half
/// its travel, so waking the screen would mean shoving the stick halfway over rather than
/// nudging it. Triggers and hats rest at one end and are unaffected either way.
///
/// `flat` is the driver's own idea of the dead zone, and is respected as a floor: a device that
/// says its center is noisy up to 128 counts knows something the config file does not.
fn threshold_for(minimum: i32, maximum: i32, flat: i32, deadzone: f32) -> i32 {
    let span = (maximum - minimum).max(0) as f32;
    // Symmetric about zero means it rests in the middle, so the travel that matters is half.
    let travel = if minimum < 0 { span / 2.0 } else { span };
    let fraction = (travel * deadzone).ceil() as i32;
    flat.max(fraction).max(1)
}

impl Motion {
    /// Record what an axis's range is, so its threshold can be a fraction of it rather than a
    /// raw count that means different things on different hardware.
    fn learn(&mut self, axis: u16, minimum: i32, maximum: i32, flat: i32, deadzone: f32) {
        self.threshold
            .insert(axis, threshold_for(minimum, maximum, flat, deadzone));
    }

    /// Whether this axis reading is the user doing something.
    fn is_activity(&mut self, axis: u16, value: i32) -> bool {
        let threshold = *self.threshold.get(&axis).unwrap_or(&1);
        let Some(baseline) = self.baseline.get(&axis).copied() else {
            // First sight of this axis. Whatever it reads now is where it is resting, not a
            // move -- a pad plugged in with a trigger held would otherwise look like input.
            self.baseline.insert(axis, value);
            return false;
        };
        if (value - baseline).abs() < threshold {
            return false;
        }
        // The baseline follows only the readings that counted, so wandering inside the dead
        // zone never drags it along. Updating it on every reading would mean measuring the push
        // that finally crosses the threshold from wherever the noise had left the baseline,
        // rather than from where the stick actually rests.
        self.baseline.insert(axis, value);
        true
    }
}

impl Pad {
    /// Read everything waiting and say whether any of it was the user.
    fn poll(&mut self) -> std::io::Result<bool> {
        let Pad { device, motion, .. } = self;
        let mut activity = false;
        loop {
            let events = match device.fetch_events() {
                Ok(events) => events,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(err) => return Err(err),
            };
            let mut any = false;
            for event in events {
                any = true;
                match event.event_type() {
                    // A press. Releases are skipped, or every button would count twice, and
                    // autorepeat (value 2) is skipped, or a button wedged under something would
                    // hold the session awake for as long as it stayed there.
                    EventType::KEY if event.value() == 1 => activity = true,
                    EventType::ABSOLUTE if motion.is_activity(event.code(), event.value()) => {
                        activity = true;
                    }
                    // SYN, MSC, LED, FF and the rest are the device talking to itself.
                    _ => {}
                }
            }
            if !any {
                break;
            }
        }
        Ok(activity)
    }

    /// Whether enough time has passed to bother the rest of the program again.
    ///
    /// A controller in use reports continuously; without this, holding a stick over would mean
    /// tearing down and recreating every idle notification hundreds of times a second, to say
    /// something the first one already said.
    fn may_report(&mut self) -> bool {
        let now = Instant::now();
        if let Some(last) = self.last_report
            && now.duration_since(last) < self.min_interval
        {
            return false;
        }
        self.last_report = Some(now);
        true
    }
}

/// What is currently being watched.
#[derive(Default)]
pub struct Pads {
    devices: Vec<(PathBuf, RegistrationToken)>,
    watch: Option<RegistrationToken>,
    /// A rescan waiting for udev to finish with a device that just appeared.
    settling: Option<RegistrationToken>,
}

/// Begin watching controllers, if the config asks for it.
pub fn start(idle: &mut Idle) {
    if !idle.config.gamepad.enable {
        info!("not watching controllers ([gamepad] enable is false)");
        return;
    }

    match Watch::new() {
        Ok(watch) => {
            let source = Generic::new(watch, Interest::READ, Mode::Level);
            match idle.loop_handle().insert_source(source, |_, watch, idle| {
                // SAFETY: `get_mut` only forbids dropping the inner I/O source, and draining
                // the watch reads from its fd without taking it.
                let watch = unsafe { watch.get_mut() };
                if watch.drain() {
                    rescan(idle);
                    schedule_settle(idle);
                }
                Ok(PostAction::Continue)
            }) {
                Ok(token) => idle.pads.watch = Some(token),
                Err(err) => warn!("could not watch {DEVICE_DIR} from the loop: {err}"),
            }
        }
        // Not fatal: the devices present at startup are still picked up below, and a machine
        // where inotify is unavailable is a stranger problem than a controller plugged in late.
        Err(err) => warn!("could not watch {DEVICE_DIR} ({err}); hot-plug will not be noticed"),
    }

    rescan(idle);
}

/// Stop watching everything.
pub fn stop(idle: &mut Idle) {
    for (_, token) in std::mem::take(&mut idle.pads.devices) {
        idle.loop_handle().remove(token);
    }
    if let Some(token) = idle.pads.watch.take() {
        idle.loop_handle().remove(token);
    }
    if let Some(token) = idle.pads.settling.take() {
        idle.loop_handle().remove(token);
    }
}

/// Apply a changed `[gamepad]` section: the filters, the deadzone and the interval are all baked
/// into open devices, so the only honest way to change them is to start again.
pub fn restart(idle: &mut Idle) {
    stop(idle);
    start(idle);
}

/// Look at `/dev/input` and bring the watched set into line with it.
fn rescan(idle: &mut Idle) {
    if !idle.config.gamepad.enable {
        return;
    }

    // Devices that have gone away. The read callback removes one it finds unplugged mid-read;
    // this covers the rest, including a node that vanished while nothing was being sent.
    let gone: Vec<PathBuf> = idle
        .pads
        .devices
        .iter()
        .filter(|(path, _)| !path.exists())
        .map(|(path, _)| path.clone())
        .collect();
    for path in gone {
        forget(idle, &path);
    }

    for path in candidates(&idle.config.gamepad) {
        if idle
            .pads
            .devices
            .iter()
            .any(|(known, _)| known.as_path() == path)
        {
            continue;
        }
        // Explicitly listed devices skip the classification entirely -- that is what the list
        // is for -- but still have to open.
        let forced = idle.config.gamepad.devices.contains(&path);
        if let Some(pad) = open(&path, &idle.config.gamepad, forced) {
            adopt(idle, pad);
        }
    }
}

/// Every device node worth looking at: those in `/dev/input`, plus anything the config named
/// explicitly (which may live somewhere else entirely, such as a `by-id` symlink).
fn candidates(config: &config::Gamepad) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    match std::fs::read_dir(DEVICE_DIR) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("event"))
                {
                    paths.push(path);
                }
            }
        }
        Err(err) => warn!("could not read {DEVICE_DIR}: {err}"),
    }
    // Sorted so the log reads the same way twice, and so `--list-devices` is not a lottery. By
    // the number rather than the name, or `event10` sorts before `event2` and reading the list
    // against `ls /dev/input` becomes an exercise.
    paths.sort_by_key(|path| (event_number(path), path.clone()));
    for path in &config.devices {
        if !paths.contains(path) {
            paths.push(path.clone());
        }
    }
    paths
}

/// The number in `eventN`, for sorting. Anything else sorts last, by name.
fn event_number(path: &Path) -> u32 {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("event"))
        .and_then(|number| number.parse().ok())
        .unwrap_or(u32::MAX)
}

/// Open a device and decide whether to keep it.
fn open(path: &Path, config: &config::Gamepad, forced: bool) -> Option<Pad> {
    let device = match Device::open(path) {
        Ok(device) => device,
        // The ordinary case for every keyboard and mouse in the directory: uaccess gave us no
        // ACL on them. Not worth a word -- see the module docs.
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            if forced {
                warn!(
                    "{} was named in [gamepad] devices but cannot be read: {err}",
                    path.display()
                );
            }
            return None;
        }
        Err(err) => {
            if forced {
                warn!("could not open {}: {err}", path.display());
            }
            return None;
        }
    };

    let name = device.name().unwrap_or("unnamed device").to_string();
    if !forced {
        if !scan::is_gamepad(&caps_of(&device)) {
            return None;
        }
        if !scan::name_allowed(&name, &config.allow, &config.deny) {
            info!("ignoring {name}, which [gamepad] allow/deny rules out");
            return None;
        }
    }

    if let Err(err) = device.set_nonblocking(true) {
        warn!("could not set {name} non-blocking: {err}");
        return None;
    }

    let mut motion = Motion::default();
    match device.get_absinfo() {
        Ok(axes) => {
            for (axis, info) in axes {
                motion.learn(
                    axis.0,
                    info.minimum(),
                    info.maximum(),
                    info.flat(),
                    config.deadzone,
                );
            }
        }
        // Pads that report their hat as buttons have no absolute axes at all, and this is also
        // what a device with a driver quirk looks like. Buttons still work either way.
        Err(err) => info!("{name} reports no axis ranges ({err}); buttons only"),
    }

    Some(Pad {
        device,
        path: path.to_path_buf(),
        name,
        motion,
        min_interval: Duration::from_millis(config.min_interval_ms),
        last_report: None,
    })
}

/// Put a pad on the event loop.
fn adopt(idle: &mut Idle, pad: Pad) {
    let path = pad.path.clone();
    let name = pad.name.clone();
    let source = Generic::new(pad, Interest::READ, Mode::Level);
    let token = idle.loop_handle().insert_source(source, |_, pad, idle| {
        // SAFETY: `get_mut` only forbids dropping the inner I/O source. Reading events and
        // updating the debounce state leave the device -- and so its fd -- in place; the source
        // is removed by returning `PostAction::Remove`, which is calloop's own business.
        let pad = unsafe { pad.get_mut() };
        match pad.poll() {
            Ok(true) => {
                if pad.may_report() {
                    idle.activity(&format!("controller: {}", pad.name));
                }
            }
            Ok(false) => {}
            Err(err) => {
                // Unplugged mid-read, which is the ordinary way a controller goes away.
                info!("stopped watching {} ({err})", pad.name);
                idle.pads.devices.retain(|(known, _)| known != &pad.path);
                return Ok(PostAction::Remove);
            }
        }
        Ok(PostAction::Continue)
    });

    match token {
        Ok(token) => {
            info!("watching controller: {name}");
            idle.pads.devices.push((path, token));
        }
        Err(err) => warn!("could not watch {name} from the loop: {err}"),
    }
}

/// Drop a device we are no longer interested in.
fn forget(idle: &mut Idle, path: &Path) {
    if let Some(position) = idle
        .pads
        .devices
        .iter()
        .position(|(known, _)| known.as_path() == path)
    {
        let (_, token) = idle.pads.devices.remove(position);
        idle.loop_handle().remove(token);
        info!("stopped watching {}", path.display());
    }
}

/// Rescan again shortly, for the device that appeared before udev finished with it.
fn schedule_settle(idle: &mut Idle) {
    if idle.pads.settling.is_some() {
        return;
    }
    let token =
        idle.loop_handle()
            .insert_source(Timer::from_duration(SETTLE), |_, _, idle: &mut Idle| {
                idle.pads.settling = None;
                rescan(idle);
                TimeoutAction::Drop
            });
    match token {
        Ok(token) => idle.pads.settling = Some(token),
        Err(err) => warn!("could not schedule a rescan: {err}"),
    }
}

/// The capability bits the classification looks at.
fn caps_of(device: &Device) -> Caps {
    let events = device.supported_events();
    Caps {
        has_key: events.contains(EventType::KEY),
        has_rel: events.contains(EventType::RELATIVE),
        keys: device
            .supported_keys()
            .map(|keys| keys.iter().map(|key| key.0).collect())
            .unwrap_or_default(),
    }
}

/// `--list-devices`: say what is there and what is made of it.
///
/// Read-only and needs no compositor, because this is what someone runs when a controller is
/// not waking the screen -- it has to work when nothing else does. Devices that cannot be
/// opened are listed too: seeing every keyboard in the machine reported as unreadable is how
/// you know the permissions are as they should be.
pub fn list(config: &config::Gamepad) {
    if !config.enable {
        println!("[gamepad] enable is false; no controllers would be watched");
    }
    for path in candidates(config) {
        let forced = config.devices.contains(&path);
        match Device::open(&path) {
            Ok(device) => {
                let name = device.name().unwrap_or("unnamed device");
                let verdict = if forced {
                    "controller (named in [gamepad] devices)"
                } else if !scan::is_gamepad(&caps_of(&device)) {
                    "not a controller"
                } else if !scan::name_allowed(name, &config.allow, &config.deny) {
                    "controller, but ruled out by allow/deny"
                } else {
                    "controller"
                };
                println!("{}: {name} -- {verdict}", path.display());
                if verdict.starts_with("controller") {
                    print_axes(&device, config);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                println!("{}: not readable -- not a controller", path.display());
            }
            Err(err) => println!("{}: could not open ({err})", path.display()),
        }
    }
}

/// Each axis's range and how far it has to move to count.
///
/// The other half of "my controller does not wake the screen": knowing the device was found is
/// only useful next to knowing what it would take to register. A threshold that reads as an
/// implausibly large number against the range beside it is the answer, and no amount of
/// pressing buttons would have revealed it.
fn print_axes(device: &Device, config: &config::Gamepad) {
    let Ok(axes) = device.get_absinfo() else {
        println!("    no axes (buttons only)");
        return;
    };
    for (axis, info) in axes {
        let threshold = threshold_for(info.minimum(), info.maximum(), info.flat(), config.deadzone);
        println!(
            "    axis {:#06x}: {}..{} (flat {}), moves by {} to count",
            axis.0,
            info.minimum(),
            info.maximum(),
            info.flat(),
            threshold,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stick with the usual signed 16-bit range and no driver dead zone.
    fn stick() -> Motion {
        let mut motion = Motion::default();
        motion.learn(0, -32768, 32767, 0, 0.25);
        motion
    }

    #[test]
    fn the_first_reading_from_an_axis_only_sets_the_baseline() {
        // A pad plugged in with a trigger already held would otherwise look like someone
        // pressing it.
        let mut motion = stick();
        assert!(!motion.is_activity(0, 20000));
    }

    #[test]
    fn a_stick_resting_at_its_baseline_is_not_activity() {
        let mut motion = stick();
        motion.is_activity(0, 0);
        for jitter in [1, -2, 3, -1, 0, 2] {
            assert!(
                !motion.is_activity(0, jitter),
                "{jitter} is noise, not a push"
            );
        }
    }

    #[test]
    fn a_stick_pushed_past_the_deadzone_is_activity() {
        let mut motion = stick();
        motion.is_activity(0, 0);
        // A quarter of the *travel* of a -32768..32767 stick is 8192.
        assert!(motion.is_activity(0, 9000));
    }

    #[test]
    fn a_stick_is_measured_against_its_travel_not_its_whole_span() {
        // A centred stick swings both ways, so its span is twice the distance from rest to the
        // end. Measuring the deadzone against the span would mean a quarter-deadzone needing
        // the stick shoved *half way over* to wake the screen. Measured on a real X-Box One S
        // pad, which reports -32768..32767: 8192, not 16384.
        assert_eq!(threshold_for(-32768, 32767, 0, 0.25), 8192);
        // A trigger rests at one end, so its span is its travel and nothing changes: the same
        // pad reports 0..1023 for a trigger.
        assert_eq!(threshold_for(0, 1023, 0, 0.25), 256);
    }

    #[test]
    fn jitter_around_a_resting_point_never_accumulates_however_long_it_goes_on() {
        // The baseline follows only moves that counted, so it does not creep. If it followed
        // every reading instead, a stick wandering inside its dead zone would drag the baseline
        // with it, and the reading that finally crossed the threshold would be measured from
        // wherever the wandering had left it rather than from where the stick actually rests.
        let mut motion = stick();
        motion.is_activity(0, 0);
        for step in 0..10_000 {
            // Well inside the 8192 threshold, and never settling.
            let jitter = (step % 200) - 100;
            assert!(
                !motion.is_activity(0, jitter),
                "{jitter} is noise, not a push"
            );
        }
        // And a real push is still measured from rest, not from the last bit of noise.
        assert!(motion.is_activity(0, 8192));
    }

    #[test]
    fn a_deliberate_walk_across_the_axis_does_count() {
        // The other side of the above, and the reason it is jitter rather than distance that is
        // filtered: a stick pushed slowly but all the way over is somebody pushing it.
        let mut motion = stick();
        motion.is_activity(0, 0);
        let counted = (1..2000)
            .filter(|step| motion.is_activity(0, step * 8))
            .count();
        assert!(counted > 0, "a 16000-count movement is a movement");
    }

    #[test]
    fn a_drivers_own_deadzone_wins_when_it_is_wider() {
        // `flat` is what the driver says about its own noise floor, and it knows the hardware.
        let mut motion = Motion::default();
        motion.learn(0, -32768, 32767, 30000, 0.25);
        motion.is_activity(0, 0);
        assert!(!motion.is_activity(0, 20000));
        assert!(motion.is_activity(0, 31000));
    }

    #[test]
    fn a_hat_reports_every_press_because_its_range_is_tiny() {
        // D-pads report -1, 0, 1. A quarter of that range rounds up to 1, so every press
        // clears it and no press is ever swallowed.
        let mut motion = Motion::default();
        motion.learn(0x10, -1, 1, 0, 0.25);
        motion.is_activity(0x10, 0);
        assert!(motion.is_activity(0x10, 1));
        assert!(motion.is_activity(0x10, 0));
    }

    #[test]
    fn an_axis_with_no_range_at_all_still_reports_a_change() {
        // A threshold of zero would make every reading activity, including the resting trickle.
        let mut motion = Motion::default();
        motion.learn(0, 0, 0, 0, 0.25);
        motion.is_activity(0, 0);
        assert!(!motion.is_activity(0, 0));
        assert!(motion.is_activity(0, 1));
    }
}
