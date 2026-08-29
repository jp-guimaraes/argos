//! Ties the pure parsing helpers in [`crate::diskutil`] to real `diskutil`
//! (and, for path-to-device resolution, `df`) subprocess calls, producing
//! the [`Device`] list the rest of Argos consumes -- the macOS mirror of
//! `argos-platform-linux`'s `enumerate.rs`.
//!
//! `list_removable_disks` deliberately does *not* hard-exclude every disk
//! with `Internal == true`, even though that's how backlog E3 first phrased
//! it. Synthesized APFS-container pseudo-disks (`VirtualOrPhysical ==
//! "Virtual"`) are excluded outright -- they're not a real, independently
//! writable block device, the same reason `argos-platform-linux` drops
//! loop/dm/md/zram entries. But an internal *physical* disk is still
//! returned, fully described, and left to fail
//! [`Device::is_safe_to_write`](argos_core::device::Device::is_safe_to_write)
//! on its own signals (`RemovableMedia` and `BusProtocol` never look like a
//! USB stick) plus [`resolve_system_whole_disk`], which cross-references
//! `diskutil info -plist /` the way the backlog calls for. That keeps this
//! backend consistent with the trait's contract ("this is not a raw dump")
//! and with the project's own guiding decision to layer independent safety
//! signals rather than gate everything on one boolean.

use crate::diskutil::{self, DiskInfo};
use argos_core::device::{Bus, Device};
use argos_core::error::{ArgosError, Result};
use std::path::Path;
use std::process::Command;

pub struct MacOsPlatform;

impl MacOsPlatform {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MacOsPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl argos_platform::PlatformOps for MacOsPlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        let whole_disk_ids = list_whole_disks()?;
        let system_whole_disk = resolve_system_whole_disk();

        let mut devices = Vec::new();
        for id in whole_disk_ids {
            let Some(info) = info_for(&id) else {
                // Raced with a disk disappearing between `list` and `info`,
                // or diskutil returned something this parser doesn't
                // recognize for this one id -- skip it rather than failing
                // the whole listing.
                continue;
            };
            if info.virtual_or_physical.as_deref() == Some("Virtual") {
                continue;
            }
            devices.push(device_from_info(&info, system_whole_disk.as_deref()));
        }

        devices.sort_by(|a, b| a.platform_id.cmp(&b.platform_id));
        Ok(devices)
    }

    fn refresh(&self, platform_id: &str, expected_serial: Option<&str>) -> Result<Option<Device>> {
        let Some(id) = platform_id.strip_prefix("/dev/") else {
            return Ok(None);
        };
        let Some(info) = info_for(id) else {
            return Ok(None);
        };
        let system_whole_disk = resolve_system_whole_disk();
        let device = device_from_info(&info, system_whole_disk.as_deref());
        Ok(Some(device)
            .filter(|d| expected_serial.is_none() || d.serial.as_deref() == expected_serial))
    }

    fn unmount(&self, device: &Device) -> Result<()> {
        // The whole disk, not a single partition -- `diskutil unmountDisk`
        // unmounts every mounted volume implied by the disk (including ones
        // behind an APFS container), which is what's needed before this
        // disk can be opened exclusively for a DD-mode write.
        let status = Command::new("diskutil")
            .args(["unmountDisk", &device.platform_id])
            .status()
            .map_err(ArgosError::Io)?;
        if !status.success() {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "diskutil unmountDisk {} exited with {status}",
                device.platform_id
            ))));
        }
        Ok(())
    }

    fn eject(&self, device: &Device) -> Result<()> {
        // Best-effort, matching the Linux backend: Argos doesn't depend on
        // eject succeeding to consider a write complete.
        let _ = Command::new("diskutil")
            .args(["eject", &device.platform_id])
            .status();
        Ok(())
    }

    fn backing_device_of(&self, path: &Path) -> Result<Option<String>> {
        let Some(partition_node) = device_node_via_df(path) else {
            return Ok(None);
        };
        let Some(id) = partition_node.strip_prefix("/dev/") else {
            return Ok(None);
        };
        let Some(info) = info_for(id) else {
            return Ok(None);
        };
        let parent = info.parent_whole_disk.unwrap_or(info.device_identifier);
        Ok(Some(format!("/dev/{parent}")))
    }
}

