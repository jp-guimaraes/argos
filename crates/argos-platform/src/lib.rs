//! The single contract every OS backend implements. Deliberately small and
//! deliberately free of Unix assumptions (no `/dev/sdX` string parsing, no
//! `major:minor` device numbers in the signature) so that `argos-platform-windows`
//! can eventually implement it without the trait itself needing to change.
//!
//! `argos-cli` (and, later, a GUI) programs only against this trait plus
//! `argos-core`'s types -- it never talks to a concrete `argos-platform-*` crate
//! directly except to pick *which* implementation to construct for the current OS.

use argos_core::device::Device;
use argos_core::error::Result;
use std::path::{Path, PathBuf};

pub trait PlatformOps {
    /// Lists every physical disk the backend can see (loop devices, device-mapper
    /// targets, and similar virtual block devices are already excluded -- this is
    /// not a raw `/sys/block` dump). Each [`Device`] already carries the signals
    /// (`bus`, `os_reports_removable`, `is_system_disk`) needed to judge safety;
    /// callers decide what to *offer for writing* via [`Device::is_safe_to_write`]
    /// rather than this method silently hiding disks.
    fn list_removable_disks(&self) -> Result<Vec<Device>>;

    /// Re-resolves a single device by its serial + expected size right before a
    /// destructive operation, to catch anything that changed since it was first
    /// listed (unplugged, replugged, or another drive claimed the same path).
    /// `Ok(None)` means the device is no longer present or no longer matches.
    fn refresh(&self, platform_id: &str, expected_serial: Option<&str>) -> Result<Option<Device>>;

    /// Unmounts every mounted partition on `device` (the whole disk, not a single
    /// partition) so it can be opened exclusively for writing.
    fn unmount(&self, device: &Device) -> Result<()>;

    /// Ejects `device` after a successful write, when the OS supports it.
    fn eject(&self, device: &Device) -> Result<()>;

    /// Resolves which physical device backs `path`, for the source/target
    /// collision preflight check. `Ok(None)` means it could not be determined
    /// (e.g. a network filesystem) -- callers must treat that as "unproven", not
    /// as "safe".
    fn backing_device_of(&self, path: &Path) -> Result<Option<String>>;

    /// Forces the OS to reread `device`'s partition table (backlog #27, W3):
    /// needed right after a privileged process has written a brand new GPT
    /// to it, so the partitions it just created show up as their own block
    /// devices before they can be formatted or mounted.
    fn reread_partition_table(&self, device: &Device) -> Result<()>;

    /// Mounts partition `partition_number` (1-indexed, matching GPT partition
    /// numbers) of `device` as NTFS and returns the mountpoint it was mounted
    /// at (backlog #27, W3). The one relaxation of "no shelling out" the
    /// phase 2 guiding decisions in `docs/architecture.md` call for --
    /// implemented via the external `ntfs-3g` driver, not a kernel `mount(2)`
    /// call, since that's the only formatting/mounting path proven across
    /// the Linux distributions Argos targets.
    fn mount_ntfs_partition(&self, device: &Device, partition_number: u32) -> Result<PathBuf>;

    /// Unmounts a path previously returned by [`mount_ntfs_partition`].
    fn unmount_path(&self, mount_path: &Path) -> Result<()>;
}
