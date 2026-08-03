// SPDX-License-Identifier: GPL-3.0-or-later
//! Running what a timeout asked for.
//!
//! Commands go through `sh -c`, which is swayidle's contract and what the configs this replaces
//! are already written against -- `wlopm --off \*` is a shell string, not an argv. Splitting it
//! ourselves would mean reimplementing quoting badly.
//!
//! Nothing waits for a command. A timeout that runs `notify-send` and one that runs a locker
//! are the same thing from here: started, and then reaped later so the process table does not
//! fill with zombies over a long session.

use std::process::{Child, Command, Stdio};

/// Start a command in the background. `None` if it could not be started at all.
///
/// Output goes nowhere. Ours is already redirected into the session log by `wlrix-session`, and
/// a locker or a dimmer chattering into that log every fifteen minutes would bury everything
/// worth reading in it.
pub fn spawn(command: &str) -> Option<Child> {
    match Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => Some(child),
        Err(err) => {
            warn!("could not run `{command}`: {err}");
            None
        }
    }
}

/// Reap anything that has exited, and drop what is still running from the list.
///
/// Returns the children still alive. A command that fails is reported once here rather than
/// being watched for, because the interesting failure -- a locker that exits immediately,
/// leaving the session unlocked -- is one nothing else would ever mention.
pub fn reap(children: Vec<Child>) -> Vec<Child> {
    let mut alive = Vec::new();
    for mut child in children {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                warn!("a command exited with {status}");
            }
            Ok(Some(_)) => {}
            // Still running, or we cannot tell; either way keep it.
            _ => alive.push(child),
        }
    }
    alive
}
