//! Pre-write checks that run in the *unprivileged* process, before the user is
//! even asked to confirm anything. Mirrors the checks balenaEtcher performs in its
//! renderer process before handing work to its privileged sidecar: capacity and
//! source/target collision are exactly the kind of mistake a confirmation prompt
//! alone won't catch (the user can still "type the device path back" correctly
//! while the underlying command would clobber the ISO it's reading from).
//!
//! Resolving *which* physical device backs a given file path is platform-specific,
//! so that resolution happens in the platform crates; this module only compares
//! already-resolved identifiers and sizes, keeping `argos-core` free of OS calls.

use crate::error::{ArgosError, Result};
use std::path::Path;

/// Fails if the target device is smaller than the image that would be written to it.
pub fn check_capacity(
    device_label: &str,
    device_size_bytes: u64,
    image_path: &Path,
    image_size_bytes: u64,
) -> Result<()> {
    if device_size_bytes < image_size_bytes {
        return Err(ArgosError::DeviceTooSmall(
            device_label.to_string(),
            image_path.to_path_buf(),
            device_size_bytes,
            image_size_bytes,
        ));
    }
    Ok(())
}

/// The `check_capacity` equivalent for a Windows installer write (phase 3
/// M3, backlog #43): a GPT-partitioned FAT32 layout needs more room than the
/// raw ISO byte count -- the partition's overhead margin and the GPT
/// structures themselves add up on top of it. Compares the device against
/// [`crate::partition::windows::WindowsFat32Plan::total_bytes_required`]
/// instead of the ISO's own size.
pub fn check_windows_fat32_capacity(
    device_label: &str,
    device_size_bytes: u64,
    image_path: &Path,
    plan: &crate::partition::windows::WindowsFat32Plan,
) -> Result<()> {
    let required_bytes = plan.total_bytes_required();
    if device_size_bytes < required_bytes {
        return Err(ArgosError::DeviceTooSmall(
            device_label.to_string(),
            image_path.to_path_buf(),
            device_size_bytes,
            required_bytes,
        ));
    }
    Ok(())
}

/// Fails if the image file is stored on the very device that would be overwritten.
/// `image_backing_device_id` and `target_device_id` must already be resolved to
/// the same identifier space (e.g. both physical-disk platform ids) by the caller.
pub fn check_no_source_target_collision(
    image_path: &Path,
    image_backing_device_id: &str,
    target_device_id: &str,
) -> Result<()> {
    if image_backing_device_id == target_device_id {
        return Err(ArgosError::SourceTargetCollision(image_path.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn rejects_device_smaller_than_image() {
        let err = check_capacity("/dev/sdz", 1_000, Path::new("image.iso"), 2_000).unwrap_err();
        assert!(matches!(err, ArgosError::DeviceTooSmall(..)));
    }

    #[test]
    fn accepts_device_at_least_as_large_as_image() {
        assert!(check_capacity("/dev/sdz", 2_000, Path::new("image.iso"), 2_000).is_ok());
    }

    #[test]
    fn rejects_iso_stored_on_target_device() {
        let err = check_no_source_target_collision(
            &PathBuf::from("/media/sdz1/ubuntu.iso"),
            "/dev/sdz",
            "/dev/sdz",
        )
        .unwrap_err();
        assert!(matches!(err, ArgosError::SourceTargetCollision(_)));
    }

    #[test]
    fn accepts_iso_stored_elsewhere() {
        assert!(check_no_source_target_collision(
            &PathBuf::from("/home/user/ubuntu.iso"),
            "/dev/nvme0n1",
            "/dev/sdz",
        )
        .is_ok());
    }
}
