//! Post-write verification: read back what was actually written and compare it
//! against the hash computed while writing. Catches corruption in flight and
//! the classic "counterfeit flash drive" failure mode (device reports more
//! capacity than it can actually store, so the tail of the image silently
//! doesn't land).

use crate::error::{ArgosError, Result};
use crate::image::checksum::sha256_stream;
use crate::partition::windows::{PartitionRegion, MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID};
use crate::progress::{Phase, ProgressSink};
use std::io::Read;

/// Reads `bytes_to_check` bytes from `written`, hashes them, and compares
/// against `expected_hash` (the hash returned by [`crate::write::dd_mode::write_stream`]).
/// Streams the comparison rather than loading the image into memory, so this
/// is safe to run against a multi-GB device.
pub fn verify_written_image<R: Read>(
    written: R,
    bytes_to_check: u64,
    expected_hash: &str,
    progress: &dyn ProgressSink,
) -> Result<()> {
    progress.on_phase(Phase::Verifying);

    let limited = written.take(bytes_to_check);
    let actual_hash = sha256_stream(limited, |done| progress.on_progress(done, bytes_to_check))?;

    if actual_hash != expected_hash {
        return Err(ArgosError::ChecksumMismatch {
            expected: expected_hash.to_string(),
            actual: actual_hash,
        });
    }
    Ok(())
}

/// One partition entry as actually read off a real GPT (backlog #27, W4) --
/// plain data, not a `gptman` type, so this comparison stays testable
/// without that dependency: only `argos-privileged` (which does the actual
/// reading) links `gptman`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedPartition {
    pub partition_type_guid: [u8; 16],
    pub region: PartitionRegion,
}

/// Confirms a real GPT (already read off the device) matches what a
/// [`crate::partition::windows::WindowsFat32Plan`]-driven write
/// (`argos_privileged::windows_fat32`, phase 3 M3, backlog #43) should have
/// produced: right partition type GUID, the partition starting at exactly
/// its planned offset with at least its planned size. Size uses `>=` rather
/// than `==` since the write always writes the exact planned size today, but
/// a future "extend the partition to fill the rest of the device" change
/// shouldn't need this check to change too.
///
/// This is the "not `verify_written_image`, which assumes a whole-device
/// hash" strategy the Windows write path needs -- a partition layout has no
/// single meaningful whole-device hash to compare against.
pub fn verify_windows_fat32_layout(
    plan: &crate::partition::windows::WindowsFat32Plan,
    windows: ObservedPartition,
) -> Result<()> {
    check_partition(
        "windows",
        MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID,
        "a Microsoft Basic Data Partition",
        &plan.windows_partition,
        &windows,
    )
}

fn check_partition(
    label: &str,
    expected_type_guid: [u8; 16],
    expected_type_name: &str,
    expected: &PartitionRegion,
    observed: &ObservedPartition,
) -> Result<()> {
    if observed.partition_type_guid != expected_type_guid {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "{label} partition is not {expected_type_name}"
        )));
    }
    if observed.region.start_offset_bytes != expected.start_offset_bytes {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "{label} partition starts at {} bytes, expected {}",
            observed.region.start_offset_bytes, expected.start_offset_bytes
        )));
    }
    if observed.region.size_bytes < expected.size_bytes {
        return Err(ArgosError::WindowsPartitionLayoutMismatch(format!(
            "{label} partition is {} bytes, expected at least {}",
            observed.region.size_bytes, expected.size_bytes
        )));
    }
    Ok(())
}

