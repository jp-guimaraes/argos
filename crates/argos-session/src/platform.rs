//! Picks the `PlatformOps` implementation for the OS Argos is running on.
//!
//! The *only* place in the front-end stack that knows which concrete
//! `argos-platform-*` crate exists -- everything past this point programs
//! against the trait. Moved here from `argos-cli` so the CLI and a GUI share
//! one construction site rather than keeping a copy each.

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
    compile_error!("Argos v1 only supports Linux and macOS hosts");
}

/// The same backend behind a `Box`, for a front end that has to *store* it --
/// a GUI keeps one in its app state across frames and hands it to worker
/// threads, which `impl PlatformOps` cannot express. The trait is
/// object-safe, so this costs a vtable and nothing else.
pub fn boxed_platform() -> Box<dyn PlatformOps + Send + Sync> {
    Box::new(current_platform())
}
