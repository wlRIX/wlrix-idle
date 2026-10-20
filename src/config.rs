// SPDX-License-Identifier: GPL-3.0-or-later
//! The idle config file.
//!
//! ```toml
//! # ~/.config/wlrix/idle.toml
//!
//! [[timeout]]
//! after_secs = 900
//! lock = true
//! blank = true
//!
//! [lock]
//! command = "wlrix-lock"
//!
//! [lid]
//! on_battery = "suspend"
//! on_ac = "suspend"
//! docked = "nothing"
//!
//! [gamepad]
//! enable = true
//! ```
//!
//! Read from the user's config directory first, then `/etc/wlrix`; the first file found wins
//! outright rather than merging, so what a user sees in their own file is the whole of what
//! they get. Unknown keys are an error, for the same reason as everywhere else in wlRIX: a
//! silently ignored typo in a config file is a bad afternoon.
//!
//! ## Timeouts are not stages
//!
//! Each `[[timeout]]` is its own countdown from the last activity. Three of them at 600, 900
//! and 1800 do not add up to fifty minutes; they are three separate things happening ten,
//! fifteen and thirty minutes after the user last touched anything. That is how
//! `ext-idle-notify` works -- one notification object per timeout, each armed from the same
//! moment -- and pretending otherwise in the config would mean the file and the protocol
//! disagreeing about what a number means.
//!
//! ## Nothing on idle by default -- but locked before sleep
//!
//! With no config file, no timeout is configured and nothing ever fires. An idle manager that
//! invents a screen-blanking policy nobody asked for is worse than one that sits there: the
//! first is a surprise in the middle of a film, the second is a line in the log.
//!
//! Sleep is different. A laptop closed in a bag and opened on a train must not wake to an
//! unlocked desktop, so `[before_sleep] lock` is on unless turned off, with `wlrix-lock` --
//! the desktop's own locker -- as the locker unless `[lock] command` names another. And the lid
//! does what logind would have done: suspend, except while docked.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Where the config lives, relative to a config directory.
const CONFIG_NAME: &str = "wlrix/idle.toml";
/// Consulted when the user has no config of their own.
const SYSTEM_CONFIG_DIR: &str = "/etc";

/// The longest countdown that means anything. A day of untouched session is well past the
/// point where another hour would tell anyone something new.
const MAX_TIMEOUT_SECS: u64 = 86_400;
/// Below this, a `lock = true` is almost certainly a typo rather than a wish.
const SUSPICIOUS_LOCK_SECS: u64 = 30;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// What to do, and how long to wait before doing it.
    #[serde(default, rename = "timeout")]
    pub timeouts: Vec<Timeout>,
    #[serde(default)]
    pub before_sleep: BeforeSleep,
    #[serde(default)]
    pub lock: Lock,
    #[serde(default)]
    pub lid: Lid,
    #[serde(default)]
    pub gamepad: Gamepad,
    #[serde(default)]
    pub dbus: Dbus,
}

/// One countdown, and what happens at the end of it.
///
/// The three actions are not exclusive: a timeout may lock, blank and run a command, and the
/// usual "lock the screen and switch the monitors off" is exactly that.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timeout {
    #[serde(default)]
    pub after_secs: u64,
    /// Switch every monitor off.
    #[serde(default)]
    pub blank: bool,
    /// Start `[lock] command`, unless it is already running.
    #[serde(default)]
    pub lock: bool,
    /// Run through `sh -c`, so a config can use quoting and pipes the way swayidle's did.
    #[serde(default)]
    pub command: Option<String>,
    /// Run when the user comes back, to undo whatever `command` did.
    #[serde(default)]
    pub resume_command: Option<String>,
}

impl Timeout {
    /// Whether this timeout would actually do anything.
    fn does_something(&self) -> bool {
        self.blank || self.lock || self.command.is_some()
    }
}

/// What to do when the machine is about to suspend.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeforeSleep {
    /// Start the locker. On by default; see the module comment.
    #[serde(default = "enabled")]
    pub lock: bool,
    #[serde(default)]
    pub blank: bool,
    #[serde(default)]
    pub command: Option<String>,
    /// How long to hold logind's delay inhibitor while the above runs.
    #[serde(default = "default_sleep_timeout")]
    pub timeout_secs: u64,
}

