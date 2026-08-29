//! Ties the pure parsing helpers in [`crate::sysfs`] and [`crate::mounts`]
//! together with real filesystem reads under `/sys/block` and `/proc/mounts` to
//! produce the [`Device`] list the rest of Argos consumes.

use crate::mounts::{self, MountEntry};
use crate::sysfs;
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
            if let Some(device) = read_block_device(&name, &mount_entries)? {
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
        let device = read_block_device(name, &mount_entries)?;
        Ok(device.filter(|d| expected_serial.is_none() || d.serial.as_deref() == expected_serial))
    }

    fn unmount(&self, device: &Device) -> Result<()> {
        let mount_entries = read_proc_mounts()?;
        let whole_disk = device.platform_id.clone();
        for mount in mount_entries
            .iter()
            .filter(|m| mounts::whole_disk_of(&m.source) == whole_disk)
        {
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
        ))
    }
}

fn read_proc_mounts() -> Result<Vec<MountEntry>> {
    let contents = fs::read_to_string("/proc/mounts").map_err(ArgosError::Io)?;
    Ok(mounts::parse_proc_mounts(&contents))
}

/// Reads everything sysfs (and, when available, the udev database) know about
/// `/sys/block/<name>`, returning `None` for devices that aren't real disks
/// (zero size -- e.g. an empty card-reader slot).
fn read_block_device(name: &str, mount_entries: &[MountEntry]) -> Result<Option<Device>> {
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
    let is_system_disk = mounts::disk_holds_a_critical_mount(mount_entries, &platform_id);

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
