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
use std::path::Path;

/// A cheap, side-effect-free way to ask the OS how many bytes it has actually
/// written to a device, as opposed to how many the caller has handed it --
/// which the page cache makes useless as a progress signal (a `write()` is
/// absorbed in microseconds while the device is still minutes from having
/// the data).
///
/// Deliberately a *counter*, not a barrier: reading it costs one small
/// pseudo-file read and does not touch the write path, so honest progress
/// costs nothing. Forcing the answer to be true instead -- an `fsync` every
/// N bytes -- was measured at roughly 1 MiB/s on a real USB stick, against a
/// device the kernel otherwise keeps saturated, because each barrier drains
/// the queue and flushes the device's internal cache.
///
/// Boxed as a `Send + Sync` closure rather than a plain trait method so it
/// can be handed to a background thread (the flush phase samples it while
/// `fsync` blocks) without carrying the whole [`PlatformOps`] object, which
/// has no such bounds, along with it.
pub struct WrittenBytes(Box<dyn Fn() -> Option<u64> + Send + Sync>);

impl WrittenBytes {
    pub fn new(probe: impl Fn() -> Option<u64> + Send + Sync + 'static) -> Self {
        Self(Box::new(probe))
    }

    /// Bytes written to the device since the counter started (boot, or when
    /// the device was attached) -- callers care about deltas, not this
    /// absolute value. `None` if the counter stopped being readable, e.g.
    /// the device was unplugged mid-write.
    pub fn read(&self) -> Option<u64> {
        (self.0)()
    }
}

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

    /// Returns a [`WrittenBytes`] counter for `device`, when the platform
    /// exposes one. `None` -- the default, for backends that don't -- means
    /// progress reporting falls back to bytes handed to the OS, which is
    /// what it always was before this existed.
    fn written_bytes(&self, device: &Device) -> Option<WrittenBytes> {
        let _ = device;
        None
    }
}