impl Default for BeforeSleep {
    fn default() -> Self {
        Self {
            lock: true,
            blank: false,
            command: None,
            timeout_secs: default_sleep_timeout(),
        }
    }
}

impl BeforeSleep {
    /// Whether anything needs to happen before the machine sleeps.
    ///
    /// When nothing does, the logind inhibitor is not taken at all -- holding a delay lock to
    /// do nothing with it only makes every suspend slower.
    pub fn does_something(&self) -> bool {
        self.lock || self.blank || self.command.is_some()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lock {
    /// The locker to run: `wlrix-lock` unless another is named. An empty string means none,
    /// for a session that wants nothing to lock at all.
    #[serde(default = "default_locker")]
    pub command: Option<String>,
}

impl Default for Lock {
    fn default() -> Self {
        Self {
            command: default_locker(),
        }
    }
}

fn default_locker() -> Option<String> {
    Some("wlrix-lock".to_owned())
}

/// What closing the lid does, in three situations.
///
/// Acted on only while this daemon holds logind's `handle-lid-switch` inhibitor, which it takes
/// at startup when `[dbus] logind` is on; while it runs, logind's own `HandleLidSwitch*`
/// settings are set aside, and they come back the moment it exits. The defaults are logind's
/// own, so a file without this section changes nothing but who decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lid {
    #[serde(default = "LidAction::suspend")]
    pub on_battery: LidAction,
    #[serde(default = "LidAction::suspend")]
    pub on_ac: LidAction,
    /// A dock, or another monitor plugged in -- logind's own test, which counts displays.
    /// Nothing by default: the laptop is being used as a desktop, and the compositor switches
    /// its panel off by itself.
    #[serde(default = "LidAction::nothing")]
    pub docked: LidAction,
}

impl Default for Lid {
    fn default() -> Self {
        Self {
            on_battery: LidAction::Suspend,
            on_ac: LidAction::Suspend,
            docked: LidAction::Nothing,
        }
    }
}

impl Lid {
    /// The action for the situation the lid was closed in.
    pub fn action(&self, docked: bool, external_power: bool) -> LidAction {
        if docked {
            self.docked
        } else if external_power {
            self.on_ac
        } else {
            self.on_battery
        }
    }
}

/// One thing closing the lid can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LidAction {
    /// Sleep; `[before_sleep]` runs first, so the session locks.
    Suspend,
    /// Save to disk and power off. Where hibernation is not set up, logind refuses and the
    /// session is locked and blanked instead.
    Hibernate,
    /// Lock and switch the screen off, and stay awake.
    Lock,
    /// Switch the screen off, and stay awake.
    Blank,
    Nothing,
}

