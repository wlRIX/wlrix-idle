// SPDX-License-Identifier: GPL-3.0-or-later
//! The laptop panel's backlight: read from sysfs, set through logind -- for dimming.
//!
//! Taken from `wlrix-osd`'s brightness keys, which find and set the same device the same way.
//!
//! # Why logind
//!
//! `/sys/class/backlight/<device>/brightness` is writable by root alone. Every desktop has
//! needed some way around that -- a setuid helper, a udev rule giving a group write access, a
//! polkit-guarded D-Bus service -- and systemd-logind now offers one to anybody with an active
//! session: `Session.SetBrightness`. It checks that the caller is the session on the seat the
//! device belongs to, which is exactly the right test, and needs nothing installed. Reading is
//! open to everyone, so the current level comes straight from sysfs.
//!
//! # Which device
//!
//! A laptop may expose several interfaces to one panel. The kernel's `type` file says which
//! kind each is, and the preference is the same as every desktop's: `firmware` (ACPI, which
//! knows the panel), then `platform` (a vendor driver), then `raw` (the GPU's register, which
//! works but knows nothing about the curve).
//!
//! A desktop with no backlight -- monitors are dimmed from their own buttons, or over DDC --
//! has none, and the brightness keys say so with a bar rather than doing nothing silently.

use std::path::{Path, PathBuf};

const SYSFS: &str = "/sys/class/backlight";

/// One backlight device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlight {
    /// The name under `/sys/class/backlight`, which is what logind is told.
    pub name: String,
    path: PathBuf,
    pub max: u32,
}

impl Backlight {
    /// The preferred device, if there is one.
    pub fn find() -> Option<Self> {
        Self::find_in(Path::new(SYSFS))
    }

    fn find_in(root: &Path) -> Option<Self> {
        let rank = |kind: &str| match kind {
            "firmware" => 0,
            "platform" => 1,
            "raw" => 2,
            _ => 3,
        };
        let mut found: Vec<(u8, Self)> = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let max = read_number(&path.join("max_brightness"))?;
                if max == 0 {
                    return None;
                }
                let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
                Some((
                    rank(kind.trim()),
                    Self {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        path,
                        max,
                    },
                ))
            })
            .collect();
        // By kind, then by name, so the choice does not depend on directory order.
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        found.into_iter().next().map(|(_, backlight)| backlight)
    }

    /// The current level, in the device's own units.
    pub fn current(&self) -> Option<u32> {
        read_number(&self.path.join("brightness"))
    }

    /// The current level as a percentage.
    #[cfg(test)]
    pub fn percent(&self) -> Option<u32> {
        Some(to_percent(self.current()?, self.max))
    }

    /// Set the level, in the device's own units, through logind.
    pub fn set(&self, system: &zbus::blocking::Connection, value: u32) -> zbus::Result<()> {
        system.call_method(
            Some("org.freedesktop.login1"),
            // logind's name for "the session of whoever is asking".
            "/org/freedesktop/login1/session/auto",
            Some("org.freedesktop.login1.Session"),
            "SetBrightness",
            &("backlight", self.name.as_str(), value.min(self.max)),
        )?;
        Ok(())
    }
}

fn read_number(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(test)]
fn to_percent(value: u32, max: u32) -> u32 {
    ((value as u64 * 100 + max as u64 / 2) / max.max(1) as u64) as u32
}

/// The level to dim to: a third of where it is, never below one unit -- which on most panels is
/// still lit -- and never above where it already is.
pub fn dimmed(current: u32) -> u32 {
    (current / 3).max(1).min(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimming_is_a_third_and_never_off() {
        assert_eq!(dimmed(900), 300);
        assert_eq!(dimmed(2), 1);
        assert_eq!(dimmed(0), 0, "already off stays off");
    }

    #[test]
    fn firmware_is_preferred_over_the_raw_register() {
        let root =
            std::env::temp_dir().join(format!("wlrix-idle-backlight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (name, kind, max) in [
            ("intel_backlight", "raw", "96000"),
            ("acpi_video0", "firmware", "10"),
        ] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("type"), kind).unwrap();
            std::fs::write(dir.join("max_brightness"), max).unwrap();
            std::fs::write(dir.join("brightness"), "5").unwrap();
        }
        let found = Backlight::find_in(&root).expect("a backlight");
        assert_eq!(found.name, "acpi_video0");
        assert_eq!(found.max, 10);
        assert_eq!(found.percent(), Some(50));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            Backlight::find_in(&root),
            None,
            "no directory, no backlight"
        );
    }
}
