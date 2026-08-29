//! The device model shared by every platform backend. Platform crates
//! (`argos-platform-linux`, `argos-platform-macos`, ...) are responsible for
//! *populating* a [`Device`] from OS-specific sources; this module only holds the
//! data and the safety judgement that must not vary by platform.

use serde::{Deserialize, Serialize};

/// The bus a disk is attached through. Used as one signal (never the only one) when
/// deciding whether a disk is safe to overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bus {
    Usb,
    Sdio,
    Ata,
    Nvme,
    Unknown,
}

/// A candidate disk, as reported by a platform backend, plus the safety verdict
/// Argos computed for it. Never trust a single OS-reported flag in isolation: a
/// disk is only ever offered for writing when multiple independent signals agree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// OS-specific identifier, e.g. `/dev/sdb` on Linux or `/dev/disk4` on macOS.
    /// Never assume this is stable across a reboot or a hot-plug event -- always
    /// re-resolve by `serial` + `size_bytes` right before a destructive operation.
    pub platform_id: String,
    /// Human-readable vendor + model, for display only.
    pub display_name: String,
    pub size_bytes: u64,
    pub bus: Bus,
    /// What the OS itself reports as "removable". Necessary but not sufficient --
    /// see [`Device::is_safe_to_write`].
    pub os_reports_removable: bool,
    /// Computed by the platform backend by cross-referencing mount points (and, on
    /// macOS, by walking up to the parent whole disk): true if this disk currently
    /// holds a mounted system partition (`/`, `/boot`, `/boot/efi`, `/home`, ...).
    pub is_system_disk: bool,
    /// Serial number, when the OS exposes one. Used to re-identify a disk across a
    /// listing-to-write window; a mismatch at write time means the disk changed
    /// under us (unplugged/replugged, or another drive claimed the same path) and
    /// the operation must abort.
    pub serial: Option<String>,
}

impl Device {
    /// The single safety gate every write path must call before opening a disk for
    /// writing. Combines bus + OS-reported removability + system-disk detection.
    /// A caller wanting to override this (`--i-know-what-im-doing`) must do so
    /// explicitly and separately -- this method itself never has a bypass, so it
    /// stays trustworthy to call from anywhere.
    pub fn is_safe_to_write(&self) -> bool {
        !self.is_system_disk && self.os_reports_removable && self.bus == Bus::Usb
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_device() -> Device {
        Device {
            platform_id: "/dev/sdz".into(),
            display_name: "Example USB Stick".into(),
            size_bytes: 8_000_000_000,
            bus: Bus::Usb,
            os_reports_removable: true,
            is_system_disk: false,
            serial: Some("ABC123".into()),
        }
    }

    #[test]
    fn safe_usb_removable_non_system_disk_is_writable() {
        assert!(base_device().is_safe_to_write());
    }

    #[test]
    fn system_disk_is_never_writable_even_if_flagged_removable() {
        let mut d = base_device();
        d.is_system_disk = true;
        assert!(!d.is_safe_to_write());
    }

    #[test]
    fn non_removable_disk_is_not_writable() {
        let mut d = base_device();
        d.os_reports_removable = false;
        assert!(!d.is_safe_to_write());
    }

    #[test]
    fn non_usb_bus_is_not_writable_even_if_removable_flag_is_set() {
        // Some hot-swap SATA bays report removable=1 while holding user data.
        let mut d = base_device();
        d.bus = Bus::Ata;
        assert!(!d.is_safe_to_write());
    }
}
