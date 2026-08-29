//! macOS implementation of [`argos_platform::PlatformOps`].
//!
//! **Not implemented yet.** The design (parse `diskutil list -plist` /
//! `diskutil info -plist`, exclude `Internal == true` disks, walk up to the
//! parent whole disk on Apple Silicon so an internal APFS volume can't be
//! mistaken for part of an external disk) is written up in the project backlog,
//! epic E3. This crate exists now so [`argos-cli`] can already depend on the
//! trait and pick a backend by `#[cfg(target_os = ...)]` without a later
//! restructuring; every method below returns
//! [`argos_core::error::ArgosError::NotImplemented`] until E3 lands.

use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use argos_platform::PlatformOps;
use std::path::Path;

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

impl PlatformOps for MacOsPlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        Err(ArgosError::NotImplemented("macOS disk enumeration (E3)"))
    }

    fn refresh(
        &self,
        _platform_id: &str,
        _expected_serial: Option<&str>,
    ) -> Result<Option<Device>> {
        Err(ArgosError::NotImplemented("macOS disk enumeration (E3)"))
    }

    fn unmount(&self, _device: &Device) -> Result<()> {
        Err(ArgosError::NotImplemented("macOS unmount (E3)"))
    }

    fn eject(&self, _device: &Device) -> Result<()> {
        Err(ArgosError::NotImplemented("macOS eject (E3)"))
    }

    fn backing_device_of(&self, _path: &Path) -> Result<Option<String>> {
        Err(ArgosError::NotImplemented(
            "macOS backing-device lookup (E3)",
        ))
    }
}