/// Compares one file's expected hash (from a fresh read of the source ISO)
/// against its actual hash (from a fresh read of what's on the written
/// Windows partition) -- the per-file half of the Windows write path's
/// verification strategy, run once per file
/// `image::windows::WindowsIso::list_files` reported.
pub fn verify_windows_file_hash(path: &str, expected: &str, actual: &str) -> Result<()> {
    if expected != actual {
        return Err(ArgosError::WindowsFileMismatch {
            path: path.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoopProgress;
    use std::io::Cursor;

    fn hash_of(data: &[u8]) -> String {
        sha256_stream(Cursor::new(data.to_vec()), |_| {}).unwrap()
    }

    #[test]
    fn accepts_matching_data() {
        let data = b"ubuntu.iso contents".to_vec();
        let hash = hash_of(&data);
        assert!(verify_written_image(
            Cursor::new(data.clone()),
            data.len() as u64,
            &hash,
            &NoopProgress
        )
        .is_ok());
    }

    #[test]
    fn rejects_corrupted_data() {
        let original = b"ubuntu.iso contents".to_vec();
        let hash = hash_of(&original);
        let mut corrupted = original.clone();
        corrupted[0] ^= 0xFF;

        let err = verify_written_image(
            Cursor::new(corrupted),
            original.len() as u64,
            &hash,
            &NoopProgress,
        )
        .unwrap_err();
        assert!(matches!(err, ArgosError::ChecksumMismatch { .. }));
    }

    #[test]
    fn rejects_a_device_that_is_shorter_than_the_expected_image() {
        // Simulates a counterfeit flash drive: it reports enough capacity but
        // silently drops the tail of what was written.
        let original = vec![0x42u8; 10_000];
        let hash = hash_of(&original);
        let truncated = original[..5_000].to_vec();

        let err = verify_written_image(
            Cursor::new(truncated),
            original.len() as u64,
            &hash,
            &NoopProgress,
        )
        .unwrap_err();
        assert!(matches!(err, ArgosError::ChecksumMismatch { .. }));
    }

    fn matching_fat32_plan_and_observed() -> (
        crate::partition::windows::WindowsFat32Plan,
        ObservedPartition,
    ) {
        let plan = crate::partition::windows::WindowsFat32Plan::new(4_000_000_000);
        let windows = ObservedPartition {
            partition_type_guid: MICROSOFT_BASIC_DATA_PARTITION_TYPE_GUID,
            region: plan.windows_partition,
        };
        (plan, windows)
    }

    #[test]
    fn accepts_a_fat32_layout_matching_the_plan_exactly() {
        let (plan, windows) = matching_fat32_plan_and_observed();
        assert!(verify_windows_fat32_layout(&plan, windows).is_ok());
    }

    #[test]
    fn accepts_a_fat32_partition_larger_than_planned() {
        let (plan, mut windows) = matching_fat32_plan_and_observed();
        windows.region.size_bytes += 4096;
        assert!(verify_windows_fat32_layout(&plan, windows).is_ok());
    }

    #[test]
    fn rejects_a_fat32_partition_with_the_wrong_type_guid() {
        let (plan, mut windows) = matching_fat32_plan_and_observed();
        windows.partition_type_guid = [0u8; 16];
        let err = verify_windows_fat32_layout(&plan, windows).unwrap_err();
        assert!(matches!(err, ArgosError::WindowsPartitionLayoutMismatch(_)));
    }

    #[test]
    fn rejects_a_fat32_partition_that_starts_in_the_wrong_place() {
        let (plan, mut windows) = matching_fat32_plan_and_observed();
        windows.region.start_offset_bytes += 1024;
        let err = verify_windows_fat32_layout(&plan, windows).unwrap_err();
        assert!(matches!(err, ArgosError::WindowsPartitionLayoutMismatch(_)));
    }

    #[test]
    fn rejects_a_fat32_partition_smaller_than_planned() {
        let (plan, mut windows) = matching_fat32_plan_and_observed();
        windows.region.size_bytes -= 1;
        let err = verify_windows_fat32_layout(&plan, windows).unwrap_err();
        assert!(matches!(err, ArgosError::WindowsPartitionLayoutMismatch(_)));
    }

    #[test]
    fn verify_windows_file_hash_accepts_matching_hashes() {
        assert!(verify_windows_file_hash("sources/boot.wim", "abc", "abc").is_ok());
    }

    #[test]
    fn verify_windows_file_hash_rejects_mismatched_hashes() {
        let err = verify_windows_file_hash("sources/boot.wim", "abc", "def").unwrap_err();
        assert!(matches!(
            err,
            ArgosError::WindowsFileMismatch { path, .. } if path == "sources/boot.wim"
        ));
    }
}
