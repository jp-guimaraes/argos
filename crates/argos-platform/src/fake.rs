//! An in-memory [`PlatformOps`] for tests. Behind the `test-fixtures`
//! feature; see its doc comment in `Cargo.toml` for the scope limit that
//! comes with it.
//!
//! This is the first time `PlatformOps` is faked in this project. Everything
//! device-related has until now been tested against real loop devices and
//! real `hdiutil` images, deliberately, and that stays true for anything that
//! decides whether a disk is safe. What a fake is good for is the layer
//! *above* that decision: check ordering, error propagation, and how a front
//! end reacts when a device changes underneath it.

use crate::PlatformOps;
use argos_core::device::Device;
use argos_core::error::{ArgosError, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What [`FakePlatform::refreshing_with`] takes. Named rather than written
/// inline so the struct field below stays readable.
type RefreshFn = Box<dyn Fn(&str) -> Result<Option<Device>> + Send + Sync>;

/// A scripted platform backend.
#[derive(Default)]
pub struct FakePlatform {
    devices: Vec<Device>,
    /// What `backing_device_of` answers, for the source/target collision
    /// check. `None` means "could not be determined", which callers must
    /// treat as unproven rather than safe.
    backing_device: Option<String>,
    /// Makes `refresh` answer with something other than what `list` returned
    /// -- the device-replaced and TOCTOU scenarios.
    refresh_override: Option<RefreshFn>,
    calls: Mutex<Calls>,
}

/// What a test asserts happened, rather than only what came back.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Calls {
    pub unmounted: Vec<String>,
    pub ejected: Vec<String>,
    pub backing_device_queries: Vec<PathBuf>,
}

impl FakePlatform {
    pub fn new(devices: Vec<Device>) -> Self {
        FakePlatform {
            devices,
            ..Default::default()
        }
    }

    /// Reports `path` as living on `device_id`, so the source/target
    /// collision check has something to fire on.
    pub fn backed_by(mut self, device_id: impl Into<String>) -> Self {
        self.backing_device = Some(device_id.into());
        self
    }

    /// Replaces what `refresh` returns, independently of the listed devices.
    pub fn refreshing_with(
        mut self,
        f: impl Fn(&str) -> Result<Option<Device>> + Send + Sync + 'static,
    ) -> Self {
        self.refresh_override = Some(Box::new(f));
        self
    }

    pub fn calls(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.calls.lock().expect("fake platform call log poisoned")
    }
}

impl PlatformOps for FakePlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        Ok(self.devices.clone())
    }

    fn refresh(&self, platform_id: &str, _expected_serial: Option<&str>) -> Result<Option<Device>> {
        if let Some(f) = &self.refresh_override {
            return f(platform_id);
        }
        Ok(self
            .devices
            .iter()
            .find(|d| d.platform_id == platform_id)
            .cloned())
    }

    fn unmount(&self, device: &Device) -> Result<()> {
        self.calls().unmounted.push(device.platform_id.clone());
        Ok(())
    }

    fn eject(&self, device: &Device) -> Result<()> {
        self.calls().ejected.push(device.platform_id.clone());
        Ok(())
    }

    fn backing_device_of(&self, path: &Path) -> Result<Option<String>> {
        self.calls().backing_device_queries.push(path.to_path_buf());
        Ok(self.backing_device.clone())
    }
}

/// A platform whose every call fails, for testing error propagation.
pub struct FailingPlatform(pub &'static str);

impl PlatformOps for FailingPlatform {
    fn list_removable_disks(&self) -> Result<Vec<Device>> {
        Err(self.err())
    }
    fn refresh(
        &self,
        _platform_id: &str,
        _expected_serial: Option<&str>,
    ) -> Result<Option<Device>> {
        Err(self.err())
    }
    fn unmount(&self, _device: &Device) -> Result<()> {
        Err(self.err())
    }
    fn eject(&self, _device: &Device) -> Result<()> {
        Err(self.err())
    }
    fn backing_device_of(&self, _path: &Path) -> Result<Option<String>> {
        Err(self.err())
    }
}

impl FailingPlatform {
    fn err(&self) -> ArgosError {
        ArgosError::Io(std::io::Error::other(self.0))
    }
}
