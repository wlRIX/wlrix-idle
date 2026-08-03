// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the daemon's output goes.
//!
//! The terminal, and only the terminal. `wlrix-session` starts this program with its stdout
//! and stderr already redirected into `wlrix-session.log`, so opening a second file here would
//! write the same lines twice in two places -- and split the record of a session in half for
//! anyone trying to read it afterwards.
//!
//! The macros exist rather than plain `println!` so that every line carries the program name.
//! In a shared log that is the difference between a message and a mystery.

/// An ordinary message.
pub fn out(args: std::fmt::Arguments) {
    println!("wlrix-idle: {args}");
}

/// Something wrong.
pub fn err(args: std::fmt::Arguments) {
    eprintln!("wlrix-idle: {args}");
}

/// Report something that went as expected.
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::out(format_args!($($arg)*)) };
}

/// Report something that did not.
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::err(format_args!($($arg)*)) };
}
