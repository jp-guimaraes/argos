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
use crate::partition::WindowsPartitionPlan;
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

/// The `check_capacity` equivalent for a Windows installer write (W2+): a
/// two-partition GPT layout needs more room than the raw ISO byte count --
/// the UEFI:NTFS boot partition, the NTFS partition's overhead margin, and
/// the GPT structures themselves all add up on top of it. Compares the
/// device against [`WindowsPartitionPlan::total_bytes_required`] instead of
/// the ISO's own size.
pub fn check_windows_capacity(
    device_label: &str,
    device_size_bytes: u64,
    image_path: &Path,
    plan: &WindowsPartitionPlan,
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

/// How much of *currently available* memory a single file is allowed to
/// occupy before [`check_windows_memory`] refuses the write. Not 1.0: the
/// file's own bytes aren't the only thing sharing that memory during the
/// copy (the destination file's write-back cache, the hasher, everything
/// else already running on the machine), and "available" itself is already
/// a moving target that can shrink between this check and the copy actually
/// reaching that file, minutes later. `0.5` is a deliberately conservative
/// margin, not a measured bound -- there's no way to know exactly how much
/// headroom is enough without knowing what else the machine is doing.
const MAX_FILE_TO_AVAILABLE_MEMORY_RATIO: f64 = 0.5;

/// Backlog #35 (discovered testing #27's W6 against real hardware): the
/// Windows write path's `image::windows::WindowsIso::open_file` has no
/// streaming reader for UDF-backed media -- real Windows installer ISOs are
/// always UDF (see `image::windows`'s top doc comment), and `hadris_udf`
/// only exposes a whole-file-into-memory read. A single-file guard, not a
/// total-bytes one: `windows::copy_files` holds one file in memory at a
/// time, not the whole ISO, so `largest_file_bytes` is what actually risks
/// exhausting `available_memory_bytes` -- comfortably fitting the sum of
/// every *other* file changes nothing about whether the one multi-GB
/// `install.wim`/`install.esd` fits.
///
/// This is exactly the guard that was missing when this shipped without it:
/// a real Windows 10 ISO's `install.wim` OOM-killed not just `argos-helper`
/// but the whole cgroup it happened to share with an unrelated process on a
/// memory-constrained machine, rather than failing cleanly beforehand.
pub fn check_windows_memory(
    image_path: &Path,
    largest_file_bytes: u64,
    available_memory_bytes: u64,
) -> Result<()> {
    let allowed = (available_memory_bytes as f64 * MAX_FILE_TO_AVAILABLE_MEMORY_RATIO) as u64;
    if largest_file_bytes > allowed {
        return Err(ArgosError::WindowsFileTooLargeForAvailableMemory {
            image_path: image_path.to_path_buf(),
            file_size_bytes: largest_file_bytes,
            available_memory_bytes,
        });
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

    #[test]
    fn accepts_a_device_exactly_as_large_as_the_windows_plan_requires() {
        let plan = WindowsPartitionPlan::new(1_474_560, 4_000_000_000);
        let required = plan.total_bytes_required();
        assert!(
            check_windows_capacity("/dev/sdz", required, Path::new("Win11.iso"), &plan).is_ok()
        );
    }

    #[test]
    fn rejects_a_device_smaller_than_the_windows_plan_requires() {
        let plan = WindowsPartitionPlan::new(1_474_560, 4_000_000_000);
        let required = plan.total_bytes_required();
        let err = check_windows_capacity("/dev/sdz", required - 1, Path::new("Win11.iso"), &plan)
            .unwrap_err();
        assert!(
            matches!(err, ArgosError::DeviceTooSmall(_, _, actual, needed) if actual == required - 1 && needed == required)
        );
    }

    #[test]
    fn accepts_a_largest_file_comfortably_under_the_memory_margin() {
        // 1 GiB file, 8 GiB available -- well under the 50% ratio.
        assert!(check_windows_memory(
            Path::new("Win11.iso"),
            1024 * 1024 * 1024,
            8 * 1024 * 1024 * 1024,
        )
        .is_ok());
    }

    #[test]
    fn rejects_a_largest_file_that_would_exceed_the_memory_margin() {
        // The real scenario this guards against: a ~5 GiB install.wim on a
        // machine with well under 10 GiB of available memory.
        let available = 7 * 1024 * 1024 * 1024;
        let largest_file = 5 * 1024 * 1024 * 1024;
        let err =
            check_windows_memory(Path::new("Win10.iso"), largest_file, available).unwrap_err();
        assert!(matches!(
            err,
            ArgosError::WindowsFileTooLargeForAvailableMemory {
                file_size_bytes,
                available_memory_bytes,
                ..
            } if file_size_bytes == largest_file && available_memory_bytes == available
        ));
    }

    #[test]
    fn accepts_a_largest_file_exactly_at_the_memory_margin() {
        let available = 8_000_000_000u64;
        let largest_file = available / 2; // exactly the 50% ratio
        assert!(check_windows_memory(Path::new("Win11.iso"), largest_file, available).is_ok());
    }
}
