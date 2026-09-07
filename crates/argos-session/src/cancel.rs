//! The cancellation handle every front end drives.

use argos_privileged::protocol;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// The held-open write end of whatever channel carries the plan to
/// `argos-helper` -- a `ChildStdin` under `pkexec`/`sudo`, and (from G3) a
/// FIFO under the macOS graphical elevator. Both front ends drive the same
/// object: the CLI's `SIGINT` handler and a GUI's Cancel button.
///
/// Handing this to the caller *before* it blocks on the event stream is the
/// whole reason [`crate::spawn`] and [`crate::Running::stream`] are separate
/// calls. `argos-cli` used to do both in one function, which is why
/// cancellation had to be a signal handler closing over a mutex rather than
/// something a button could call.
#[derive(Clone)]
pub struct Canceller(Arc<Mutex<Option<Box<dyn Write + Send>>>>);

impl Canceller {
    pub(crate) fn new(channel: Box<dyn Write + Send>) -> Self {
        Canceller(Arc::new(Mutex::new(Some(channel))))
    }

    /// A handle attached to nothing, so a front end's state machine can be
    /// unit-tested without a child process.
    pub fn noop() -> Self {
        Canceller(Arc::new(Mutex::new(None)))
    }

    /// Writes [`protocol::CANCEL_SIGNAL`] and drops the channel.
    ///
    /// Best-effort throughout: if the helper has already exited this just
    /// fails with a broken pipe, and there is nothing left to cancel anyway.
    /// Dropping the handle right after closes the channel, which is itself a
    /// second, redundant cancel signal (see `protocol::watch_for_cancel`).
    pub fn cancel(&self) {
        if let Ok(mut guard) = self.0.lock() {
            if let Some(mut channel) = guard.take() {
                let _ = channel.write_all(&[protocol::CANCEL_SIGNAL]);
                let _ = channel.flush();
            }
        }
    }

    /// Closes the channel without asking for a cancellation -- the normal
    /// end-of-stream path. Without this the helper would sit waiting on a
    /// pipe nobody is going to write to.
    pub fn close(&self) {
        if let Ok(mut guard) = self.0.lock() {
            guard.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bytes a front end's Cancel button actually puts on the wire.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<u8>>>);
    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cancel_writes_the_agreed_signal_byte() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.0);
        Canceller::new(Box::new(recorder)).cancel();
        assert_eq!(*seen.lock().unwrap(), vec![protocol::CANCEL_SIGNAL]);
    }

    /// Cancelling twice must not write the byte twice: the second call finds
    /// the channel already taken. A GUI can plausibly deliver two clicks.
    #[test]
    fn cancelling_twice_writes_the_byte_once() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.0);
        let canceller = Canceller::new(Box::new(recorder));
        canceller.cancel();
        canceller.cancel();
        assert_eq!(*seen.lock().unwrap(), vec![protocol::CANCEL_SIGNAL]);
    }

    #[test]
    fn close_drops_the_channel_without_asking_to_cancel() {
        let recorder = Recorder::default();
        let seen = Arc::clone(&recorder.0);
        let canceller = Canceller::new(Box::new(recorder));
        canceller.close();
        canceller.cancel(); // already closed: nothing left to write to
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_noop_canceller_is_safe_to_drive() {
        let canceller = Canceller::noop();
        canceller.cancel();
        canceller.close();
    }
}
