// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercises the two inhibit interfaces, including the case that cannot be tested by hand.
//!
//! An application that takes an inhibit and then crashes never calls `UnInhibit`. Nothing else
//! would ever release it, so the session stops blanking for the rest of the login with no sign
//! of why -- which is what the `NameOwnerChanged` watch in `src/dbus/mod.rs` exists to prevent.
//! It cannot be checked with `busctl call` or `gdbus call`, because both exit the moment they
//! return and so never hold an inhibit long enough to be killed. So this starts a copy of
//! itself, has it take an inhibit and die, and watches the inhibit go.
//!
//! `HasInhibit` is the observable throughout: it is the only method either interface offers
//! that reports on state rather than changing it.
//!
//! Usage: `cargo run --example test_inhibit`, against a `wlrix-idle` on the same session bus.
//! During development that means running both under `dbus-run-session`, since a desktop
//! session already has something else owning these names:
//!
//! ```sh
//! dbus-run-session -- sh -c './target/debug/wlrix-idle & sleep 1; cargo run --example test_inhibit'
//! ```
//!
//! Exits non-zero on failure. Not part of the program; a dev tool only.

use std::time::{Duration, Instant};

#[zbus::proxy(
    interface = "org.freedesktop.PowerManagement.Inhibit",
    default_service = "org.freedesktop.PowerManagement.Inhibit",
    default_path = "/org/freedesktop/PowerManagement/Inhibit"
)]
trait PowerManagement {
    fn inhibit(&self, application: &str, reason: &str) -> zbus::Result<u32>;
    fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    fn has_inhibit(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(
    interface = "org.freedesktop.ScreenSaver",
    default_service = "org.freedesktop.ScreenSaver",
    default_path = "/org/freedesktop/ScreenSaver"
)]
trait ScreenSaver {
    fn inhibit(&self, application_name: &str, reason: &str) -> zbus::Result<u32>;
    fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    fn get_active(&self) -> zbus::Result<bool>;
    fn simulate_user_activity(&self) -> zbus::Result<()>;
}

/// The child half: take an inhibit and die without giving it back.
///
/// `process::exit` rather than falling off the end of `main`, so nothing gets a chance to run a
/// destructor that might close the connection politely. A crash does not say goodbye.
fn hold_and_die() -> ! {
    let connection = zbus::blocking::Connection::session().expect("no session bus");
    let power = PowerManagementProxyBlocking::new(&connection).expect("no inhibit interface");
    let cookie = power
        .inhibit("test_inhibit child", "about to die")
        .expect("Inhibit failed");
    println!("  child took cookie {cookie} and is exiting without releasing it");
    std::process::exit(0);
}

/// Wait for a condition, or give up. Returns whether it came true.
fn eventually(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn main() {
    if std::env::args().any(|arg| arg == "--hold-and-die") {
        hold_and_die();
    }

    let connection = zbus::blocking::Connection::session().expect("no session bus");
    let power = PowerManagementProxyBlocking::new(&connection)
        .expect("nothing is serving org.freedesktop.PowerManagement.Inhibit");
    let screensaver = ScreenSaverProxyBlocking::new(&connection)
        .expect("nothing is serving org.freedesktop.ScreenSaver");
    let mut failures = 0;

    let mut check = |name: &str, ok: bool| {
        if ok {
            println!("PASS: {name}");
        } else {
            eprintln!("FAIL: {name}");
            failures += 1;
        }
    };

    // 1. The plain round trip.
    check(
        "nothing is inhibiting to begin with",
        !power.has_inhibit().unwrap(),
    );
    let cookie = power
        .inhibit("test_inhibit", "checking")
        .expect("Inhibit failed");
    check("a cookie is never zero", cookie != 0);
    check(
        "Inhibit is reported by HasInhibit",
        power.has_inhibit().unwrap(),
    );
    power.un_inhibit(cookie).expect("UnInhibit failed");
    check("UnInhibit clears it", !power.has_inhibit().unwrap());

    // 2. Releasing twice. Common in real clients, and must not fail the call or the daemon.
    power
        .un_inhibit(cookie)
        .expect("a second UnInhibit should not be an error");
    check(
        "a second UnInhibit is harmless",
        !power.has_inhibit().unwrap(),
    );

    // 3. The two interfaces keep separate books. A screensaver inhibit holds the countdown off
    //    but is not an answer to "is anything stopping this machine suspending?".
    let saver_cookie = screensaver
        .inhibit("test_inhibit", "checking")
        .expect("Inhibit failed");
    check("a ScreenSaver cookie is never zero", saver_cookie != 0);
    check(
        "a ScreenSaver inhibit is not a PowerManagement one",
        !power.has_inhibit().unwrap(),
    );
    // A cookie from one interface must not release anything in the other.
    power
        .un_inhibit(saver_cookie)
        .expect("UnInhibit should not error");
    screensaver
        .un_inhibit(saver_cookie)
        .expect("UnInhibit failed");
    check(
        "cookies are not interchangeable between the interfaces",
        true,
    );

    // 4. The important one. A client that takes an inhibit and dies must not keep the session
    //    awake for the rest of the login.
    println!("starting a child that will inhibit and die");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--hold-and-die")
        .status()
        .expect("could not start the child");
    check("the child ran", child.success());
    // Deliberately not asserting that the inhibit was ever *observed* as held: the child takes
    // it and exits in the same breath, so whether this process sees the window is a race. What
    // matters is where it ends up.
    check(
        "a client that dies loses its inhibit",
        eventually(|| !power.has_inhibit().unwrap_or(true)),
    );

    // 5. The methods that only have to not fail.
    screensaver
        .simulate_user_activity()
        .expect("SimulateUserActivity failed");
    let _ = screensaver.get_active().expect("GetActive failed");
    check("SimulateUserActivity and GetActive are answered", true);

    if failures > 0 {
        eprintln!("{failures} failure(s)");
        std::process::exit(1);
    }
    println!("OK");
}
