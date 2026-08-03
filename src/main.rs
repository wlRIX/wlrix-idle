// SPDX-License-Identifier: GPL-3.0-or-later
//! wlRIX idle manager.
//!
//! Watches `ext-idle-notify-v1` for the timeouts in `idle.toml`, and blanks the monitors, runs
//! a locker or runs a command when one of them runs out. It is the session's single owner of
//! idle policy: the compositor serves the protocols, this decides what to do with them.
//!
//! Three things it does that `swayidle` and `wlopm` between them cannot. It **serves**
//! `org.freedesktop.ScreenSaver`, so an application playing a film can say "not now" and be
//! heard. It reads controllers straight from evdev, because libinput classifies a gamepad as a
//! joystick and drops it -- so without this the compositor never learns that anybody is
//! playing anything. And it holds a logind delay inhibitor, so the locker has finished before
//! the machine actually sleeps.

#[macro_use]
mod log;

mod action;
mod config;
mod dbus;
mod gamepad;
mod idle;
mod notify;
mod outputs;
mod pidfile;
mod signals;

use std::path::PathBuf;

/// What the command line asked for. The config file says everything else; these are the few
/// things that are either about this one run (`--list-devices`) or a deliberate override of a
/// setting it would be surprising to keep in a file (`--replace`).
#[derive(Default)]
pub struct Args {
    pub config: Option<PathBuf>,
    pub list_devices: bool,
    pub replace: bool,
    pub no_dbus: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();

    let mut argv = std::env::args().skip(1);
    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--config" => {
                args.config = Some(PathBuf::from(
                    argv.next()
                        .ok_or_else(|| "--config needs a path".to_string())?,
                ));
            }
            "--list-devices" => args.list_devices = true,
            "--replace" => args.replace = true,
            "--no-dbus" => args.no_dbus = true,
            "--help" | "-h" => {
                println!(
                    "wlrix-idle {}\n\n\
                     Usage: wlrix-idle [options]\n\n\
                     Options:\n  \
                       --config <path>   config file to use instead of the usual places\n  \
                       --list-devices    list input devices, say which count as controllers, exit\n  \
                       --replace         take the D-Bus names from whoever already holds them\n  \
                       --no-dbus         no inhibit interfaces and no before-sleep handling\n  \
                       -h, --help        this message\n\n\
                     Config: $XDG_CONFIG_HOME/wlrix/idle.toml, else /etc/wlrix/idle.toml\n\
                     Reload: kill -HUP $(cat $XDG_RUNTIME_DIR/wlrix-idle.pid)",
                    env!("CARGO_PKG_VERSION"),
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(args)
}

fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            warn!("{err}");
            return std::process::ExitCode::from(2);
        }
    };

    let loaded = config::load(args.config.as_deref());

    // Read-only, and needs no compositor: this is what someone runs when a controller is not
    // waking the screen, so it must work even when nothing else does.
    if args.list_devices {
        gamepad::list(&loaded.config.gamepad);
        return std::process::ExitCode::SUCCESS;
    }

    info!("config: {}", loaded.source.describe());
    info!("{}", loaded.config.summary());

    match idle::run(loaded.config, &args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            warn!("{err}");
            std::process::ExitCode::FAILURE
        }
    }
}
