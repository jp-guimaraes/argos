//! Platform-agnostic core of Argos.
//!
//! This crate must never call into `std::fs`/`std::process` for anything disk- or
//! OS-topology-related (opening a raw device, listing disks, invoking `diskutil`,
//! ...). All of that lives in the `argos-platform-*` crates, which implement the
//! [`argos_platform`] traits and hand this crate plain data (`Device`, byte
//! streams, sizes). That split is what makes the logic here testable with plain
//! files and in-memory buffers, no root and no real hardware required, and it's
//! what will let a future GUI reuse every module below unchanged.

pub mod device;
pub mod error;
pub mod image;
pub mod preflight;
pub mod progress;

pub use device::{Bus, Device};
pub use error::{ArgosError, Result};
