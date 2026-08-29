//! The DD-mode write loop: copies an isohybrid image byte-for-byte onto a
//! device. This is the only write strategy implemented in v1 -- see
//! [`crate::write::WriteStrategy`].
//!
//! Deliberately synchronous, single-threaded I/O: the operation is a `read`/
//! `write` loop over large blocks, and async brings no real benefit here while
//! adding complexity to the one code path that must stay easy to audit.

use crate::error::{ArgosError, Result};
use crate::progress::{CancelToken, Phase, ProgressSink};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

/// Block size used both for the copy loop and for how often the cancel token is
/// checked. 4 MiB balances syscall overhead against cancellation latency.
pub const BLOCK_SIZE: usize = 4 * 1024 * 1024;

/// Copies every byte `source` yields to `dest`, in `BLOCK_SIZE` chunks,
/// reporting progress via `progress` and checking `cancel` after every chunk.
///
/// Returns the SHA-256 of the bytes written, computed incrementally (no need
/// to re-read the source afterwards to know what was sent). `total_size` is
/// used for progress reporting only -- the loop still stops correctly at EOF
/// if `source` turns out to be shorter, and keeps writing past `total_size` if
/// it turns out to be longer (unusual for a well-formed ISO, but the write
/// itself is not the place to second-guess that).
pub fn write_stream<R: Read, W: Write>(
    mut source: R,
    mut dest: W,
    total_size: u64,
    progress: &dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<String> {
    progress.on_phase(Phase::Writing);

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; BLOCK_SIZE];
    let mut written: u64 = 0;

    loop {
        if cancel.is_cancelled() {
            return Err(ArgosError::Cancelled);
        }

        let n = source.read(&mut buf)?;
        if n == 0 {
            break;
        }

        dest.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        written += n as u64;
        progress.on_progress(written, total_size);
    }

    dest.flush()?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoopProgress;
    use std::io::Cursor;

    #[test]
    fn copies_every_byte_and_returns_matching_hash() {
        let data = b"the quick brown fox jumps over the lazy dog".to_vec();
        let mut dest = Vec::new();
        let hash = write_stream(
            Cursor::new(data.clone()),
            &mut dest,
            data.len() as u64,
            &NoopProgress,
            &CancelToken::new(),
        )
        .unwrap();

        assert_eq!(dest, data);
        let expected = {
            let mut h = Sha256::new();
            h.update(&data);
            format!("{:x}", h.finalize())
        };
        assert_eq!(hash, expected);
    }

    #[test]
    fn reports_progress_incrementally_across_multiple_blocks() {
        let data = vec![0xABu8; BLOCK_SIZE * 2 + 123];
        let mut dest = Vec::new();
        let mut seen = Vec::new();

        struct Recorder<'a>(&'a std::sync::Mutex<Vec<(u64, u64)>>);
        impl ProgressSink for Recorder<'_> {
            fn on_progress(&self, bytes_done: u64, bytes_total: u64) {
                self.0.lock().unwrap().push((bytes_done, bytes_total));
            }
        }
        let log = std::sync::Mutex::new(Vec::new());
        write_stream(
            Cursor::new(data.clone()),
            &mut dest,
            data.len() as u64,
            &Recorder(&log),
            &CancelToken::new(),
        )
        .unwrap();
        seen.extend(log.into_inner().unwrap());

        assert_eq!(seen.len(), 3); // two full blocks + the remainder
        assert_eq!(seen.last().unwrap().0, data.len() as u64);
        assert!(seen.iter().all(|(_, total)| *total == data.len() as u64));
    }

    #[test]
    fn stops_immediately_when_cancelled_before_starting() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = write_stream(
            Cursor::new(vec![0u8; BLOCK_SIZE]),
            Vec::new(),
            BLOCK_SIZE as u64,
            &NoopProgress,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, ArgosError::Cancelled));
    }

    #[test]
    fn stops_mid_copy_when_cancelled_after_first_block() {
        struct CancelAfterFirstBlock {
            token: CancelToken,
            blocks_seen: std::sync::atomic::AtomicUsize,
        }
        impl ProgressSink for CancelAfterFirstBlock {
            fn on_progress(&self, _bytes_done: u64, _bytes_total: u64) {
                if self
                    .blocks_seen
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    self.token.cancel();
                }
            }
        }

        let cancel = CancelToken::new();
        let sink = CancelAfterFirstBlock {
            token: cancel.clone(),
            blocks_seen: std::sync::atomic::AtomicUsize::new(0),
        };
        let data = vec![0u8; BLOCK_SIZE * 3];
        let mut dest = Vec::new();
        let err = write_stream(
            Cursor::new(data.clone()),
            &mut dest,
            data.len() as u64,
            &sink,
            &cancel,
        )
        .unwrap_err();

        assert!(matches!(err, ArgosError::Cancelled));
        assert_eq!(dest.len(), BLOCK_SIZE); // exactly the first block landed
    }
}
