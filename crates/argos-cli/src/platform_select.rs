//! Picks the `PlatformOps` implementation for the OS Argos is actually running
//! on. This is the *only* place in the CLI that knows which concrete
//! `argos-platform-*` crate exists -- every command past this point programs
//! against the trait, so a future GUI (or a Windows CLI, once phase 2 lands)
//! reuses `commands::*` unchanged.

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
    compile_error!("argos-cli v1 only supports Linux and macOS hosts");
}
