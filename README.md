# wlrix-idle

The wlRIX idle manager. Watches how long the session has gone untouched, and blanks the screen, runs a locker or takes
whatever other action the config asks for — while listening to everything that has a right to say "not now".

- **Language:** Rust
- **License:** GPL-3.0-or-later
- **Reference:** [swayidle](https://github.com/swaywm/swayidle), [cosmic-idle](https://github.com/pop-os/cosmic-idle)

Started by `wlrix-session` as part of the default session, so on a normal wlRIX install there is nothing to set up. It
replaces the `swayidle` + `wlopm` pairing, and does three things neither of them can:

- **Serves `org.freedesktop.ScreenSaver`.** Firefox, mpv and Steam ask the session not to blank while they are playing
  something. Without a listener that request goes nowhere and the screen blanks anyway.
- **Notices a controller.** libinput classifies a gamepad as a joystick and drops it, so the compositor's input path —
  and therefore `ext-idle-notify` — never sees a stick move. This reads controllers directly and puts that input back.
- **Locks before suspend.** Takes a logind delay inhibitor, so the locker has actually finished before the machine
  sleeps. On by default, with `wlrix-lock`: a laptop must never wake to an unlocked desktop.
- **Decides what the lid does.** Takes logind's `handle-lid-switch` inhibitor and acts on `[lid]` instead -- per user,
  without editing `/etc/systemd/logind.conf` -- with separate actions on battery, plugged in, and docked.

## Build

```sh
cargo build
cargo run
```

## Configuration

Read from `$XDG_CONFIG_HOME/wlrix/idle.toml` (or `~/.config/wlrix/idle.toml`), falling back to `/etc/wlrix/idle.toml`.
The first file found is used whole rather than merged, so your own file is all of what you get. Unknown keys are an
error: a silently ignored typo in a config file is a bad afternoon.

Without a config file nothing happens on idle — an idle manager that invents a screen-blanking policy nobody asked
for is worse than one that sits there. Power Settings (`wlrix-settings-power`) has a When Idle group that sets the two
profiles.

```toml
# Two profiles: plugged in (and desktops, which always are), and on battery, picked by UPower's
# OnBattery and switched as the plug is pulled or put back. 0 is never. Without [on_battery],
# the plugged-in profile applies on battery too.
[plugged_in]
dim_after_secs = 600          # fade the screens dark; on a laptop, the backlight to a third
screen_off_after_secs = 900   # switch every monitor off...
lock = true                   # ...and lock as they go
suspend_after_secs = 0        # never sleep

[on_battery]
dim_after_secs = 180
screen_off_after_secs = 300
lock = true
suspend_after_secs = 600

# Anything else, as its own countdown from the last activity -- run under both profiles. Each
# [[timeout]] is independent, not a stage in a sequence: two at 600 and 900 both count from the
# same moment.
[[timeout]]
after_secs = 1200
command = "notify-send 'Still there?'"   # run when the countdown ends
resume_command = "true"                  # run when the user comes back
# Also: dim, blank, lock and suspend, as booleans, like the profiles' actions.

# What to do when the machine is about to suspend. A logind delay inhibitor is held while
# this runs, so it finishes before the machine actually sleeps.
[before_sleep]
lock = true           # the default: never wake unlocked
blank = false
command = ""
timeout_secs = 4      # logind's InhibitDelayMaxSec is 5 by default; stay under it

[lock]
command = "wlrix-lock"   # the default; "" for no locker. Stays running until unlocked; do not use a
                         # locker that forks

# Closing the lid: suspend, hibernate, lock (lock and blank, stay awake), blank, or nothing.
[lid]
on_battery = "suspend"
on_ac = "suspend"
docked = "nothing"       # a dock or another monitor; the compositor switches the panel off itself

# Controllers. libinput treats a gamepad as a joystick and ignores it, so the compositor
# never sees a stick move -- this is what puts that input back.
[gamepad]
enable = true
deadzone = 0.25       # how far a stick must move, as a fraction of its travel from rest
min_interval_ms = 1000
allow = []            # case-insensitive substrings of the device name; empty means "any gamepad"
deny = []
devices = []          # explicit paths, bypassing detection entirely

[dbus]
screensaver = true       # own org.freedesktop.ScreenSaver
power_management = true  # own org.freedesktop.PowerManagement.Inhibit
logind = true            # take the before-sleep delay inhibitor and the lid
replace = false          # take the bus names from whoever already holds them
```

### Profiles, dimming and sleep

**Which profile** is UPower's `OnBattery`, read at startup and watched; a desktop, or a machine with no UPower, is
plugged in. Pulling the plug or putting it back counts as activity: whatever has fired is undone, and every countdown
starts again under the other profile. A dim that would come at or after the screen-off or sleep time is left out.

**Dimming** is a `wlr-layer-shell` overlay on every monitor: one black pixel with some alpha, stretched by
`wp_viewporter`, faded in over about a second, with an empty input region so nothing is clicked through it. It works on
any monitor and does not touch the gamma ramp a night-light tool may hold. On a machine with a panel backlight it also
takes the backlight to a third through logind (`Session.SetBrightness`, as `wlrix-osd`'s keys do), and puts it back on
the next input -- unless the brightness keys moved it meanwhile. Undimming is instant.

**Sleep** asks logind to suspend; `[before_sleep]` locks on the way down. Woken with nobody there -- a bump in a bag --
it sleeps again after the same wait. An application inhibiting the screensaver (a video playing) holds sleep off as it
holds off the rest.

### The lid

While `wlrix-idle` runs it holds logind's `handle-lid-switch` *block* inhibitor, so logind's own `HandleLidSwitch*`
settings are set aside and `[lid]` decides. The defaults are logind's: suspend on battery and plugged in, nothing when
docked. *Docked* is logind's own test -- a docking station, or more than one display connected -- read at the moment the
lid shuts, as is whether the machine is on external power. When the process exits, crashed or not, the inhibitor's
descriptor closes and logind is back in charge.

- **suspend** and **hibernate** ask logind, so `[before_sleep]` runs first and the session locks. If logind refuses --
  hibernation not set up, or polkit saying no -- the session is locked and blanked instead.
- **lock** locks and switches the screen off, and the machine stays awake: for a download that has to finish.
- **blank** only switches the screen off.

Opening the lid counts as activity: the screen comes back, and the idle countdowns start again.

With a monitor plugged in and the lid shut, the compositor switches the laptop's panel off by itself and moves its
windows to the monitors left (`wlrix-compositor/src/lid.rs`); `docked = "nothing"` leaves the rest of the session
running on them.

Numbers outside a sensible range are clamped rather than refused. Two things are dropped with a warning instead: a
timeout of zero seconds, which would fire instantly and forever, and a timeout that has no `blank`, no `lock` and no
`command`, which is a countdown that does nothing.

## Options

| Option            | Meaning                                                                       |
|-------------------|-------------------------------------------------------------------------------|
| `--config <path>` | Use this config file instead of searching the usual places.                   |
| `--list-devices`  | List the input devices found, say which are treated as controllers, and exit. |
| `--replace`       | Take `org.freedesktop.ScreenSaver` and friends from whoever holds them.       |
| `--no-dbus`       | Do not touch D-Bus at all: no inhibit interfaces, no before-sleep or lid.     |
| `-h`, `--help`    | Print usage and exit.                                                         |

## Reloading

Edit the config and send `SIGHUP`:

```sh
kill -HUP $(cat "$XDG_RUNTIME_DIR/wlrix-idle.pid")
```

Timeouts, the lock command, before-sleep and the whole `[gamepad]` section reload. Every countdown restarts from zero.

`[dbus]` does **not** reload — a message says so. Dropping and retaking a bus name would silently void every inhibit
applications are currently holding, and open a window for another daemon to grab `org.freedesktop.ScreenSaver`. A reload
must never leave things worse than not reloading; restart the daemon for that.

## Controllers

Gamepads are read straight from `/dev/input`, and need no privileges: udev's `70-uaccess.rules` gives the logged-in user
an ACL on joystick nodes and on nothing else. That also means `wlrix-idle` structurally cannot read a keyboard or a
mouse, even if its device detection were wrong.

`--list-devices` also prints each controller's axes and how far each has to move to register, which is the other half of
diagnosing a pad that does not wake the screen:

```
/dev/input/event258: Microsoft X-Box One S pad -- controller
    axis 0x0000: -32768..32767 (flat 128), moves by 8192 to count
    axis 0x0002: 0..1023 (flat 0), moves by 256 to count
    axis 0x0010: -1..1 (flat 0), moves by 1 to count
```

Detection is by capability bits — a device counts as a controller if it has gamepad or joystick buttons, and looks like
neither a keyboard nor a pointer. Deliberately not udev's `ID_INPUT_JOYSTICK`, which is wrong often enough to matter:
the power/sleep node some keyboards expose carries that tag. Run `wlrix-idle --list-devices` to see what is picked up,
and use `[gamepad] allow` / `deny` / `devices` for anything the heuristic gets wrong.

A controller counts as *activity*, not as an inhibitor: pressing a button restarts the countdown and switches the
screens back on. Sitting through a long cutscene without touching the pad is a different problem, and one that games
already solve by asking `org.freedesktop.ScreenSaver` not to blank.

## Logs

Nothing is written to a file. `wlrix-session` already redirects the output of everything it starts into
`wlrix-session.log`, so a second file would only duplicate it. Started by hand, messages go to the terminal.
