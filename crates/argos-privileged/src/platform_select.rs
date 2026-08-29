//! Same pattern as `argos-cli`'s `platform_select`: the one place that knows
//! which concrete `argos-platform-*` crate to use for re-validating a device.

use argos_platform::PlatformOps;

#[cfg(target_os = "linux")]
pub fn current_platform() -> impl PlatformOps {
    argos_platform_linux::LinuxPlatform::new()
}

#[cfg(target_os = "macos")]
pub fn current_platform() -> impl PlatformOps {
    argos_platform_macos::MacOsPlatform::new()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn current_platform() -> impl PlatformOps {
    compile_error!("argos-helper v1 only supports Linux and macOS hosts");
}
