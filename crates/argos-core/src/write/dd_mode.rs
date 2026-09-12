//! The DD-mode write loop: copies an isohybrid image byte-for-byte onto a
//! device. This is the only write strategy implemented in v1 -- see
//! [`crate::write::WriteStrategy`].
//!
//! Deliberately synchronous, single-threaded I/O: the operation is a `read`/
//! `write` loop over large blocks, and async brings no real benefit here while
//! adding complexity to the one code path that must stay easy to audit.
//! Parallel writers would be no better: the kernel already merges this loop's
//! writes into far fewer, larger device requests (a real 5.8GB write showed
//! ~172k writes merged into ~1k device I/Os, with the device busy 100% of the
//! time), and splitting the stream across threads would scatter what flash
//! media most wants to receive sequentially.

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
/// `committed` is what makes the reported progress mean something. A `write()`
/// only queues bytes in the page cache, so counting them reaches 100% while a
/// slow USB stick is still minutes from having the data -- the bar then sits
/// at "done" through a flush it never mentioned. `committed` instead answers
/// "how many bytes has the OS actually written to the device so far", and is
/// what gets reported when it answers at all; `None` (a platform with no such
/// counter) falls back to counting bytes handed over, the way this always did.
///
/// What this deliberately does *not* do is make the answer true by force, with
/// an `fsync` every N bytes: measured on real hardware, that dropped a write
/// the kernel otherwise kept saturated to roughly 1 MiB/s, because each
/// barrier drains the queue and flushes the device's internal cache. Watching
/// a counter costs nothing and slows nothing down.
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
    committed: &impl Fn() -> Option<u64>,
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
        // Clamped: the counter is the whole device's, so anything else that
        // touched it would otherwise be able to report progress this write
        // hasn't actually made.
        progress.on_progress(committed().unwrap_or(written).min(written), total_size);
    }

    dest.flush()?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NoopProgress;
    use std::io::Cursor;

    /// Stands in for a platform with no write counter: progress falls back to
    /// bytes handed to the OS.
    fn no_counter() -> impl Fn() -> Option<u64> {
        || None
    }

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
            &no_counter(),
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
            &no_counter(),
        )
        .unwrap();
        let seen = log.into_inner().unwrap();

        assert_eq!(seen.len(), 3); // two full blocks + the remainder
        assert_eq!(seen.last().unwrap().0, data.len() as u64);
        assert!(seen.iter().all(|(_, total)| *total == data.len() as u64));
    }

    /// The regression this guards: the bar must show what the device has
    /// actually taken, not what the page cache swallowed. With a counter
    /// lagging behind the write loop, every report follows the counter.
    #[test]
    fn progress_follows_the_device_counter_when_there_is_one() {
        let data = vec![0xCDu8; BLOCK_SIZE * 3];
        let mut dest = Vec::new();

        struct Recorder<'a>(&'a std::sync::Mutex<Vec<u64>>);
        impl ProgressSink for Recorder<'_> {
            fn on_progress(&self, bytes_done: u64, _bytes_total: u64) {
                self.0.lock().unwrap().push(bytes_done);
            }
        }
        let log = std::sync::Mutex::new(Vec::new());
        // One block behind the loop, the way a real device is: nothing has
        // reached it while the first block is still in flight.
        let block = BLOCK_SIZE as u64;
        let calls = std::sync::atomic::AtomicU64::new(0);
        let committed = || {
            let nth = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(nth * block)
        };

        write_stream(
            Cursor::new(data.clone()),
            &mut dest,
            data.len() as u64,
            &Recorder(&log),
            &CancelToken::new(),
            &committed,
        )
        .unwrap();

        let seen = log.into_inner().unwrap();
        assert_eq!(seen, vec![0, block, 2 * block]);
    }

    /// A counter that runs ahead of this write (another writer touching the
    /// same device) must not be able to report progress the write hasn't
    /// made.
    #[test]
    fn progress_never_exceeds_what_was_actually_written() {
        let data = vec![0u8; BLOCK_SIZE];
        let mut dest = Vec::new();

        struct Recorder<'a>(&'a std::sync::Mutex<Vec<u64>>);
        impl ProgressSink for Recorder<'_> {
            fn on_progress(&self, bytes_done: u64, _bytes_total: u64) {
                self.0.lock().unwrap().push(bytes_done);
            }
        }
        let log = std::sync::Mutex::new(Vec::new());
        let runaway = || Some(u64::MAX);

        write_stream(
            Cursor::new(data.clone()),
            &mut dest,
            data.len() as u64,
            &Recorder(&log),
            &CancelToken::new(),
            &runaway,
        )
        .unwrap();

        assert_eq!(log.into_inner().unwrap(), vec![BLOCK_SIZE as u64]);
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
            &no_counter(),
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
            &no_counter(),
        )
        .unwrap_err();

        assert!(matches!(err, ArgosError::Cancelled));
        assert_eq!(dest.len(), BLOCK_SIZE); // exactly the first block landed
    }
}
