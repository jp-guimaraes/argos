//! Deliberately empty Windows backend.
//!
//! This crate is **not** part of the v1 scope (host support is Linux + macOS
//! only -- see the backlog's "fora de escopo do v1" section). It exists purely
//! as a compile-time check: if `argos-platform::PlatformOps` could not be
//! implemented here without secretly assuming a `/dev/sdX`-shaped identifier or
//! a POSIX-only concept, the trait would be wrong for a future phase-2 Windows
//! backend. Every method is unreachable by construction (no public constructor),
//! so there is nothing here for CI to accidentally exercise.

use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use argos_platform::PlatformOps;
use std::path::{Path, PathBuf};

pub struct WindowsPlatform(());

impl PlatformOps for WindowsPlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn refresh(
        &self,
        _platform_id: &str,
        _expected_serial: Option<&str>,
    ) -> Result<Option<Device>> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn unmount(&self, _device: &Device) -> Result<()> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn eject(&self, _device: &Device) -> Result<()> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn backing_device_of(&self, _path: &Path) -> Result<Option<String>> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn reread_partition_table(&self, _device: &Device) -> Result<()> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn mount_ntfs_partition(&self, _device: &Device, _partition_number: u32) -> Result<PathBuf> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn unmount_path(&self, _mount_path: &Path) -> Result<()> {
        Err(ArgosError::NotImplemented("Windows host support (phase 2)"))
    }

    fn partition_device_path(&self, _device: &Device, _partition_number: u32) -> String {
        unreachable!("no public constructor -- see this module's doc comment")
    }
}
