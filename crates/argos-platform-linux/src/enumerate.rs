//! Ties the pure parsing helpers in [`crate::sysfs`] and [`crate::mounts`]
//! together with real filesystem reads under `/sys/block` and `/proc/mounts` to
//! produce the [`Device`] list the rest of Argos consumes.

use crate::dm;
use crate::mounts::{self, MountEntry};
use crate::sysfs;
use crate::udisks2::Udisks2Snapshot;
use argos_core::device::{Bus, Device};
use argos_core::error::{ArgosError, Result};
use argos_platform::WrittenBytes;
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
            unmount_path(&mount.mountpoint)?;
        }
        Ok(())
    }

    fn eject(&self, device: &Device) -> Result<()> {
        // Best-effort in the sense that Argos doesn't depend on this
        // succeeding to consider a write complete -- but that posture lives
        // in the CLI (`eject_best_effort` prints a warning on `Err` rather
        // than failing the command), not here. Swallowing every failure
        // into `Ok(())`, as this used to do, made that warning path dead
        // code: a real failure (seen on real hardware: `eject` exiting with
        // "Permission denied") went completely unreported, and the CLI
        // printed "Ejected. Safe to unplug." regardless.
        match std::process::Command::new("eject")
            .arg(&device.platform_id)
            .status()
        {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(ArgosError::Io(std::io::Error::other(format!(
                "eject exited with {status}"
            )))),
            // Not every system has `eject` installed -- nothing to report,
            // there's no eject-manage step to have failed.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(ArgosError::Io(err)),
        }
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

    fn written_bytes(&self, device: &Device) -> Option<WrittenBytes> {
        let name = device.platform_id.strip_prefix("/dev/")?;
        let stat_path = PathBuf::from("/sys/block").join(name).join("stat");
        // Probed once here rather than only inside the closure so an
        // unreadable counter degrades to "no counter" (and the old
        // bytes-handed-to-the-OS progress) up front, instead of silently
        // reporting nothing for the whole write.
        read_sectors_written(&stat_path)?;
        Some(WrittenBytes::new(move || {
            read_sectors_written(&stat_path).map(|sectors| sectors * SECTOR_BYTES)
        }))
    }
}

/// `/sys/block/<dev>/stat` reports in 512-byte sectors regardless of the
/// device's own logical or physical block size -- see
/// [`sysfs::parse_sectors_written`], which is where the column itself is
/// documented and tested.
const SECTOR_BYTES: u64 = 512;

/// Reads the counter [`sysfs::parse_sectors_written`] parses.
///
/// It counts *every* writer to the device, not just this process -- fine
/// here, since Argos unmounts the device before writing and nothing else is
/// expected to touch it, and per-process accounting isn't exposed per-device
/// at all. Callers clamp against their own byte count anyway, so a stray
/// writer can only make progress look slower than it is, never further along.
fn read_sectors_written(stat_path: &Path) -> Option<u64> {
    sysfs::parse_sectors_written(&fs::read_to_string(stat_path).ok()?)
}

/// Unmounts one filesystem with `umount2(2)`, replacing a shell-out to
/// `umount(8)` (M7.1, backlog #46).
///
/// Two reasons beyond tidiness: one fewer runtime dependency for a
/// distribution package to declare, and one fewer program whose presence,
/// exit codes or messages could change under us.
///
/// Note the argument. `umount(8)` accepts either the source device or the
/// mountpoint; the syscall takes **only the mountpoint**, which is why this
/// is passed `mount.mountpoint` where the shell-out was passed
/// `mount.source`.
#[cfg(target_os = "linux")]
fn unmount_path(mountpoint: &str) -> Result<()> {
    let target = std::ffi::CString::new(mountpoint).map_err(|_| {
        ArgosError::Io(std::io::Error::other(format!(
            "mountpoint {mountpoint} contains a NUL byte and cannot be unmounted"
        )))
    })?;

    // SAFETY: `target` is a valid NUL-terminated C string that outlives the
    // call, and `umount2` only reads through the pointer.
    if unsafe { libc::umount2(target.as_ptr(), 0) } == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    Err(ArgosError::Io(std::io::Error::new(
        err.kind(),
        format!("could not unmount {mountpoint}: {err}"),
    )))
}

/// This backend only ever runs on Linux; the stub exists because the crate is
/// a workspace member and so is compiled on every host (see its `Cargo.toml`).
#[cfg(not(target_os = "linux"))]
fn unmount_path(mountpoint: &str) -> Result<()> {
    Err(ArgosError::Io(std::io::Error::other(format!(
        "unmounting {mountpoint} needs umount2(2), which exists only on Linux"
    ))))
}

fn read_proc_mounts() -> Result<Vec<MountEntry>> {
    let contents = fs::read_to_string("/proc/mounts").map_err(ArgosError::Io)?;
    Ok(mounts::parse_proc_mounts(&contents))
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