impl LidAction {
    fn suspend() -> Self {
        Self::Suspend
    }
    fn nothing() -> Self {
        Self::Nothing
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gamepad {
    #[serde(default = "enabled")]
    pub enable: bool,
    /// How far a stick has to move before it counts, as a fraction of its travel from rest to
    /// the end of the axis -- not of the axis's whole span, which for a centered stick is twice
    /// that. Triggers and d-pads rest at one end, so for them the two are the same.
    #[serde(default = "default_deadzone")]
    pub deadzone: f32,
    /// The least time between two reports from the same device.
    #[serde(default = "default_interval")]
    pub min_interval_ms: u64,
    /// Case-insensitive substrings of the device name. Empty means every controller.
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    /// Devices to use whatever the detection thinks, for the controller nobody's heuristic
    /// gets right.
    #[serde(default)]
    pub devices: Vec<PathBuf>,
}

impl Default for Gamepad {
    fn default() -> Self {
        Self {
            enable: true,
            deadzone: default_deadzone(),
            min_interval_ms: default_interval(),
            allow: Vec::new(),
            deny: Vec::new(),
            devices: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dbus {
    /// Own `org.freedesktop.ScreenSaver`.
    #[serde(default = "enabled")]
    pub screensaver: bool,
    /// Own `org.freedesktop.PowerManagement.Inhibit`.
    #[serde(default = "enabled")]
    pub power_management: bool,
    /// Take logind's delay inhibitor and act before the machine suspends.
    #[serde(default = "enabled")]
    pub logind: bool,
    /// Take the bus names from whoever already holds them.
    #[serde(default)]
    pub replace: bool,
}

impl Default for Dbus {
    fn default() -> Self {
        Self {
            screensaver: true,
            power_management: true,
            logind: true,
            replace: false,
        }
    }
}

fn enabled() -> bool {
    true
}

/// logind's `InhibitDelayMaxSec` is five seconds by default. Past that it suspends regardless,
/// so a longer wait here would only mean losing the race with the lock half-run.
fn default_sleep_timeout() -> u64 {
    4
}

/// A quarter of the way over. On a real X-Box One S pad that is 8192 counts against a driver
/// noise floor (`flat`) of 128, so it clears jitter by a factor of sixty while still being a
/// nudge rather than a shove. A d-pad's whole range is -1..1, so every press clears it.
fn default_deadzone() -> f32 {
    0.25
}

fn default_interval() -> u64 {
    1000
}

impl Config {
    /// Bring the config into a shape the rest of the program can rely on.
    ///
    /// Numbers out of range are clamped rather than refused, as everywhere else in wlRIX --
    /// refusing to start over a silly number in a config file helps nobody. Two things are
    /// dropped instead of clamped, because there is no sensible value to clamp them *to*: a
    /// zero-second countdown, which would fire immediately and then immediately again forever,
    /// and a timeout that does nothing at all.
    fn tidy(&mut self) {
        self.timeouts.retain(|timeout| {
            if timeout.after_secs == 0 {
                warn!("ignoring a timeout of 0 seconds; it would fire without pause");
                return false;
            }
            if !timeout.does_something() {
                warn!(
                    "ignoring the timeout at {} seconds; it has no blank, no lock and no command",
                    timeout.after_secs
                );
                return false;
            }
            true
        });

        for timeout in &mut self.timeouts {
            timeout.after_secs = timeout.after_secs.min(MAX_TIMEOUT_SECS);
            if timeout.lock && timeout.after_secs < SUSPICIOUS_LOCK_SECS {
                warn!(
                    "the timeout at {} seconds locks the screen; that is soon enough to be a typo",
                    timeout.after_secs
                );
            }
        }

        // Ascending, which is what lets the resume actions be unwound deepest-first without
        // sorting again at the moment the user is waiting for the screen to come back.
        self.timeouts.sort_by_key(|timeout| timeout.after_secs);

        // An empty command is how a file says "no locker".
        if self
            .lock
            .command
            .as_deref()
            .is_some_and(|command| command.trim().is_empty())
        {
            self.lock.command = None;
        }
        if self.timeouts.iter().any(|timeout| timeout.lock) && self.lock.command.is_none() {
            warn!("a timeout asks to lock, but [lock] command is not set; nothing will lock");
        }
        // Locking before sleep is on by default, so with the locker turned off it quietly is too
        // rather than warning on every start about a setting nobody wrote.
        if self.lock.command.is_none() {
            self.before_sleep.lock = false;
        }

        self.before_sleep.timeout_secs = self.before_sleep.timeout_secs.clamp(1, 20);
        self.gamepad.deadzone = self.gamepad.deadzone.clamp(0.02, 0.90);
        self.gamepad.min_interval_ms = self.gamepad.min_interval_ms.min(60_000);
    }

    /// A one-line account of what this config does, for the log after a load or a reload.
    pub fn summary(&self) -> String {
        if self.timeouts.is_empty() {
            return "no timeouts configured; nothing will happen on idle".to_string();
        }
        let steps: Vec<String> = self
            .timeouts
            .iter()
            .map(|timeout| {
                let mut what = Vec::new();
                if timeout.blank {
                    what.push("blank");
                }
                if timeout.lock {
                    what.push("lock");
                }
                if timeout.command.is_some() {
                    what.push("command");
                }
                format!("{}s: {}", timeout.after_secs, what.join("+"))
            })
            .collect();
        steps.join(", ")
    }
}

/// A config file, and where it came from.
pub struct Loaded {
    pub config: Config,
    pub source: Source,
}

/// Where the settings came from.
///
/// "No file" and "a file we could not use" both end in the defaults, but they are not the same
/// thing to whoever is reading the log: one is the ordinary case on a fresh install, the other
/// means a file they wrote is being ignored.
pub enum Source {
    /// No config file anywhere.
    None,
    /// Read and used.
    File(PathBuf),
    /// Found, but unusable. The reason has already been reported.
    Rejected(PathBuf),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Self::None => "no config file; nothing will happen on idle".to_string(),
            Self::File(path) => path.display().to_string(),
            Self::Rejected(path) => format!("{} (ignored)", path.display()),
        }
    }
}

/// Read the config, from `explicit` if given and the usual places otherwise.
///
/// A broken config is reported and then ignored rather than being fatal. This is started by the
/// session manager with nothing watching its output, and a daemon that refuses to run leaves a
/// desktop that quietly never blanks -- which nobody notices until the machine has sat awake
/// all night.
///
/// An explicitly named file that does not exist is different, and is reported as such: someone
/// who passed `--config` meant that file and wants to know it was not there.
pub fn load(explicit: Option<&Path>) -> Loaded {
    let path = match explicit {
        Some(path) => {
            if !path.is_file() {
                warn!("{} does not exist", path.display());
                return Loaded {
                    config: Config::default(),
                    source: Source::Rejected(path.to_path_buf()),
                };
            }
            path.to_path_buf()
        }
        None => match find(CONFIG_NAME) {
            Some(path) => path,
            None => {
                return Loaded {
                    config: Config::default(),
                    source: Source::None,
                };
            }
        },
    };

    let rejected = |path: PathBuf| Loaded {
        config: Config::default(),
        source: Source::Rejected(path),
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(mut config) => {
                config.tidy();
                Loaded {
                    config,
                    source: Source::File(path),
                }
            }
            Err(err) => {
                warn!("{} is not valid: {err}", path.display());
                rejected(path)
            }
        },
        Err(err) => {
            warn!("could not read {}: {err}", path.display());
            rejected(path)
        }
    }
}

/// Parse a candidate config file, for `--check-config`.
///
/// This program's own serde types are the authority on what its config file may contain.
/// `wlrix-settings-daemon` writes a temporary file and runs this against it before renaming it
/// into place, so a settings app cannot produce an `idle.toml` this program would refuse --
/// which matters because `deny_unknown_fields` means one wrong key costs the *whole* file, and
/// an idle manager that fell back to defaults is one that silently never blanks.
///
/// Deliberately not [`load`]: that reports through `warn!` and carries on with defaults, which
/// is right at startup and exactly wrong here, where the question *is* whether the file is
/// acceptable. `tidy` is not run either -- clamping is what this program does with a value it
/// has accepted, not part of accepting it.
pub fn check(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    toml::from_str::<Config>(&text)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// The first file of this name that exists: the user's, then the system's.
fn find(name: &str) -> Option<PathBuf> {
    config_dirs()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Directories to look in, most specific first.
fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = user_config_dir() {
        dirs.push(dir);
    }
    dirs.push(Path::new(SYSTEM_CONFIG_DIR).to_path_buf());
    dirs
}

/// `$XDG_CONFIG_HOME`, or `~/.config` as the spec says to assume.
fn user_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        let mut config: Config = toml::from_str(text).expect("should parse");
        config.tidy();
        config
    }

    #[test]
    fn an_empty_file_never_blanks_but_locks_before_sleep() {
        let config = parse("");
        assert!(config.timeouts.is_empty());
        assert!(
            config.before_sleep.lock,
            "the session must not wake unlocked"
        );
        assert_eq!(config.lock.command.as_deref(), Some("wlrix-lock"));
        assert_eq!(config.lid, Lid::default());
        assert!(config.gamepad.enable, "controllers are watched by default");
    }

    #[test]
    fn no_locker_means_nothing_locks_before_sleep() {
        let config = parse("[lock]\ncommand = \"\"\n");
        assert_eq!(config.lock.command, None);
        assert!(!config.before_sleep.does_something());
    }

    #[test]
    fn the_lid_action_follows_the_situation() {
        let lid =
            parse("[lid]\non_battery = \"hibernate\"\non_ac = \"lock\"\ndocked = \"blank\"\n").lid;
        assert_eq!(lid.action(false, false), LidAction::Hibernate);
        assert_eq!(lid.action(false, true), LidAction::Lock);
        // Docked wins whatever the power: a docked laptop is nearly always plugged in.
        assert_eq!(lid.action(true, true), LidAction::Blank);
        let default = Lid::default();
        assert_eq!(default.action(false, false), LidAction::Suspend);
        assert_eq!(default.action(true, false), LidAction::Nothing);
        assert!(toml::from_str::<Config>("[lid]\non_ac = \"shutdown\"\n").is_err());
    }

    #[test]
    fn the_setup_this_replaces_round_trips() {
        // `swayidle -w timeout 900 'wlopm --off *' resume 'wlopm --on *'`, which is what the
        // session ran before this program existed. The switchover has to be behavior
        // preserving or it is not a switchover.
        let config = parse("[[timeout]]\nafter_secs = 900\nblank = true\n");
        assert_eq!(config.timeouts.len(), 1);
        assert_eq!(config.timeouts[0].after_secs, 900);
        assert!(config.timeouts[0].blank);
        assert!(!config.timeouts[0].lock);
    }

    #[test]
    fn timeouts_are_sorted_by_when_they_fire() {
        // The resume actions are unwound in reverse of this order, so the ordering is not a
        // tidiness question -- it is what decides which undo runs first.
        let config = parse(
            "[[timeout]]\nafter_secs = 1800\nblank = true\n\
             \n\
             [[timeout]]\nafter_secs = 600\ncommand = \"dim\"\n\
             \n\
             [[timeout]]\nafter_secs = 900\ncommand = \"lock\"\n",
        );
        let order: Vec<u64> = config
            .timeouts
            .iter()
            .map(|timeout| timeout.after_secs)
            .collect();
        assert_eq!(order, [600, 900, 1800]);
    }

    #[test]
    fn a_zero_second_timeout_is_dropped_and_the_rest_are_kept() {
        // There is no value to clamp zero *to*: it would fire the moment it was armed, and
        // then again the moment it was re-armed, forever.
        let config = parse(
            "[[timeout]]\nafter_secs = 0\nblank = true\n\
             \n\
             [[timeout]]\nafter_secs = 900\nblank = true\n",
        );
        assert_eq!(config.timeouts.len(), 1);
        assert_eq!(config.timeouts[0].after_secs, 900);
    }

    #[test]
    fn a_timeout_that_does_nothing_is_dropped() {
        let config = parse("[[timeout]]\nafter_secs = 900\nresume_command = \"undim\"\n");
        assert!(
            config.timeouts.is_empty(),
            "a resume with nothing to resume from is not a timeout"
        );
    }

    #[test]
    fn a_countdown_longer_than_a_day_is_clamped_not_rejected() {
        let config = parse("[[timeout]]\nafter_secs = 999999999\nblank = true\n");
        assert_eq!(config.timeouts[0].after_secs, MAX_TIMEOUT_SECS);
    }

    #[test]
    fn a_deadzone_outside_the_useful_range_is_clamped_not_rejected() {
        // A deadzone of 0 makes every stick permanently active; one of 1 makes no stick ever
        // register. Both are configs that cannot be honored, and neither is worth refusing to
        // start over.
        assert_eq!(parse("[gamepad]\ndeadzone = 0.0\n").gamepad.deadzone, 0.02);
        assert_eq!(parse("[gamepad]\ndeadzone = 5.0\n").gamepad.deadzone, 0.90);
    }

    #[test]
    fn a_sleep_timeout_past_loginds_patience_is_clamped() {
        assert_eq!(
            parse("[before_sleep]\nlock = true\ntimeout_secs = 600\n")
                .before_sleep
                .timeout_secs,
            20
        );
    }

    #[test]
    fn an_unknown_key_is_an_error() {
        // The case this is really here for is a plural, which is exactly what someone writes
        // when they have several of something.
        assert!(toml::from_str::<Config>("[[timeouts]]\nafter_secs = 900\n").is_err());
        assert!(toml::from_str::<Config>("[[timeout]]\nafter_sec = 900\n").is_err());
        assert!(toml::from_str::<Config>("[gamepad]\ndeadzone_pct = 25\n").is_err());
    }

    #[test]
    fn before_sleep_can_be_told_to_do_nothing() {
        assert!(BeforeSleep::default().does_something());
        assert!(
            !parse("[before_sleep]\nlock = false\n")
                .before_sleep
                .does_something()
        );
        assert!(
            parse("[before_sleep]\nlock = true\n")
                .before_sleep
                .does_something()
        );
        assert!(
            parse("[before_sleep]\ncommand = \"sync\"\n")
                .before_sleep
                .does_something()
        );
    }

    #[test]
    fn the_summary_says_what_each_countdown_does() {
        let config = parse(
            "[[timeout]]\nafter_secs = 600\ncommand = \"dim\"\n\
             \n\
             [[timeout]]\nafter_secs = 900\nlock = true\nblank = true\n",
        );
        assert_eq!(config.summary(), "600s: command, 900s: blank+lock");
    }
}
