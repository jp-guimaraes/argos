//! Ties the pure parsing helpers in [`crate::sysfs`] and [`crate::mounts`]
//! together with real filesystem reads under `/sys/block` and `/proc/mounts` to
//! produce the [`Device`] list the rest of Argos consumes.

use crate::dm;
use crate::mounts::{self, MountEntry};
use crate::sysfs;
use crate::udisks2::Udisks2Snapshot;
use argos_core::device::{Bus, Device};
use argos_core::error::{ArgosError, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub struct LinuxPlatform;

impl LinuxPlatform {
    pub fn new() -> Self {
        Self
    }
}

impl Default for LinuxPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl argos_platform::PlatformOps for LinuxPlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        let mount_entries = read_proc_mounts()?;
        // Fetched once per call, not once per device: a single D-Bus round
        // trip either way, and it's fine for the same snapshot to be
        // (very slightly) stale across the handful of devices this loop
        // processes. `None` here just means "cross-check unavailable" --
        // see read_block_device.
        let udisks2_snapshot = Udisks2Snapshot::fetch();
        let mut devices = Vec::new();

        let sys_block = Path::new("/sys/block");
        let entries = match fs::read_dir(sys_block) {
            Ok(entries) => entries,
            // No /sys/block at all (e.g. non-Linux test sandbox): report no disks
            // rather than failing the whole command.
            Err(_) => return Ok(devices),
        };

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if sysfs::is_excluded_block_device_name(&name) {
                continue;
            }
            if let Some(device) =
                read_block_device(&name, &mount_entries, udisks2_snapshot.as_ref())?
            {
                devices.push(device);
            }
        }

        devices.sort_by(|a, b| a.platform_id.cmp(&b.platform_id));
        Ok(devices)
    }

    fn refresh(&self, platform_id: &str, expected_serial: Option<&str>) -> Result<Option<Device>> {
        let name = match platform_id.strip_prefix("/dev/") {
            Some(name) => name,
            None => return Ok(None),
        };
        let mount_entries = read_proc_mounts()?;
        let udisks2_snapshot = Udisks2Snapshot::fetch();
        let device = read_block_device(name, &mount_entries, udisks2_snapshot.as_ref())?;
        Ok(device.filter(|d| expected_serial.is_none() || d.serial.as_deref() == expected_serial))
    }

    fn unmount(&self, device: &Device) -> Result<()> {
        let mount_entries = read_proc_mounts()?;
        let whole_disk = device.platform_id.clone();
        for mount in mount_entries.iter().filter(|m| {
            dm::resolve_mount_source(&m.source)
                .iter()
                .any(|d| d == &whole_disk)
        }) {
            let status = std::process::Command::new("umount")
                .arg(&mount.source)
                .status()
                .map_err(ArgosError::Io)?;
            if !status.success() {
                return Err(ArgosError::Io(std::io::Error::other(format!(
                    "umount {} exited with {status}",
                    mount.source
                ))));
            }
        }
        Ok(())
    }

    fn eject(&self, device: &Device) -> Result<()> {
        // Best-effort: not every system has `eject` installed, and Argos doesn't
        // depend on it succeeding to consider a write complete.
        let _ = std::process::Command::new("eject")
            .arg(&device.platform_id)
            .status();
        Ok(())
    }

    fn backing_device_of(&self, path: &Path) -> Result<Option<String>> {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mount_entries = read_proc_mounts()?;
        Ok(mounts::whole_disk_containing_path(
            &mount_entries,
            &canonical,
            &dm::resolve_mount_source,
        ))
    }

    fn reread_partition_table(&self, device: &Device) -> Result<()> {
        reread_partition_table_impl(&device.platform_id)
    }

    fn mount_ntfs_partition(&self, device: &Device, partition_number: u32) -> Result<PathBuf> {
        let partition_path = self.partition_device_path(device, partition_number);
        let mountpoint = tempfile::Builder::new()
            .prefix("argos-windows-write-")
            .tempdir()
            .map_err(ArgosError::Io)?
            .keep();

        let status = std::process::Command::new("ntfs-3g")
            .arg(&partition_path)
            .arg(&mountpoint)
            .status()
            .map_err(ArgosError::Io)?;
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
        let status = std::process::Command::new("umount")
            .arg(mount_path)
            .status()
            .map_err(ArgosError::Io)?;
        if !status.success() {
            return Err(ArgosError::Io(std::io::Error::other(format!(
                "umount {} exited with {status}",
                mount_path.display()
            ))));
        }
        Ok(())
    }

    fn partition_device_path(&self, device: &Device, partition_number: u32) -> String {
        mounts::partition_device_path(&device.platform_id, partition_number)
    }
}

fn read_proc_mounts() -> Result<Vec<MountEntry>> {
    let contents = fs::read_to_string("/proc/mounts").map_err(ArgosError::Io)?;
    Ok(mounts::parse_proc_mounts(&contents))
}

