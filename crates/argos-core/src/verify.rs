//! Post-write verification: read back what was actually written and compare it
//! against the hash computed while writing. Catches corruption in flight and
//! the classic "counterfeit flash drive" failure mode (device reports more
//! capacity than it can actually store, so the tail of the image silently
//! doesn't land).

use crate::error::{ArgosError, Result};
use crate::image::checksum::sha256_stream;
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
}
