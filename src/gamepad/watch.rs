// SPDX-License-Identifier: GPL-3.0-or-later
//! Noticing a controller being plugged in.
//!
//! An inotify watch on `/dev/input`, whose fd goes into the same calloop loop as everything
//! else -- the same arrangement `wlrix-desktop` uses to watch the desktop directory, and for
//! the same reason: nothing polls.
//!
//! The events are thrown away. What arrives is "something changed", and the answer is always to
//! rescan the directory; tracking individual creates and removals would mean reimplementing the
//! device list incrementally for a directory with a couple of dozen entries in it.
//!
//! `ATTRIB` is load-bearing here in a way it is not for a directory of files. The device node
//! appears -- `CREATE` -- *before* udev has applied the uaccess ACL that makes it readable, so
//! a rescan triggered by the create alone opens it and gets `EACCES`. udev's `setfacl` then
//! changes the node's permissions, which is an `ATTRIB` on the directory entry, and the rescan
//! that fires from that one succeeds.

use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::inotify;

/// Where the device nodes live.
pub const DEVICE_DIR: &str = "/dev/input";

/// `CREATE` and `ATTRIB` between them cover a device appearing and then becoming readable; the
/// rest cover it going away, and udev renaming a node into place.
const WATCHED: inotify::WatchFlags = inotify::WatchFlags::CREATE
    .union(inotify::WatchFlags::ATTRIB)
    .union(inotify::WatchFlags::DELETE)
    .union(inotify::WatchFlags::MOVED_TO)
    .union(inotify::WatchFlags::MOVED_FROM);

/// A watch on `/dev/input`.
pub struct Watch {
    fd: OwnedFd,
}

impl Watch {
    /// Start watching. Fails if inotify is unavailable, or if there is no `/dev/input` at all --
    /// on a machine with no input devices, which is not one anybody is playing games on.
    pub fn new() -> std::io::Result<Self> {
        // Non-blocking: the loop reads until the fd is drained and must not stall there.
        let fd = inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
        inotify::add_watch(&fd, Path::new(DEVICE_DIR), WATCHED)?;
        Ok(Self { fd })
    }

    /// Drain every pending event and say whether anything happened.
    ///
    /// Always drains fully, whatever it finds: a level-triggered loop source would spin forever
    /// on an fd left readable. Plugging one controller in produces a burst of events across its
    /// several nodes, and one rescan covers the burst.
    pub fn drain(&mut self) -> bool {
        let mut buffer = [MaybeUninit::<u8>::uninit(); 4096];
        let mut changed = false;
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        loop {
            match reader.next() {
                Ok(_) => changed = true,
                // Drained, or nothing there yet. (`AGAIN` is the same value on Linux.)
                Err(rustix::io::Errno::WOULDBLOCK) => break,
                Err(rustix::io::Errno::INTR) => continue,
                // Anything else means the fd is unusable; stop rather than spin on it.
                Err(_) => break,
            }
        }
        changed
    }
}

/// So the watch can go on the loop as a `Generic` source directly, rather than the loop holding
/// a bare fd and the watch separately.
impl AsFd for Watch {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