/// `gptman::linux::reread_partition_table` (the `BLKRRPART` ioctl wrapper
/// this delegates to) only exists when actually compiling for Linux --
/// unlike the rest of this crate, which happens to compile cleanly
/// everywhere because every other OS call it makes degrades gracefully at
/// *runtime* instead (no `/sys/block`, no `udisksd`, ...). This is the one
/// genuinely Linux-only API surface, so it needs an explicit `cfg` split
/// instead, with the same `NotImplemented` posture `argos-platform-macos`
/// and `argos-platform-windows` already use for methods that plain don't
/// apply on their platform.
#[cfg(target_os = "linux")]
fn reread_partition_table_impl(device_path: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device_path)
        .map_err(ArgosError::Io)?;
    gptman::linux::reread_partition_table(&mut file)
        .map_err(|e| ArgosError::Io(std::io::Error::other(e.to_string())))
}

#[cfg(not(target_os = "linux"))]
fn reread_partition_table_impl(_device_path: &str) -> Result<()> {
    Err(ArgosError::NotImplemented(
        "reread_partition_table (non-Linux)",
    ))
}

/// Reads everything sysfs (and, when available, the udev database and a
/// UDisks2 snapshot) know about `/sys/block/<name>`, returning `None` for
/// devices that aren't real disks (zero size -- e.g. an empty card-reader
/// slot).
fn read_block_device(
    name: &str,
    mount_entries: &[MountEntry],
    udisks2_snapshot: Option<&Udisks2Snapshot>,
) -> Result<Option<Device>> {
    let sys_path = PathBuf::from("/sys/block").join(name);

    let size_sectors: u64 = read_trimmed(&sys_path.join("size"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if size_sectors == 0 {
        return Ok(None);
    }
    let size_bytes = size_sectors * 512;

    let os_reports_removable = read_trimmed(&sys_path.join("removable"))
        .map(|s| s == "1")
        .unwrap_or(false);

    let resolved_syspath = fs::canonicalize(&sys_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut bus = sysfs::detect_bus_from_syspath(&resolved_syspath);

    let mut serial = read_trimmed(&sys_path.join("device/serial")).ok();

    if let Ok(dev_id) = read_trimmed(&sys_path.join("dev")) {
        if let Some(udev_props) = read_udev_db_record(&dev_id) {
            if let Some(id_bus) = udev_props.get("ID_BUS") {
                bus = match id_bus.as_str() {
                    "usb" => Bus::Usb,
                    "ata" => Bus::Ata,
                    "mmc" => Bus::Sdio,
                    "nvme" => Bus::Nvme,
                    _ => bus,
                };
            }
            if serial.is_none() {
                serial = udev_props.get("ID_SERIAL_SHORT").cloned();
            }
        }
    }

    let vendor = read_trimmed(&sys_path.join("device/vendor")).unwrap_or_default();
    let model = read_trimmed(&sys_path.join("device/model")).unwrap_or_default();
    let display_name = format!("{vendor} {model}").trim().to_string();
    let display_name = if display_name.is_empty() {
        name.to_string()
    } else {
        display_name
    };

    let platform_id = format!("/dev/{name}");
    let is_system_disk =
        mounts::disk_holds_a_critical_mount(mount_entries, &platform_id, &dm::resolve_mount_source);

    #[allow(unused_mut)]
    let mut os_reports_removable = os_reports_removable;

    // Cross-check against UDisks2, when reachable: defense in depth means
    // this can only push the verdict towards *more* conservative, never
    // towards allowing something sysfs/udev alone wouldn't have. A `None`
    // snapshot (no D-Bus/udisksd) or no entry for this device leaves the
    // sysfs/udev-derived signals completely untouched.
    if let Some(info) = udisks2_snapshot.and_then(|s| s.get(&platform_id)) {
        if !info.looks_removable_usb() {
            os_reports_removable = false;
            if bus == Bus::Usb {
                bus = Bus::Unknown;
            }
        }
    }

    #[cfg(feature = "test-overrides")]
    if std::env::var("ARGOS_TEST_FORCE_REMOVABLE").as_deref() == Ok(platform_id.as_str()) {
        os_reports_removable = true;
        bus = Bus::Usb;
    }

    Ok(Some(Device {
        platform_id,
        display_name,
        size_bytes,
        bus,
        os_reports_removable,
        is_system_disk,
        serial,
    }))
}

fn read_trimmed(path: &Path) -> std::io::Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

/// Reads `/run/udev/data/b<major>:<minor>` for the block device whose `dev` file
/// (in sysfs) contains `"<major>:<minor>"`. Returns `None` when udev isn't
/// running or hasn't recorded this device (never an error -- sysfs alone is
/// still enough to build a `Device`).
fn read_udev_db_record(major_minor: &str) -> Option<std::collections::HashMap<String, String>> {
    let path = format!("/run/udev/data/b{major_minor}");
    let contents = fs::read_to_string(path).ok()?;
    Some(sysfs::parse_udev_db_record(&contents))
}
