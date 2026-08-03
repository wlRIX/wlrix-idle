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
  sleeps.

## Build

```sh
cargo build
cargo run
```

## Configuration

Read from `$XDG_CONFIG_HOME/wlrix/idle.toml` (or `~/.config/wlrix/idle.toml`), falling back to `/etc/wlrix/idle.toml`.
The first file found is used whole rather than merged, so your own file is all of what you get. Unknown keys are an
error: a silently ignored typo in a config file is a bad afternoon.

Without a config file nothing happens on a timeout — an idle manager that invents a screen-blanking policy nobody asked
for is worse than one that sits there.

```toml
# Each [[timeout]] is an independent countdown from the last activity, not a stage in a
# sequence. 600 and 900 below mean "dim at ten minutes, lock and blank at fifteen", both
# measured from the same moment.

[[timeout]]
after_secs = 600
command = "wlrix-osd --dim"           # run when the countdown ends
resume_command = "wlrix-osd --undim"  # run when the user comes back

[[timeout]]
after_secs = 900
lock = true                           # run [lock] command, if it is not already running
blank = true                          # switch the monitors off

# What to do when the machine is about to suspend. A logind delay inhibitor is held while
# this runs, so it finishes before the machine actually sleeps.
[before_sleep]
lock = true
blank = false
command = ""
timeout_secs = 4      # logind's InhibitDelayMaxSec is 5 by default; stay under it

[lock]
command = "swaylock -f -c 000000"

# Controllers. libinput treats a gamepad as a joystick and ignores it, so the compositor
# never sees a stick move -- this is what puts that input back.
[gamepad]
enable = true
deadzone = 0.25       # fraction of an axis's full range that counts as a move
min_interval_ms = 1000
allow = []            # case-insensitive substrings of the device name; empty means "any gamepad"
deny = []
devices = []          # explicit paths, bypassing detection entirely

[dbus]
screensaver = true       # own org.freedesktop.ScreenSaver
power_management = true  # own org.freedesktop.PowerManagement.Inhibit
logind = true            # take the before-sleep delay inhibitor
replace = false          # take the bus names from whoever already holds them
```

Numbers outside a sensible range are clamped rather than refused. Two things are dropped with a warning instead: a
timeout of zero seconds, which would fire instantly and forever, and a timeout that has no `blank`, no `lock` and no
`command`, which is a countdown that does nothing.

## Options

| Option            | Meaning                                                                       |
|-------------------|-------------------------------------------------------------------------------|
| `--config <path>` | Use this config file instead of searching the usual places.                   |
| `--list-devices`  | List the input devices found, say which are treated as controllers, and exit. |
| `--replace`       | Take `org.freedesktop.ScreenSaver` and friends from whoever holds them.       |
| `--no-dbus`       | Do not touch D-Bus at all: no inhibit interfaces, no before-sleep handling.   |
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
