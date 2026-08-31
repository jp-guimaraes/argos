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
use std::path::{Path, PathBuf};
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
            let is_virtual = info.virtual_or_physical.as_deref() == Some("Virtual");
            if is_virtual && !test_force_removable_matches(&info) {
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
        unmount_whole_disk(&device.platform_id)
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

    // Windows installer writes (backlog #27, extended to macOS by backlog
    // #34/WM1) -- see `docs/architecture.md`'s phase 3 guiding decisions for
    // the macFUSE/ntfs-3g prerequisite and why `diskutil unmountDisk` doubles
    // as this platform's "reread the partition table" primitive.
    fn reread_partition_table(&self, device: &Device) -> Result<()> {
        // macOS has no BLKRRPART-style ioctl: DiskArbitration discovers a
        // freshly-written GPT on its own, but only after being nudged --
        // `diskutil unmountDisk` is the idiomatic way to force that
        // re-probe (the same command `PlatformOps::unmount` already uses),
        // and it also guards against the exact race the module doc for
        // `argos-platform-macos` warns about: `diskarbitrationd` noticing
        // the new partitions and trying to auto-mount one mid-write. Calling
        // it again here, right after the GPT write, closes that window
        // before the caller opens any partition device directly.
        unmount_whole_disk(&device.platform_id)
    }

    fn mount_ntfs_partition(&self, device: &Device, partition_number: u32) -> Result<PathBuf> {
        let partition_path = self.partition_device_path(device, partition_number);

        // Best-effort: macOS's own built-in (read-only) NTFS driver can
        // auto-mount a partition as soon as `mkfs.ntfs` gives it a valid
        // NTFS filesystem, racing this call. ntfs-3g can't mount an
        // already-mounted device, so clear that first -- a no-op, not an
        // error, when nothing was mounted yet.
        let _ = Command::new("diskutil")
            .args(["unmount", &partition_path])
            .status();

        let mountpoint = tempfile::Builder::new()
            .prefix("argos-windows-write-")
            .tempdir()
            .map_err(ArgosError::Io)?
            .keep();

        let status = Command::new("ntfs-3g")
            .arg(&partition_path)
            .arg(&mountpoint)
            .status()
            .map_err(|err| ntfs_3g_error(err, "mount"))?;
        if !status.success() {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "ntfs-3g {} {} exited with {status}",
                partition_path,
                mountpoint.display()
            ))));
        }
        Ok(mountpoint)
    }

    fn unmount_path(&self, mount_path: &Path) -> Result<()> {
        let status = Command::new("diskutil")
            .arg("unmount")
            .arg(mount_path)
            .status()
            .map_err(ArgosError::Io)?;
        if !status.success() {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "diskutil unmount {} exited with {status}",
                mount_path.display()
            ))));
        }
        Ok(())
    }

    fn partition_device_path(&self, device: &Device, partition_number: u32) -> String {
        diskutil::partition_device_path(&device.platform_id, partition_number)
    }
}

fn unmount_whole_disk(platform_id: &str) -> Result<()> {
    let status = Command::new("diskutil")
        .args(["unmountDisk", platform_id])
        .status()
        .map_err(ArgosError::Io)?;
    if !status.success() {
        return Err(ArgosError::Io(std::io::Error::other(format!(
            "diskutil unmountDisk {platform_id} exited with {status}"
        ))));
    }
    Ok(())
}

/// Wraps a failure to even spawn `ntfs-3g` with a pointer at the actual
/// prerequisite (macFUSE, approved in System Settings) rather than a bare
/// "No such file or directory" -- the one relaxation of "no shelling out"
/// backlog #27's phase 2 guiding decisions call for is far less discoverable
/// on macOS than on Linux, since nothing here can `apt-get install` it.
fn ntfs_3g_error(err: std::io::Error, action: &str) -> ArgosError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ArgosError::Io(std::io::Error::other(format!(
            "could not run ntfs-3g to {action} the Windows partition: {err} -- install it \
             (e.g. `brew install --cask macfuse && brew install ntfs-3g-mac`) and approve the \
             macFUSE system extension in System Settings > Privacy & Security, then try again"
        )))
    } else {
        ArgosError::Io(err)
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

    let mut bus = bus_from_protocol(info.bus_protocol.as_deref());
    let mut os_reports_removable = info.removable_media;
    if test_force_removable_matches(info) {
        bus = Bus::Usb;
        os_reports_removable = true;
    }

    Device {
        platform_id,
        display_name,
        size_bytes: info.size_bytes,
        bus,
        os_reports_removable,
        is_system_disk: system_whole_disk == Some(info.device_identifier.as_str()),
        serial: info.serial_number.clone(),
    }
}

/// True when the `test-overrides` feature is enabled *and*
/// `ARGOS_TEST_FORCE_REMOVABLE` names this disk's device node. Always `false`
/// when the feature is off, so this can never fire in a production build --
/// see the feature's doc comment in `Cargo.toml`.
fn test_force_removable_matches(info: &DiskInfo) -> bool {
    #[cfg(feature = "test-overrides")]
    {
        let platform_id = if info.device_node.is_empty() {
            format!("/dev/{}", info.device_identifier)
        } else {
            info.device_node.clone()
        };
        std::env::var("ARGOS_TEST_FORCE_REMOVABLE").as_deref() == Ok(platform_id.as_str())
    }
    #[cfg(not(feature = "test-overrides"))]
    {
        let _ = info;
        false
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

    // Single test covering every ARGOS_TEST_FORCE_REMOVABLE-dependent
    // behavior, rather than several -- this crate's tests run in parallel by
    // default, and this is the one process-global piece of state any of them
    // touch, so one test owns the full set/assert/unset sequence instead of
    // risking two tests racing on the same env var.
    #[test]
    #[cfg(feature = "test-overrides")]
    fn test_force_removable_overrides_bus_removable_and_the_virtual_filter() {
        let device_node = "/dev/disk97-test-override";
        let virtual_disk = DiskInfo {
            device_identifier: "disk97".into(),
            device_node: device_node.into(),
            virtual_or_physical: Some("Virtual".into()),
            bus_protocol: Some("Disk Image".into()),
            removable_media: false,
            ..Default::default()
        };

        // Not matched yet: looks exactly like a real, non-removable disk
        // image -- unsafe to write, and not a match for the override.
        assert!(!test_force_removable_matches(&virtual_disk));
        let device = device_from_info(&virtual_disk, None);
        assert_eq!(device.bus, Bus::Unknown);
        assert!(!device.os_reports_removable);

        std::env::set_var("ARGOS_TEST_FORCE_REMOVABLE", device_node);
        assert!(test_force_removable_matches(&virtual_disk));
        let device = device_from_info(&virtual_disk, None);
        assert_eq!(device.bus, Bus::Usb);
        assert!(device.os_reports_removable);

        // A disk the override doesn't name is never affected.
        let other = DiskInfo {
            device_identifier: "disk98".into(),
            device_node: "/dev/disk98".into(),
            ..Default::default()
        };
        assert!(!test_force_removable_matches(&other));

        std::env::remove_var("ARGOS_TEST_FORCE_REMOVABLE");
    }
}