fn run_diskutil(args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("diskutil").args(args).output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn list_whole_disks() -> Result<Vec<String>> {
    let output = Command::new("diskutil")
        .args(["list", "-plist"])
        .output()
        .map_err(ArgosError::Io)?;
    if !output.status.success() {
        return Err(ArgosError::Io(std::io::Error::other(
            "diskutil list -plist exited with a non-zero status",
        )));
    }
    diskutil::parse_whole_disks(&output.stdout).ok_or_else(|| {
        ArgosError::Io(std::io::Error::other(
            "could not parse `diskutil list -plist` output",
        ))
    })
}

fn info_for(id: &str) -> Option<DiskInfo> {
    let stdout = run_diskutil(&["info", "-plist", id])?;
    diskutil::parse_disk_info(&stdout)
}

/// Resolves the physical whole disk currently holding the running system, by
/// cross-referencing `diskutil info -plist /` -- see the module doc for why
/// this matters in addition to, not instead of, the `Internal`/bus/removable
/// signals already on each [`Device`]. Returns `None` only when `diskutil`
/// itself is unavailable or its output no longer parses; callers still have
/// those other signals in that case, so this is one extra layer, not the
/// only one.
fn resolve_system_whole_disk() -> Option<String> {
    let root = info_for("/")?;
    let parent_id = root.parent_whole_disk.unwrap_or(root.device_identifier);
    let parent = info_for(&parent_id)?;
    Some(diskutil::resolve_physical_system_disk(&parent))
}

/// Resolves the device node (e.g. `/dev/disk3s5`) of the partition backing
/// `path`, by asking `df` -- unlike `diskutil info`, `df` accepts an
/// arbitrary file path (not just a device identifier or a volume's own mount
/// point) and walks up to whichever mounted filesystem actually contains it.
fn device_node_via_df(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let output = Command::new("df")
        .args(["-P", &canonical.to_string_lossy()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Line 0 is the header ("Filesystem 512-blocks ... Mounted on"); line 1
    // is the one data row `df -P <single-path>` ever prints, whose first
    // field is the `Filesystem` column.
    text.lines()
        .nth(1)?
        .split_whitespace()
        .next()
        .map(str::to_owned)
}

fn device_from_info(info: &DiskInfo, system_whole_disk: Option<&str>) -> Device {
    let platform_id = if info.device_node.is_empty() {
        format!("/dev/{}", info.device_identifier)
    } else {
        info.device_node.clone()
    };
    let display_name = info
        .media_name
        .clone()
        .or_else(|| info.volume_name.clone())
        .unwrap_or_else(|| info.device_identifier.clone());

    Device {
        platform_id,
        display_name,
        size_bytes: info.size_bytes,
        bus: bus_from_protocol(info.bus_protocol.as_deref()),
        os_reports_removable: info.removable_media,
        is_system_disk: system_whole_disk == Some(info.device_identifier.as_str()),
        serial: info.serial_number.clone(),
    }
}

/// Maps `diskutil`'s `BusProtocol` string to [`Bus`]. Only `"USB"` maps to
/// [`Bus::Usb`] -- the one bus [`Device::is_safe_to_write`] ever accepts --
/// so an unrecognized or unmapped protocol string intentionally falls back
/// to [`Bus::Unknown`] rather than guessing: it can never accidentally let a
/// disk through the safety gate.
fn bus_from_protocol(protocol: Option<&str>) -> Bus {
    match protocol {
        Some("USB") => Bus::Usb,
        Some("SATA") | Some("Serial ATA") | Some("ATA") => Bus::Ata,
        Some("PCI-Express") | Some("PCI") | Some("NVMe") => Bus::Nvme,
        Some("Secure Digital") | Some("SDIO") => Bus::Sdio,
        _ => Bus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_from_protocol_maps_usb() {
        assert_eq!(bus_from_protocol(Some("USB")), Bus::Usb);
    }

    #[test]
    fn bus_from_protocol_maps_internal_buses_never_to_usb() {
        assert_eq!(bus_from_protocol(Some("Apple Fabric")), Bus::Unknown);
        assert_eq!(bus_from_protocol(Some("PCI-Express")), Bus::Nvme);
        assert_eq!(bus_from_protocol(Some("SATA")), Bus::Ata);
        assert_eq!(bus_from_protocol(None), Bus::Unknown);
    }

    #[test]
    fn device_from_info_flags_the_resolved_system_disk() {
        let info = DiskInfo {
            device_identifier: "disk0".into(),
            device_node: "/dev/disk0".into(),
            ..Default::default()
        };
        let device = device_from_info(&info, Some("disk0"));
        assert!(device.is_system_disk);

        let device = device_from_info(&info, Some("disk4"));
        assert!(!device.is_system_disk);

        let device = device_from_info(&info, None);
        assert!(!device.is_system_disk);
    }

    #[test]
    fn device_from_info_falls_back_to_device_identifier_for_platform_id_and_display_name() {
        let info = DiskInfo {
            device_identifier: "disk4".into(),
            ..Default::default()
        };
        let device = device_from_info(&info, None);
        assert_eq!(device.platform_id, "/dev/disk4");
        assert_eq!(device.display_name, "disk4");
    }
}
