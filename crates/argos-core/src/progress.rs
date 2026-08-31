//! Progress reporting and cooperative cancellation for long-running operations
//! (writing an image, verifying a device). Kept as a trait + atomic flag so the
//! CLI can render an `indicatif` bar today and a future GUI can drive its own
//! widgets tomorrow without touching `argos-core`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Unmounting,
    Checksumming,
    Writing,
    Verifying,
    /// Creating the GPT for a Windows installer write (backlog #27, W3).
    Partitioning,
    /// Formatting the NTFS partition (`mkfs.ntfs`) for a Windows installer
    /// write (backlog #27, W3).
    FormattingNtfs,
    /// Mounting the freshly-formatted NTFS partition (`ntfs-3g`) for a
    /// Windows installer write (backlog #27, W3).
    Mounting,
    /// Copying the extracted Windows installer files onto the mounted NTFS
    /// partition (backlog #27, W3).
    CopyingFiles,
}

/// Implemented by whatever wants to observe an operation's progress. `argos-core`
/// never assumes a particular UI; it only calls this trait.
pub trait ProgressSink: Send + Sync {
    fn on_phase(&self, phase: Phase) {
        let _ = phase;
    }
    fn on_progress(&self, bytes_done: u64, bytes_total: u64) {
        let _ = (bytes_done, bytes_total);
    }
}

/// A `ProgressSink` that does nothing, for callers (mostly tests) that don't care.
pub struct NoopProgress;
impl ProgressSink for NoopProgress {}

/// A cooperative cancellation flag. Long-running loops in `argos-core` must check
/// this every block (every 1-4 MiB), never less often. There is deliberately no
/// "undo": once bytes have been written, a cancel only stops writing *further*
/// bytes -- the caller must treat the device as inconsistent afterwards.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_is_not_cancelled() {
        assert!(!CancelToken::new().is_cancelled());
    }

    #[test]
    fn cancel_is_visible_through_clones() {
        let token = CancelToken::new();
        let clone = token.clone();
        clone.cancel();
        assert!(token.is_cancelled());
    }
}
