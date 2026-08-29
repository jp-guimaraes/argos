//! macOS implementation of [`argos_platform::PlatformOps`] (backlog epic E3).
//!
//! Enumerates disks via `diskutil list -plist` / `diskutil info -plist`
//! (parsed in [`diskutil`]), unmounts/ejects via `diskutil unmountDisk`/
//! `diskutil eject`, and resolves a path's backing device via `df` --
//! `argos-core`'s "no shelling out except unmount/eject helpers" rule names
//! `diskutil` as the one exception on this platform, since it's also the
//! only supported way to read disk topology without linking against a
//! private/IOKit-adjacent API. See `enumerate.rs` for how the pieces fit
//! together, and its module doc for why an internal disk is still returned
//! from [`MacOsPlatform::list_removable_disks`] (marked unsafe to write via
//! its own signals) rather than filtered out entirely.

mod diskutil;
mod enumerate;

pub use enumerate::MacOsPlatform;
